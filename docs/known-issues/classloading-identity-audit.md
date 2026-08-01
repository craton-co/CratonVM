# Class-loader identity and isolation in `classloading/` — audit, fixes, and what remains

**Status: 🟡 PARTIALLY FIXED 2026-08-01.** One isolation break (a class re-homed
to a user-defined loader kept its bootstrap map key) is fixed with two
regression tests; the absent-vs-ambiguous collapse now has a first-class API
(`NameResolution`) and tests, with the remaining `Option`-returning call sites
left to migrate; four items are confirmed correct so a later sweep can skip
them; five remain open with a recipe each.

This is the C2 review's P1 architecture lane where it touches class loading.

## The invariant

A runtime class is identified by **`(binary name, defining loader)`**, never by
name alone. Two consequences drive everything below:

1. Every map, cache and lookup either keys on that pair, or keys on a
   `ClassId` (which already encodes the pair), or is a bug waiting for a second
   loader to show up.
2. A "not found" is three different answers — **absent**, **ambiguous**,
   **failed to load** — and they demand opposite actions from the caller.
   Collapsing *ambiguous* onto *absent* is how a second (then a third) copy of
   a class gets minted, which is the mechanism behind the Spring AOT
   `argument type mismatch` family and the Groovy `$_run_closureN` collapse.

Fail closed: an ambiguous identity answer must be an error or a miss, never a
guess. Two genuinely distinct classes treated as one is type confusion — it
defeats the verifier and produces machine code that reads the wrong object
layout.

## Census

Every map, cache, set and lookup in `classloading/` that can answer a class
identity question. `loader-aware?` means the key distinguishes two classes that
share a name but not a defining loader.

### `ClassManager` state (`classloading/src/class_manager.rs`)

| map / cache / lookup | key | loader-aware? | wrong-hit consequence | action |
| --- | --- | --- | --- | --- |
| `loaded_classes` (:1819) | `(ClassLoaderId, Arc<str>)` | **yes** — the authoritative index | — | ✅ confirmed correct |
| `name_definitions` (:2002) | `Arc<str>` → `{ClassId: refcount}` | by construction — it *counts* distinct definitions per name so ambiguity is detectable in O(1) | — | ✅ confirmed correct; now also backs `classify_loaded_name` |
| `user_loaders` (:1976) | `ClassLoaderId` | yes (a set of loaders, not of names) | — | ✅ confirmed correct; rebuilt on unload |
| `class_bytes_cache` + `_fifo` (:1839, :1846) | `ClassId` | yes (transitively) | — | ✅ confirmed correct |
| `vtable_descriptors` (:1906) | `ClassId` | yes | — | ✅ |
| `skip_bytecode_verification` (:1923) | `ClassId` | yes | — | ✅ |
| `redefine_generations` (:1961) | `ClassId` | yes | — | ✅ |
| `init_states` (:2027) | `ClassId` | yes | — | ✅ |
| `loading_guard` (:1866) | `String` (name only) | **no** | (a) false `ClassCircularityError` when loader B must define its own `X` re-entrantly while loader A's `X` define is in flight; (b) guard poisoning — the inner `remove(name)` clears the outer define's entry, so the outer loses circularity protection | ⚠️ OPEN — see *Open 1* |
| `synthetic_upgrade_absent` (:1891) | `String` (name only) | no, deliberately | absence of a `.class` on the classpath is a classpath-global fact, not a per-loader one; invalidated on every classpath extension | ✅ correct as keyed |
| `cds_class_cache` (:1859) | `String` (name only) | no, deliberately | holds *bytes* from a CDS archive this VM produced, consulted only on the built-in delegation path; bytes carry no identity | ✅ correct as keyed |
| `origin_violations_seen` (:2053) | `String` | n/a — diagnostic dedupe | at worst an under-reported census row | ✅ |
| `get_loaded_class_id` (:2841) | name; probes built-in chain then `user_loaders` | partially — fails closed on user-loader ambiguity, returns `None` | `None` is *also* what "absent" returns; a caller that loads on a miss mints a second copy | ⚠️ half-fixed — see *Fixed 2* / *Open 2* |
| `get_loaded_class_id_for_requester` (:2794) | `(requesting loader, name)` | **yes** — built-ins parent-first and never down; user loaders own-first, then registered ancestors, then built-ins | — | ✅ confirmed correct |
| `find_class_by_name_for_loader` (:7390) | `(requesting loader, name)` | **yes**, via `loaded_class_for_requesting_loader` (:192) | — | ✅ confirmed correct |
| `find_class_by_name` (:7250) | name | **no** — `#[deprecated]`, keeps ~50 external call sites honest | returns a built-in copy in delegation order, then a *unique* user-loader copy; ambiguity → miss | ⚠️ known, deliberately left; migrate call sites (out of lane) |
| `find_unique_class_by_name` (:7466) | name | fails closed on ambiguity | ambiguity is indistinguishable from absence at the call site | ⚠️ see *Fixed 2* |
| `find_class_by_name_in_loader` (:7663) | `(loader, name)` then delegates | yes | — | ✅ |
| `class_defined_by_loader_exact` (:7633) | `(loader, name)` exactly, plus a `ClassStore` linear-scan fallback | yes | the fallback matches on `class.loader_id == loader_id && name`, so it is exact too | ✅ |
| `find_bootstrap_class_by_name` (:7443) | `(Bootstrap, name)` | yes | — | ✅ |
| `resolve_fast_path_class_id` (:3935) | name, built-in chain first, lone-user-loader only as a last resort gated on "no real bytes exist" | yes enough | documented at length in situ | ✅ |
| `synthesize_array_class` (:8002) | `(Bootstrap, "[L…;")` **always** | **no** | JVMS §5.3.3 gives an array class the defining loader of its *element type*. Every array class here is bootstrap-defined, so `[Lp/X;` is one class even when two loaders define distinct `p/X`, and `getComponentType()` re-derives the component *by name* | ⚠️ OPEN — see *Open 3* |
| `create_synthetic_stub` (:7752) / `fabricate_class` (:3197) | `(Bootstrap, name)` always | n/a (stubs are bootstrap by construction) | a stub minted for a name two user loaders already own would outrank both (Bootstrap is probed first) | ⚠️ OPEN — see *Open 2* |
| `upgrade_synthetic_class` (:8207) | re-homes `Class::loader_id`, **did not re-key the map** | was **no** | see *Fixed 1* | ✅ FIXED |
| `unload_user_classes_inner` (:5771) | removes by `ClassId` set | yes | — | ✅ confirmed correct |

### Other modules

| map / lookup | key | loader-aware? | wrong-hit consequence | action |
| --- | --- | --- | --- | --- |
| `loaders.rs` `USER_LOADER_PARENTS` (:89) | `u32` namespace id → parent id | yes | a stale entry could re-point a *recycled* namespace id at a dead loader's ancestor | ✅ confirmed safe — ids are never recycled (see *Confirmed 3*); leaks one `u32→u32` per dead loader (accepted) |
| `loaders.rs` `BUILTIN_LOADER_DELEGATION_CHAIN` (:45) | — | yes | — | ✅ |
| `resolution.rs` `ResolutionKey` (:331) = `(ClassId, cp_index)` | `ClassId` | yes | — | ✅ |
| `resolution.rs` `InvokeCacheKey` (:1371) = `(ClassId, cp_index, bool)` | `ClassId` | yes | — | ✅ |
| `resolution.rs` `LinkResolver` cache (:752) = `(ClassId, name, descriptor)` | `ClassId` | yes | — | ✅ |
| `type_maps.rs` chunk directory (:1191) | `ClassId`, **process-global** | per-VM identity in a process-global table | two VMs in one process share the table; a `ClassId` is only unique *within* a VM | ⚠️ OPEN — see *Open 4* |
| `access_control.rs` `same_runtime_package` (:500) | `(defining loader, package name)` | **yes** | — | ✅ confirmed correct — this is the consumer that made `upgrade_synthetic_class`'s `loader_id` write necessary in the first place |
| `module.rs` `package_to_module` (:191) | package name | no | a split package across loaders maps to one module | ⚠️ low risk — see *Open 5* |
| `class.rs` `is_subclass_of_by_name` / `is_assignable_to_name` | name | **no, by design** | documented in situ: exception `catch_type` matching and JIT `checkcast` deliberately accept a same-named class from another loader | ⚠️ risk accepted, documented at the definitions |
| `class_manager.rs` `BOOTSTRAP_APPENDED_CLASSES` (:16914) | name, **process-global** | no | pins a name to the bootstrap loader process-wide, across VMs | ⚠️ OPEN — see *Open 4* |
| `proxy_gen.rs` `dedup` / `utf8_dedup` (:648, :656) | constant-pool entry | n/a — per-builder CP dedupe, not identity | — | ✅ |

## What was fixed

### Fixed 1 — a class re-homed to a user-defined loader kept its bootstrap map key (HIGH)

`upgrade_synthetic_class` (`classloading/src/class_manager.rs`) replaces a
synthetic stub's methods, fields, constant pool, layout and origin with the real
bytes, and — since the JVMS §5.3 runtime-package-identity fix — also rewrites
`class.loader_id` to the loader that supplied those bytes. It did **not**
rewrite the `loaded_classes` key, which is `(defining loader, name)`.

A synthetic stub is always filed under `(Bootstrap, name)`
(`create_synthetic_stub` and `synthesize_array_class` both `debug_assert` it).
`define_class_with_options` reaches this path deliberately: when a user loader
defines a name that already has a fabricated bootstrap stub, it upgrades the
stub in place rather than minting a `ClassId` that the stub would permanently
shadow. So after a user-loader `defineClass`:

* `ClassStore` said the defining loader was `UserDefined(n)`;
* `loaded_classes` still said `Bootstrap`.

Two defects followed. The mild one: `class_defined_by_loader_exact(name,
user_loader)` missed its O(1) index and only answered through the cold
linear-scan fallback. The real one: `(Bootstrap, name) → id` kept resolving for
bootstrap/extension/application-initiated lookups — `get_loaded_class_id`,
`find_class_by_name_for_loader`, `find_bootstrap_class_by_name`. That is a
built-in loader **delegating down** into a user-defined namespace, the exact
shape every other lookup in the file refuses (see
`built_in_lookup_is_parent_first_and_never_delegates_down`). JDK code and an
unrelated loader shared one `ClassId` for a class only one of them defined, and
a *second* user loader defining the same name was then silently outranked by the
stale built-in alias instead of reading as ambiguous.

The fix re-keys `loaded_classes` in the same transaction as the `loader_id`
write, and joins the new loader to `user_loaders` so the class stays reachable
from the context-free probe.

It is **deliberately narrow**: only a re-home *into* a user-defined loader is
corrected. Built-in → built-in re-homing (the ordinary "stub for a JDK class,
real bytes later found on the application classpath" case) is untouched, because
(a) the built-in chain is walked parent-first at lookup time, so `(Bootstrap,
name)` there is a legitimate initiating-loader record for a name the bootstrap
loader really did initiate, and (b) *adding* the `(Application, name)` defining
key would start making a later `define_class` for that pair report a
duplicate-define `LinkageError` where it currently mints a second copy — arguably
the JVMS-correct outcome, but a behaviour change on the hottest path in the VM
and out of scope here.

Regression tests (both fail before the fix):

* `a_user_loader_upgrading_a_bootstrap_stub_takes_the_map_key_with_it` — asserts
  the map and the `ClassStore` agree on the defining loader, that
  `user_loaders` gained it, and that *no* built-in loader resolves the name any
  more.
* `two_user_loaders_defining_the_same_name_stay_distinct_and_read_as_ambiguous`
  — the natural two-loader shape. Loader A takes ownership of `Foo` by upgrading
  the stub, loader B defines its own `Foo`; the two must stay distinct classes,
  each resolvable from its own loader, and `get_loaded_class_id` must report the
  name as a miss rather than handing back whichever copy the stale bootstrap
  alias named.

### Fixed 2 — absent and ambiguous are no longer the same answer (MEDIUM)

Added `ClassManager::classify_loaded_name(&str) -> NameResolution` with

```rust
pub enum NameResolution {
    Absent,                                // load it — nothing to conflict with
    Unique(ClassId),                       // use it
    Ambiguous { definitions: usize },      // do NOT load; re-ask with an initiating loader
}
```

backed by the existing `name_definitions` index, so it is O(1) and costs nothing
until called. `NameResolution::unique()` reproduces the old `Option` answer for
call sites that genuinely do not care why there is no answer;
`is_ambiguous()` is the predicate a caller that would otherwise *load* must
consult. `unique_visible_definition` is now expressed in terms of it, so the two
cannot drift.

A lone **hidden** class reads as `Absent`, not `Unique`: a hidden class is never
recorded in a loader's name table (JVMS §5.4.3.1) and is not name-resolvable.

Behaviour of every existing `Option`-returning lookup is unchanged — they still
fail closed on ambiguity. What is new is that a caller can now tell the two
apart. Tests: `name_classification_separates_absent_from_ambiguous` (walks
absent → unique → ambiguous → back to unique via unload) and
`name_classification_normalises_the_separator`.

## Confirmed correct — a later sweep can skip these

1. **The defining-loader index is genuinely loader-aware.** `loaded_classes` is
   `hashbrown::HashMap<(ClassLoaderId, Arc<str>), ClassId>` with full name
   equality (`loaded_classes_probe`, `class_manager.rs:138`). The FNV-digest
   shadow map that used to sit in front of it (loader-blind and
   collision-unsafe) is gone. `name_definitions` refcounts *entries* per
   `ClassId`, which is what makes "two loaders, same `ClassId`" (a delegation
   alias) still read as unique while "two loaders, two `ClassId`s" reads as
   ambiguous — `unique_definition_index_matches_linear_scan` pins the O(1) index
   against the executable-spec linear scan.

2. **Parent delegation walks the real chain, in the right order.**
   `loaded_class_for_requesting_loader` (`class_manager.rs:192`): built-ins walk
   `BUILTIN_LOADER_DELEGATION_CHAIN` parent-first and `break` at the requester,
   so a built-in never delegates *down*. A user loader tries its own namespace,
   then its **registered ancestors** (`loaded_class_via_parent_chain`, :158 →
   `loaders::user_loader_ancestors`, `loaders.rs:148`), then the built-in chain.
   The ancestor walk is depth-capped, cycle-guarded (`a_cycle_terminates`) and
   stops at a built-in parent. The null/bootstrap loader is modelled as
   `ClassLoaderId::Bootstrap` throughout, and `builtin_parent_class`
   (`builtin_loaders.rs:91`) returns `None` for the platform loader — i.e. its
   parent *is* the null bootstrap loader, matching HotSpot.

3. **A loader namespace id can never be recycled, so a stale
   `USER_LOADER_PARENTS` entry can never be re-pointed at a new loader.** Every
   assignment site funnels through `NativeContext::allocate_loader_id`
   (`native-builtins/src/classloader.rs:831`, `:934`, `:1036`, and the
   store-allocating path at `:1535` / `:1541`), whose only real implementation
   (`vm/src/vm/vm_exec.rs:6826`) is a **process-global monotonic
   `AtomicU32`** seeded at `ClassLoaderId::NATIVE_FIRST_USER_DEFINED`. Ids are
   therefore unique across every VM in the process and are never reused. This is
   the same defect shape as the recycled GC region this branch already fixed, and
   it does **not** apply here. The residue is an unbounded but tiny leak: one
   `u32 → u32` entry per dead loader, never pruned. Accepted.
   *Caveat, cross-lane:* the object-keyed side table that maps a `ClassLoader`
   object to its id (`loader_namespace_id_store`,
   `native-builtins/src/classloader.rs:1455`) must keep being pruned/remapped by
   `gc_reconcile_defining_loaders`; if an entry survived its loader and the
   address were reused, a *new* loader would inherit a dead one's id **and** its
   parent link. That store's own doc comment records exactly this bug in its
   previous, identity-hash-keyed form.

4. **Class unloading detaches every `ClassId`-keyed table.**
   `unload_user_classes_inner` (`class_manager.rs:5771`) clears
   `loaded_classes` (by id set), rebuilds `name_definitions`, and retains
   `vtable_descriptors`, `skip_bytecode_verification`, `redefine_generations`,
   `init_states`, `class_bytes_cache` (+ its FIFO) and `user_loaders`; it bumps
   the class-definition epoch and the metadata realm generation, and fires the
   resolution-invalidate hook per class. `ClassStore::remove`
   (`class.rs:1311`) leaves a **monotonic tombstone** — `next_id` is
   `classes.len()` (`class.rs:1124`), so a `ClassId` slot is never reused and a
   stale id fails closed rather than aliasing a later class
   (`unloaded_slots_are_tombstoned_and_never_reused`).

## Open, with a recipe

### Open 1 — `loading_guard` is keyed by name, not by `(loader, name)` (LOW likelihood, MEDIUM blast)

`class_manager.rs:1866`, inserted at `:4576` / `:8236`, checked at `:4094`.
Two failure modes, both needing a re-entrant define of the *same name* under a
*different* loader:

* a false `ClassCircularityError` ("circular class hierarchy") for a
  legitimate two-loader define — HotSpot's own circularity detection is per
  `(loader, name)`;
* guard poisoning: `remove(name)` in the nested define clears the outer
  define's entry, so the outer loses its protection for the rest of its own
  supertype walk.

Not fixed here because the check at `:4094` happens *before* the byte-finding
that determines the defining loader, so keying the guard would mean restructuring
`load_class`'s entry — and getting that wrong silently disables real circularity
detection. **Recipe:** move the guard insert/check to
`define_class_shared_with_options` (which knows `loader_id`), key it
`(ClassLoaderId, Arc<str>)`, and keep `load_class`'s pre-check as a
name-keyed *fast* path that only escalates to the real check. Regression test:
loader A defines `X extends Y`, application `Y implements X'` where `X'` is a
*different* class with the same name — must link, not raise circularity.

### Open 2 — `fabricate_class` treats "ambiguous" as "absent" and mints a shadowing stub (LOW likelihood, HIGH blast)

`class_manager.rs:3197` opens with `if let Some(id) = self.get_loaded_class_id(name)`.
When two user loaders each own a distinct class under `name`, that returns
`None` (correctly, fail-closed), and the function then fabricates a fresh empty
stub under `(Bootstrap, name)` — which, because the bootstrap loader is probed
*first*, immediately outranks **both** real classes for every subsequent by-name
resolution. Symptom: `NoSuchMethodError` / `<init> … has no Code attribute` on a
class that is demonstrably loaded.

Reachability today is low: `ensure_synthetic_class` is called for JDK and
`cratonvm/synthetic/*` names that user loaders do not define. It is listed
because the shape is exactly the one this lane exists to remove, and because
`classify_loaded_name` now makes the check a one-liner.

**Recipe:** at `:3205`, `match self.classify_loaded_name(name)` — `Unique(id)`
takes the existing early-return, `Absent` falls through to fabrication, and
`Ambiguous` must refuse. Refusing needs an error channel:
`try_ensure_synthetic_class` has one (it returns
`ClassNotFoundException`), `ensure_synthetic_class` does not — it returns a bare
`ClassId` and `.expect`s the non-enforcing path never fails. So the real fix is
to make `ensure_synthetic_class` fallible and migrate its `native-builtins`
callers, which is a cross-crate change.

### Open 3 — array classes are always bootstrap-defined (MEDIUM)

`synthesize_array_class` (`class_manager.rs:8002`) hard-codes
`ClassLoaderId::Bootstrap` for every array class and `debug_assert`s it, with a
comment citing JVMS §5.3.3. The citation is half right: §5.3.3 says an array
class is *created by the JVM rather than a class loader*, but it also says its
**defining loader is the defining loader of its component type** (and only the
bootstrap loader for a primitive component). So `[Lp/X;` is a single class even
when two loaders define distinct `p/X`, and `Class.getComponentType()` re-derives
the component *by name* — the ambiguous lookup this whole document is about.

The interpreter-side half of this was fixed on 2026-07-28
(`resolve_class_loader_aware`, a pre-pass plus a retry so `[LX;` no longer
fabricates a stub for `X`); the class-manager half was not.

Not fixed here because `ClassManager::load_class` takes no loader parameter at
all, so the array name cannot reach a component-loader-aware synthesis path
without a signature change that ripples into `vm/`. **Recipe:**

1. Add `ClassManager::load_array_class_for_loader(name, requesting_loader)`:
   resolve the component through `find_class_by_name_for_loader`, take its
   `loader_id` (Bootstrap for a primitive or an unresolvable component), and
   register the array under `(component_defining_loader, array_name)`.
2. Leave `load_class`'s `[` arm delegating to it with `Bootstrap` as the
   requester, so existing behaviour is byte-identical.
3. Point `vm/src/runtime/interpreter.rs`'s `resolve_class_loader_aware` array
   branch at the new entry point.
4. Test: two user loaders each define `p/X`; `[Lp/X;` resolved through each must
   be two different `ClassId`s whose component resolves back to that loader's
   own `p/X`.

### Open 4 — process-global tables keyed on per-VM identity (MEDIUM, cross-VM)

Two tables in this crate are `static` but hold state that is only unique
*within* one VM:

* `type_maps.rs:1191` — the chunk directory is indexed by `ClassId`, and a
  `ClassId` is meaningless outside the `ClassStore` that allocated it
  (`docs/architecture/per-vm-state.md` §0, Fact 1). Two VMs in one process
  index each other's entries.
* `class_manager.rs:16914` `BOOTSTRAP_APPENDED_CLASSES` — a name-keyed set that
  forces bootstrap delegation for every `.class` entry of a jar appended via
  `Instrumentation.appendToBootstrapClassLoaderSearch`. Process-global, so one
  VM's agent pins those names to the bootstrap loader for every other VM, and
  the set is never pruned.

Both are the pattern `ClassManager::compatibility_mode` (`:2037`) and
`metadata_realm` (`:2069`) were deliberately made fields to avoid. Neither is
fixed here: `type_maps`' directory is a lock-free `AtomicPtr` chunk array whose
readers are `&'static`-returning free functions, and `is_bootstrap_appended_class`
is a free function called from `native-builtins` with no manager in hand. Both
need a VM handle threaded to the reader. Track with the `vm/src/` half in
`docs/known-issues/vm-process-global-state.md`.

### Open 5 — no initiating-loader records and no loader constraints (MEDIUM, architectural)

JVMS §5.3.4 requires the VM to record, for every class `C` a loader `L` has
*asked for*, that `L` is an initiating loader of `C` — not just the loaders that
*defined* something. Those records are what the **loader constraint** checks use
to prevent type confusion when two loaders agree on a name appearing in a method
signature.

Verified by reading: `loaded_classes` holds **defining**-loader keys only. The
only writes are `loaded_classes_insert` from `define_class_with_options`
(`:5317`), `register_class_name` (`:7727`, used by `vm/` for VM-generated
classes, always with `Application`), and the two synthesis paths. Delegation is
instead **simulated at lookup time** by walking
`BUILTIN_LOADER_DELEGATION_CHAIN` plus `USER_LOADER_PARENTS`. That is sound for
the built-in chain (it is static) and for user loaders whose parent link is
registered, and it is cheaper than materialising an alias per initiation. It is
*not* sound for a user loader whose `loadClass` override returns a class it did
not obtain by delegation — JVMS records that as an initiating record, and this
model cannot represent it.

Separately: a repository-wide grep for `loader.constraint` finds **nothing**.
There is no loader-constraint checking anywhere in the workspace. In practice
the crate's loader-faithful supertype linking (`resolve_supertype`, `:4598`)
covers the common case that constraints exist to protect — a subclass linking
its own loader's copy of a supertype — but a signature-level mismatch (loader
`L1` and `L2` both name `p/X` in the same method descriptor, resolving to
different classes) is undetected. **Recipe:** record an initiating alias
`(initiating_loader, name) -> ClassId` alongside the defining insert whenever a
load is *driven through* a specific loader (the `drive_defining_loader_load` /
`ucl_try_define_local_class` paths in `native-builtins`), then add a check at
method resolution that every name shared between the resolving class's loader
and the resolved class's loader maps to the same `ClassId`. Test shape: two
loaders, one shared interface name, a method whose descriptor mentions it.

## How to reproduce / debug this family

| flag | what it shows |
| --- | --- |
| `CRATONVM_DBG_LOADER_CHAIN=1` | one line per supertype resolved (or not) through a user loader's parent chain |
| `CRATONVM_DBG_DUPCLASS[=substr]` (+ `_BT`) | every loader-blind "blind pick" in `find_class_by_name`, with an optional Rust backtrace |
| `CRATONVM_TRACE_UNIMPLEMENTED=1` | every fabricated synthetic stub |
| `CRATONVM_DBG_STUB_BT=<substr>` | Rust backtrace at fabrication — this is what surfaced the `synthesize_array_class` frame in *Open 3* |
| `CRATONVM_DBG_CLASS_RESOURCE=<substr>` | the loader a `Class.getResourceAsStream` was routed through |
| `CRATONVM_LOADER_PARENT_CHAIN=0` | disables the Rust-side user-loader parent chain (bisecting a regression to it) |
| `CRATONVM_LOADER_AWARE_RESOLUTION=0` | disables loader-faithful supertype / verifier-hierarchy resolution |

## Related

* `docs/internal/loader-identity.md` — per-file tally of remaining
  `find_class_by_name` call sites.
* `docs/known-issues/hib-bytecode-enhancement-loader-faithful-linking.md`
* `docs/known-issues/vm-process-global-state.md` — the `vm/src/` half of
  *Open 4*.
