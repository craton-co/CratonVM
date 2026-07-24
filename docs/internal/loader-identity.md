# Loader identity: gate consolidation + loader-aware lookup

Status: gate consolidated, new loader-aware lookup added and wired into one
existing call path (`find_class_by_name_in_loader`). The loader-*blind*
`ClassManager::find_class_by_name` is marked `#[deprecated]` but not removed —
~53 external call sites still use it (tally below) and are follow-up work.

## The (loader, name) identity rule

Per JVMS §5.3, a class's true runtime identity is the pair **(defining
loader, fully-qualified name)**, not the name alone. Two different
loaders — most commonly two isolating/enhancing user-defined loaders (Spring
`@CompileWithForkedClassLoader`, Hibernate bytecode-enhancement loaders,
`GroovyClassLoader$InnerLoader` instances, OSGi/webapp classloaders) — can
each legitimately define their own class named e.g. `com.example.Foo`, and
those are two *different* classes that must never be silently collapsed into
one `ClassId`.

CratonVM's `ClassManager` stores classes in a flat `ClassStore` keyed by
`ClassId`, with a `(ClassLoaderId, Arc<str>) -> ClassId` index
(`loaded_classes`) for name lookup. Because that index is keyed on the pair,
looking a name up *without* a loader — `find_class_by_name(name)` — is
inherently loader-blind: when two distinct user-defined loaders each have an
entry for the same name, the correct answer depends on which loader is
asking, and a context-free function cannot know that. The existing "context
groovy" fix (see `class_manager.rs`'s `find_class_by_name` /
`get_loaded_class_id` doc comments) made that ambiguous case report a miss
(`None`) instead of guessing which loader's copy to return — safe, but still
leaves a correctly-answerable case (the requester's own namespace, or the
built-in delegation chain) indistinguishable from the truly ambiguous one,
because the caller's identity was never passed in.

## Gate consolidation

`CRATONVM_LOADER_AWARE_RESOLUTION` (default **ON**) gates the loader-faithful
`CONSTANT_Class` resolution family (implicit class-constant references
reached from bytecode defined by a user-defined loader resolve through that
loader as JVMS §5.4.3 initiating loader, instead of through the flat global
store). Before this change it was read independently in **three** places:

| Location (before) | Was |
|---|---|
| `classloading/src/class_manager.rs` (private `loader_aware_resolution()`, own `OnceLock`) | had drifted to default **OFF** |
| `vm/src/runtime/env_cache.rs::loader_aware_resolution()` (own `OnceLock`) | default **ON** |
| `native-builtins/src/classloader.rs::loader_aware_resolution()` (own `OnceLock`) | had drifted to default **OFF** |
| `native-builtins/src/classloader.rs::is_loader_aware_resolution_eligible()` (inlined, no cache) | a **fourth**, uncached inline copy in the same file as the third |

The classloading and native-builtins copies had both drifted back to
default-OFF after `env_cache::loader_aware_resolution` flipped to default-ON
for the `context.groovy` bug-cluster fix — each copy's default was supposed
to move in lockstep but nothing enforced that, so the interpreter's half of
loader-faithful resolution was live in production while the class-manager's
supertype/interface-linking half and the native-builtins half (per-user-loader
`defineClass` namespace assignment, exact `findLoadedClass`,
reflective Field/Method/Constructor type resolution, annotation Class-value
resolution) silently stayed on the old behavior by default. See
`docs/internal/fixed-suite-bugs/hib-bytecode-enhancement-loader-faithful-linking-FIXED.md`
for the bug cluster this caused. (The in-code doc comments this change
carries forward still cite the older `docs/known-issues/...` path for that
same file — a pre-existing stale reference, not introduced here.)

**Fix — single source of truth:** `cratonvm_classloading::loader_aware_resolution()`
(defined in `classloading/src/class_manager.rs`, re-exported from
`classloading/src/lib.rs`) is now the only place that parses
`CRATONVM_LOADER_AWARE_RESOLUTION`, cached in its own `OnceLock`. Both other
crates already depend on `cratonvm-classloading` directly (see their
`Cargo.toml`s), so both were changed to thin delegating wrappers that keep
their existing name/signature/visibility (no caller of either has to change):

- `vm/src/runtime/env_cache.rs::loader_aware_resolution()` — body is now
  `cratonvm_classloading::loader_aware_resolution()`; its own `OnceLock` was
  removed (the classloading copy already caches). The doc-comment history
  (Groovy repro, the 2026-07-04 Hibernate app-gauntlet soak numbers) is kept
  in place since it documents the *default* that the shared function now
  implements.
- `native-builtins/src/classloader.rs::loader_aware_resolution()` (still
  `pub(crate)`) — same treatment: body forwards to
  `cratonvm_classloading::loader_aware_resolution()`, own `OnceLock` removed.
- `native-builtins/src/classloader.rs::is_loader_aware_resolution_eligible()` —
  previously inlined its *own* `std::env::var("CRATONVM_LOADER_AWARE_RESOLUTION")`
  parse (a fourth copy, uncached, living a few dozen lines from the crate's
  own `loader_aware_resolution()`). Now calls that in-crate function instead.

Net effect: one env-var parse, one cache, three call sites (four, counting the
inline one that got folded in) all reading the same answer. Default is ON
everywhere, matching the validated Hibernate app-gauntlet soak recorded in
`env_cache.rs`'s doc comment. `CRATONVM_LOADER_AWARE_RESOLUTION=0` still forces
it off everywhere in one shot.

## Loader-aware lookup: `find_class_by_name_for_loader`

Added beside `find_class_by_name` in `classloading/src/class_manager.rs`:

```rust
pub fn find_class_by_name_for_loader(
    &self,
    name: &str,
    requesting_loader: ClassLoaderId,
) -> Option<ClassId>
```

Semantics: checks `requesting_loader`'s own namespace first (classes it has
itself defined), then walks `BUILTIN_LOADER_DELEGATION_CHAIN`
(`Bootstrap → Extension → Application`, from `classloading/src/loaders.rs`)
up to the bootstrap loader. **It never falls through to scanning
`self.user_loaders` for an unrelated user-defined loader's same-named class**
— that flat "any same-named class from any loader" fallback is exactly the
unsoundness described above. If neither the requester's own namespace nor the
built-in chain has the name, the answer is `None`.

### Known limitation: no tracked user-loader parentage

`ClassManager` does not track user-defined-loader parent chains. Per
`loaders.rs`'s doc comment on `BUILTIN_LOADER_DELEGATION_CHAIN`: "User-defined
loaders... have their parent chains modelled on the **Java side** (the
`parent` field of `java.lang.ClassLoader`) — the Rust side never observes a
deep parent walk for those." So when `requesting_loader` is itself a
`ClassLoaderId::UserDefined` loader whose actual Java-level parent is
*another* user-defined loader (not one of the three built-ins),
`find_class_by_name_for_loader` cannot walk that link — it degrades to
"requester's own namespace, then the built-in chain," which is a strict
subset of full JVMS delegation for that specific case. This mirrors the
existing `class_id_by_name_near` / `find_class_by_name_in_loader` pattern
(see below); callers that need the true parent chain for such a loader still
have to drive that loader's `loadClass` directly at the bytecode/interpreter
layer (see `native-builtins/src/lang_class.rs`'s
`native_class_get_declared_classes`, which does exactly this via
`native-builtins::classloader::defining_loader_for` +
`NativeContext::invoke_virtual` when the namespace/chain lookup misses).

### Relationship to the existing loader-aware sibling

`class_id_by_name_near(name, near: ClassId)` (a `NativeContext` trait method,
implemented in `vm/src/vm/vm_exec.rs:5912` as
`find_class_by_name_in_loader(name, loader_of(near))`, consumed by
`native-builtins/src/lang_class.rs:15913` for `Class.getDeclaredClasses()`
nested-class resolution) already existed as a loader-aware entry point before
this change. It stays as-is at the `NativeContext` layer. What changed is its
*implementation*, `ClassManager::find_class_by_name_in_loader`: its own-namespace
probe is unchanged, but its parent-chain fallback used to be the loader-blind
`find_class_by_name` (which — after missing the built-in chain — additionally
scanned every *other* user-defined loader's namespace and returned an
unambiguous same-named match if it found exactly one; a guess). It now
forwards to `find_class_by_name_for_loader`, which stops at the built-in chain
and never makes that guess.

**Behavior change and who it affects:** the only observable difference is in
the case that was unsound before: `requesting_loader` doesn't have the name,
the built-in chain doesn't have it, but some unrelated user-defined loader
does. Old behavior: return that unrelated loader's class. New behavior:
`None`. `class_defined_by_loader_exact` (exact-key, no delegation at all) is
untouched. This ripples to every caller of `find_class_by_name_in_loader` /
`class_id_by_name_near` / the `NativeContext::class_id_by_name_and_loader`
native-builtins entry point (17 files reference the latter name, mostly trait
plumbing and mocks) — none of those files are in this change's scope, so
this is flagged here rather than fixed. The two classloading-crate
integration tests that exercise `find_class_by_name_in_loader`
(`classloading/tests/wp_security_robustness.rs`,
`classloading/tests/wp2_3_define_class_backend.rs`) were checked by hand:
neither depends on the removed guess (one asserts only inside `if let
Some(id) = r`, tolerating the now-`None` result; the other's lookup always
hits the own-namespace probe before the fallback is ever reached).

## Deprecation of `find_class_by_name`

`ClassManager::find_class_by_name(name: &str)` is marked:

```rust
#[deprecated(note = "loader-blind; use find_class_by_name_for_loader")]
```

Checked first: no `deny(warnings)` anywhere in the workspace (`Cargo.toml`'s
`[workspace.lints.rust]` / `[workspace.lints.clippy]`, `clippy.toml`, and a
project-wide grep for `#![deny(warnings)]` / `#![forbid(warnings)]` all came
back clean), so the attribute cannot turn into a build-breaking error under
the orchestrator's centrally-run build — it only adds advisory warnings.

### Remaining call sites (as of this change)

Migrated (within `classloading/src/class_manager.rs`, requesting loader was
in scope):

- `find_class_by_name_in_loader`'s parent-chain fallback → now calls
  `find_class_by_name_for_loader(name, loader_id)` instead of
  `find_class_by_name(name)`.

Left alone deliberately (calls the deprecated fn directly, `#[allow(deprecated)]`
added so the intentional test doesn't just add warning noise):

- `classloading/src/class_manager.rs` test `class_manager_find_before_load_returns_none`
  (no loader in scope to migrate to — the test is about the "nothing loaded
  yet" miss case generically).

**External call sites — out of this change's scope (other files/agents), left
as-is, tallied here for follow-up:**

| File | Count | Notes |
|---|---:|---|
| `vm/src/runtime/interpreter.rs` | 25 | Exception-handler `catch_type` resolution (`catch_class_id`, 3 sites), JIT devirtualization / CHA target resolution, `String`/`Reference` well-known-class lookups, module-boundary checks. Several sites (e.g. the `catch_class_id` ones) already have a `holder_cid`/`class` in local scope whose loader would be the natural `requesting_loader` — good first candidates for follow-up migration. |
| `vm/src/vm/vm_util.rs` | 11 | — |
| `vm/src/vm/vm_exec.rs` | 6 | Includes `class_id_by_name` (the loader-blind half of the `NativeContext` trait) and `class_id_by_name_near`'s own fallback (`cm.find_class_by_name(name)` after the loader-scoped probe misses — same shape as the `find_class_by_name_in_loader` fix above, not migrated here because `vm_exec.rs` is outside this change's file scope). |
| `vm/src/jit/helpers.rs` | 4 | — |
| `vm/src/vm.rs` | 3 | — |
| `vm/src/native/jni.rs` | 1 | JNI `FindClass` — loader-blind by JNI spec in the no-explicit-loader overload; likely fine to stay as-is. |
| `vm/tests/new19_module_access.rs` | 3 | Test file. |

Total external: **53** call sites across 7 files, none touched by this change.

## Files changed by this consolidation

- `classloading/src/class_manager.rs` — gate is the single source of truth
  (now `pub`); `find_class_by_name` deprecated; new
  `find_class_by_name_for_loader` added; `find_class_by_name_in_loader`'s
  fallback migrated to it.
- `classloading/src/lib.rs` — re-exports `loader_aware_resolution` (needed for
  `vm` and `native-builtins` to reach it; not part of the original file
  scope for this change, touched only for this one-line export).
- `vm/src/runtime/env_cache.rs` — `loader_aware_resolution()` delegates to
  `cratonvm_classloading::loader_aware_resolution()`.
- `native-builtins/src/classloader.rs` — `loader_aware_resolution()` delegates
  to `cratonvm_classloading::loader_aware_resolution()`;
  `is_loader_aware_resolution_eligible()` calls the in-crate function instead
  of inlining its own env-var parse.
