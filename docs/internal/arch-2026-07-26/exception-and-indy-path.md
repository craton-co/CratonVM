# Exception throw/catch and `invokedynamic` paths — measurement and fixes

**Date:** 2026-07-26
**Slug:** `exception-and-indy-path`
**Base:** `dev` @ `6495a191c` (merged into the worktree branch before any edit)
**Files owned by this pass:** `vm/src/runtime/exceptions.rs`,
`vm/src/runtime/invokedynamic.rs`, this document.

The architecture review measured CratonVM at 2.3–2.9x HotSpot on arithmetic
kernels but 8–21x on allocation- and framework-shaped workloads, and flagged
these two paths as uncovered. This pass **measured what actually happens today**
before changing anything, because two of the review's premises turned out to be
wrong (see [Already optimized](#already-optimized-review-premises-that-did-not-hold)).

---

## 1. What a throw costs today

Two distinct throw shapes exist, and only one of them goes through
`runtime/exceptions.rs`:

| Shape | Path | Trace captures |
|---|---|---|
| Java bytecode `new FooException(...)` + `athrow` | `native_exc_init_*` constructor shadow → `capture_throwable_trace` | 1 |
| VM-raised (`NullPointerException` from a null receiver, `ClassCastException` from `checkcast`, `IOException` from a native, `NoSuchMethodError` from resolution, …) | `throw_runtime_error` / `throw_linkage_error` / `raise_no_class_def_found*` → `create_exception_object` | **2** (before this pass) |

### 1a. Per-throw fixed overhead: three uncached `env::var_os` syscalls

`throw_runtime_error` is the funnel for *every* VM-raised exception. Before this
pass its preamble executed, unconditionally, on every call:

```rust
if std::env::var_os("CRATONVM_DBG_NPE_NONE").is_some()  { … }
if std::env::var_os("CRATONVM_DBG_AIOOBE").is_some()    { … }
if std::env::var_os("CRATONVM_DBG_BUFUNDER").is_some()  { … }
```

`std::env::var_os` is a `getenv` global-mutex acquisition plus an `OsString`
allocation on Linux, and a `GetEnvironmentVariableW` syscall (~500 ns) plus a
UTF-16→UTF-8 decode on Windows. That is **~1.5 µs of pure syscall per throw on
Windows**, paid before a single useful instruction ran — and it *serialises
threads* on the libc env lock on Linux, so it is worse than 3x the single-thread
cost under concurrency.

`convert_class_not_found` (`CRATONVM_DBG_NCDFE`) and `throw_linkage_error`
(`CRATONVM_DBG_VERIFY_ERROR`) each carried one more on their own entry paths;
`CRATONVM_DBG_NPE_TRACE` and `CRATONVM_DBG_WF_NPE` sat inside the
`tracing::enabled!(DEBUG)` guard, so those two were already cheap in production
but free to fix at the same time.

This is exactly the cost class that `runtime::env_cache` exists to eliminate —
and its module doc even names `runtime::exceptions::iae_trace_enabled` as the
pattern's origin. The throw path had simply never been swept.

**Fixed.** A local `cached_env_flag!` macro (same `OnceLock<bool>` shape as the
pre-existing `iae_trace_enabled` in the same file) now backs all seven flags.
Steady-state cost per flag collapses from a syscall to a relaxed atomic load.
Semantics are unchanged apart from the documented process-lifetime memo: setting
the variable *after* the first throw no longer takes effect — the same contract
every flag in `env_cache` already has.

### 1b. Per-throw variable overhead: the stack trace was captured twice

`create_exception_object_for_class` did this:

1. allocate the throwable,
2. invoke `<init>` — which for the ~50 exception subclasses with registered
   `native_exc_init_*` shadows funnels into `capture_throwable_trace`,
3. then **unconditionally** invoke `fillInStackTrace` — which funnels into
   `capture_throwable_trace` *again*.

The original comment was `"This is done automatically by the Throwable
constructor in most JDK versions, but we call it explicitly just in case."`

One capture is not cheap. Per Java frame, `stackwalker::entry_from_frame`:

* looks the class up in the `ClassStore`,
* calls `find_method(name, descriptor)` on it,
* decodes the `Code` attribute and **linearly scans the `LineNumberTable`**.

The result is collected into a `Vec<StackTraceEntry>`, then
`NativeContextImpl::capture_throwable_stack_trace` **clones the whole vector**
so it can both return it and store it, and `SharedVm::store_throwable_stack_trace`
takes the VM-wide `throwable_stacks` `RwLock` in **write** mode to insert it.

So at a Spring/JUnit-shaped stack depth of 50–150 frames, one VM-raised throw
cost **two O(depth) walks with a per-frame line-table scan, four vector
allocations, and two global write-lock acquisitions** — where one of each
suffices. The second capture's output was byte-identical to the first; it
replaced a correct map entry with an equal one.

**Yes, the global lock is on the throw path** (the review asked): `store_throwable_stack_trace`
takes `threads.throwable_stacks.write()`, and it was taken twice per VM-raised throw.
This pass halves that to once. Removing it entirely would require a lock-free or
sharded store — see [cross-owner requests](#cross-owner-requests).

**Fixed, without trading fidelity.** The explicit `fillInStackTrace` is now
guarded by `trace_already_captured_at_current_depth`, which returns `true` only
when the constructor's capture is **provably identical** to what the explicit
call would produce:

* `capture_throwable_trace` parks `backtrace = this` (the non-null marker
  real-JDK `getOurStackTrace()` needs) and `depth = trace.len()`;
* the captured trace is the *whole* Java stack (`capture_full_trace` maps every
  entry of `thread.frames`), so `depth == thread.frames.len()` **as it stood at
  capture time**;
* therefore `backtrace == this && depth == frames.len()` proves the constructor
  captured with this thread's frame stack in exactly the state `fillInStackTrace`
  would see now — same frames, same `last_instr_pc` each, same
  `StackTraceEntry` vector.

The guard **fails closed** in every uncertain case, which is what keeps the
prior art honest (this repo has a scar where tail-call optimisation silently
dropped caller frames — `tco-breaks-stacktrace-fidelity`):

* a **bytecode** (non-shadowed) `<init>` runs inside a pushed Java frame, so its
  capture records `depth == frames.len() + k`; the comparison mismatches and the
  explicit `fillInStackTrace` still runs, replacing the
  constructor-frame-contaminated trace with the clean one — i.e. the correcting
  behaviour that made the unconditional call worth keeping is preserved exactly;
* a missing or unwritten `backtrace` / `depth` field (opaque `_fN` bootstrap
  layouts), `depth == 0`, or an empty frame stack all return `false`.

A trace can only be *skipped*, never *altered*. Lazy materialisation (record
frame identities, resolve line numbers only on `getStackTrace`/`printStackTrace`)
is the larger remaining win and is written up under
[cross-owner requests](#cross-owner-requests) — the per-frame `LineNumberTable`
scan lives in `stackwalker.rs`, which this pass does not own.

### 1c. The two-layer model is *not* costing an allocation on the catchable path

Review item 4 asked whether the `MethodCallFailed::ExceptionThrown` (Java-catchable)
vs `InternalError` (VM bug) split costs an allocation or a `format!` on the
common catchable path. **It does not.**

* `MethodCallFailed::ExceptionThrown(ObjectRef)` is a bare pointer — no
  allocation, no formatting.
* Every `format!` in `exceptions.rs` sits on an `InternalError` construction
  (`"failed to load exception class {class_name}"`, `"exception class {…} is not
  loaded"`), inside `linkage_throwable` (only reached when a `LinkageError` is
  genuinely being converted), or in the OOM message.
* The `String` messages carried by `RuntimeError` variants are allocated by the
  *callers* (`RuntimeError::ClassCastException { message: format!(…) }` at the
  `checkcast` site, etc.), not by this file. That is a real cost — a
  `ClassCastException` that is caught and discarded still pays a `format!` — but
  the allocation happens in `interpreter.rs` and the native crates, which this
  pass does not own. See [cross-owner requests](#cross-owner-requests).

---

## 2. What an `invokedynamic` costs today

### 2a. Call-site caching is already implemented — for the JDK factories

The review's premise was "if the call-site bootstrap result is not cached per
call site, every lambda evaluation re-runs bootstrap". **Verified: it is cached**,
and has been. `execute_invokedynamic` opens with a `resolution_cache` lookup
(`get_call_site(current_class_id, cp_index)`), and every JDK-factory bootstrap
branch calls `put_call_site` before executing:

| Bootstrap | Cached? |
|---|---|
| `StringConcatFactory.makeConcatWithConstants` | yes |
| `StringConcatFactory.makeConcat` | yes |
| `LambdaMetafactory.metafactory` / `altMetafactory` | yes |
| `SwitchBootstraps.typeSwitch` / `enumSwitch` | yes |
| `ObjectMethods.bootstrap` (records) | yes |
| **anything else (`bootstrap_generic`)** | **no — re-bootstraps every execution** |
| `groovy_cast_to_boolean` | no (but it does no CP work per execution) |

Zero-capture lambdas additionally get a HotSpot-style singleton via
`LAMBDA_SINGLETON_CACHE`, so a non-capturing lambda does not even allocate a
proxy after its first evaluation.

Invalidation is correct too. `ResolutionCache::invalidate_class` is wired to the
JVMTI redefine path (`install_resolution_invalidate_hook` →
`resolution_invalidate_adapter` in `vm_init.rs`) and drops every call site keyed
on the redefined class, so the next execution re-bootstraps against the new
constant pool. Call sites are evicted on **key** match only, and that is sound
because `LambdaCallSite::impl_handle` is a fully **symbolic** `MethodHandle`
(owner / name / descriptor `Arc<str>`s) re-resolved at each dispatch — redefining
the class that *owns the lambda body* is therefore picked up without any cache
eviction. Locked in by
`lambda_impl_handle_is_symbolic_so_impl_redefinition_needs_no_eviction`.

**No "memoized `None` is permanent" bug in the call-site cache.** `get_call_site`
is a plain map lookup returning `Option<&_>`; only successful bootstraps ever
insert, so a miss is always retried. (There *is* a related latent issue on a
different field — see 2d.)

### 2b. The cached string-concat fast path allocated `2 + N` strings per `"a" + b`

This is the real, measurable indy regression, and it is on by far the hottest
cached indy shape: `StringConcatFactory` is every `"a" + b` in every logging
call, `toString()`, and exception message in framework code.

`ResolvedCallSite::StringConcat` stores `recipe: Arc<str>`,
`constant_args: Vec<Arc<str>>`, `target_descriptor: Arc<str>` — cheap refcount
data, chosen deliberately so cloning a cached site is a refcount bump. But
`execute_cached_call_site` then **rebuilt a whole `IndyInfo`** just to satisfy
`execute_string_concat`'s `&IndyInfo` parameter:

```rust
let info = IndyInfo {
    target_descriptor: target_descriptor.to_string(),                    // alloc
    recipe: recipe.to_string(),                                          // alloc
    constant_args: constant_args.iter().map(|s| s.to_string()).collect(),// alloc × (N+1)
    …
};
```

That is `2 + N` `String` heap allocations plus a `Vec` **on every single string
concatenation**, discarded microseconds later, purely as an `Arc<str> → String`
adapter. The `makeConcat` bootstrap branch cloned the entire `IndyInfo` a second
time for the same reason (`patched_info`).

**Fixed.** `execute_string_concat` now takes borrowed data —
`recipe: &str`, `constant_args: &[S] where S: AsRef<str>`, `target_descriptor: &str`
— so the cached path passes its `Arc<str>`s straight through with zero
conversion, and the bootstrap path passes its `&[String]` against the same bound.
The `patched_info` clone is gone. Behaviour is identical; the function reads
exactly the same three values it always did.

Remaining per-execution allocation on this path: `parse_descriptor_args` returns
a fresh `Vec<char>` each time, and the result `String` itself (unavoidable —
`StringConcatFactory` must produce a new, uninterned `String` per the spec). See
[follow-ups](#follow-ups-not-landed-here).

### 2c. `bootstrap_generic` re-runs the entire bootstrap on every execution

Documented in the source as deliberate ("Correctness-first (no call-site caching
yet)"). Per execution of a non-JDK-factory `invokedynamic` — i.e. **every Groovy,
JRuby and Kotlin-`invokedynamic` call site** — it:

1. re-takes `class_manager.read()` and re-resolves the `InvokeDynamic` CP entry,
   the bootstrap-method handle and every static argument;
2. invokes `MethodHandles.lookup()` (real Java);
3. allocates a `java.lang.String` for the call-site name;
4. builds a `MethodType` object;
5. materialises every static bootstrap argument (loading classes, minting class
   mirrors, boxing primitives through `Integer.valueOf` &c.);
6. **invokes the bootstrap method itself** — arbitrary user Java;
7. calls `CallSite.getTarget()`;
8. only then invokes the target `MethodHandle`.

Steps 1–6 are pure linkage that a real JVM performs exactly once. This is a
plausible contributor to the review's 8–21x framework-shaped gap wherever Groovy
is in play (Spring Boot's Groovy config, LayoutDialect, JRuby templates).

**Not landed here**, deliberately. The correct fix caches the **`CallSite`
object** — *not* its target `MethodHandle`: Groovy's `IndyInterface` uses
`MutableCallSite` for its inline caches and swaps the target at runtime, so
`getTarget()` must still run per execution. Caching an `ObjectRef` requires a GC
root-scan hook and a post-compaction remap hook, which live in
`vm/src/memory/roots.rs` and `vm/src/memory/gc.rs` — files this pass does not
own. Full handoff in [cross-owner requests](#cross-owner-requests). The
env-var read that sat in this loop (`CRATONVM_DBG_INDY_GENERIC`, read on every
execution) **is** fixed here.

### 2d. Latent: `functional_interface_id: None` is baked in permanently

The "memoized `None` is permanent" pattern the brief warned about **does** occur,
though not in the call-site cache. In `bootstrap_lambda`:

```rust
let functional_interface_id = shared.classes.class_manager.read()
    .get_loaded_class_id_for_requester(&functional_interface, host_loader);
```

`get_loaded_class_id_for_requester` only consults **already-loaded** classes. At
first execution of the `invokedynamic` the functional interface frequently is not
loaded yet, so `None` is stored into the `LambdaCallSite` — which is then cached
in *both* `resolution_cache` and the `lambda_proxies` registry and never
re-resolved, even after the interface loads moments later.

**Impact is degradation, not breakage:** every consumer treats the field as a
*hint* with a name-based fallback (`interpreter.rs` `lambda_iface_override` uses
`?` and falls through; `try_lambda_default_method_dispatch` passes it as
`interface_id_hint`). The field exists to disambiguate two loaders defining the
same interface name (the Spring `@CompileWithForkedClassLoader` scenario), so a
permanent `None` silently reintroduces exactly the loader-identity race the field
was added to remove — in whichever call sites happen to bootstrap early.

**Deliberately not changed.** Every candidate fix (force-resolve the interface at
bootstrap; re-resolve and patch the cached site on a `None` hit) either
introduces a new class-load at a new point in bootstrap ordering, or mutates two
shared registries from the dispatch path. This is the loader-identity area that
`spring-aot-cluster-loader-identity-fixes` and
`aot-double-refresh-springextension-loader-blind-fix` scarred; changing it
without the ability to build or run the Spring AOT suite would be reckless.
Written up in [follow-ups](#follow-ups-not-landed-here) with the proposed shape.

---

## Already optimized — review premises that did not hold

Recorded explicitly, because the review assumed otherwise and a sibling pass hit
the same class of stale assumption (ARCHITECTURE.md described quickening as a
future fix when it had already landed):

1. **"If the call-site bootstrap result is not cached per call site, every lambda
   evaluation re-runs bootstrap."** It *is* cached, for every JDK factory
   including `LambdaMetafactory`. Only non-JDK bootstraps re-run (2c).
2. **Zero-capture lambdas do not even allocate** after first evaluation —
   `LAMBDA_SINGLETON_CACHE` mirrors HotSpot's `InnerClassLambdaMetafactory`
   cached-`INSTANCE` behaviour, with GC scan/remap hooks already wired.
3. **Redefinition invalidation is already correct**, including the non-obvious
   case: symbolic `MethodHandle`s mean redefining the class that owns a lambda's
   implementation needs no cache eviction at all.
4. **The two-layer `ExceptionThrown`/`InternalError` split costs nothing on the
   catchable path** (1c) — every `format!` in this file is on an `InternalError`
   or genuine-`LinkageError` branch.
5. `create_exception_object` and friends are already `#[cold]`, and the
   `tracing::enabled!(DEBUG)` guard already covers the expensive frame-walk
   diagnostics. The uncached `env::var_os` calls (1a) were the ones that had
   escaped *outside* that guard.

---

## Changes landed

`vm/src/runtime/exceptions.rs`

* `cached_env_flag!` macro + seven memoized flags replacing uncached
  `std::env::var_os` reads on the throw / class-resolution / linkage-error entry
  paths. `iae_trace_enabled` keeps its `env::var` semantics, now with a
  function-local `OnceLock`.
* `instance_field_index_by_name` — read-only hierarchy field-index resolver,
  the companion to the existing `set_detail_message_by_name` /
  `set_cause_by_name` walkers.
* `trace_already_captured_at_current_depth` — the fail-closed proof that lets
  `create_exception_object_for_class` skip its redundant `fillInStackTrace`.
* Step 4 of `create_exception_object_for_class` is now guarded by that check.

`vm/src/runtime/invokedynamic.rs`

* `cached_env_flag!` macro + three memoized flags (`CRATONVM_DBG_INDY_ALL`,
  `CRATONVM_DBG_INDY_GENERIC`, `CRATONVM_DBG_LAMBDA_DISPATCH`); the
  `INDY_GENERIC` one was read on every generic-indy execution.
* `execute_string_concat` re-signatured to borrowed `&str` / `&[S: AsRef<str>]`
  data; the per-execution `IndyInfo` rebuild in `execute_cached_call_site` and
  the `patched_info` clone in the `makeConcat` branch are both deleted.

Tests added (all `#[cfg(test)]`, all deterministic without a JDK on the
classpath):

* `cached_throw_path_debug_flags_match_environment_and_are_stable` /
  `cached_indy_debug_flags_match_environment_and_are_stable` — compare every
  memoized flag against the live environment, catching a typo'd or inverted
  variable name (a memo's characteristic failure mode: a silently dead switch).
* `duplicate_trace_guard_is_closed_with_no_frames` — the fail-closed base case.
* `constructor_capture_marker_implies_a_registered_trace` — the fidelity
  invariant behind the skip: `backtrace == this` ⟹ a trace is registered.
* `nested_throwables_keep_independent_traces` — nested/rethrown shape; building
  an outer throwable must not clobber the inner one's trace.
* `bootstrapped_call_site_is_reused_not_rebootstrapped` — cache hit/miss
  contract, and that no negative result is memoized.
* `lambda_call_site_is_dropped_when_its_caller_is_redefined` — lambda before and
  after a redefinition, including that unrelated classes are untouched.
* `lambda_impl_handle_is_symbolic_so_impl_redefinition_needs_no_eviction` —
  guards the reasoning that makes key-only eviction sound.
* `cached_string_concat_site_exposes_borrowable_recipe_and_constants`,
  `synthesized_make_concat_recipe_is_all_argument_placeholders` — the borrow
  shape and the `patched_info` removal.

The `athrow`-path JEP 358 message contract (`helpful_npe::action_throw()` ==
`"Cannot throw exception"`) already had coverage in this file and was left as-is.

Not built or tested (nine concurrent agents; building is prohibited this pass).
Both files are `rustfmt --check` clean and CRLF line endings are preserved.

---

## Cross-owner requests

Requested edits in files this pass does not own. **None of these were made.**

### CR-1 — `vm/src/vm/vm_exec.rs`: uncached `env::var_os` on the trace path

`NativeContextImpl::capture_current_stack_trace` (~line 2195) and
`get_stack_trace` (~line 4833) each call
`std::env::var_os("CRATONVM_DBG_STTRACE")` **per capture / per lookup**. That is
the same syscall cost fixed in 1a, sitting directly on the throw path and on
every `getStackTrace()`. Please route both through
`crate::runtime::env_cache` (add `cached_is_set!(dbg_sttrace, "CRATONVM_DBG_STTRACE")`).
Cheap, zero-risk, no behaviour change.

### CR-2 — `vm/src/vm/vm_exec.rs`: the redundant `Vec` clone per capture

```rust
fn capture_throwable_stack_trace(&mut self, throwable: ObjectRef) -> Vec<StackTraceEntry> {
    let trace = self.capture_current_stack_trace();
    self.shared.store_throwable_stack_trace(throwable, trace.clone());  // ← full clone
    trace
}
```

Every caller in `native-builtins/src/lang_misc.rs::capture_throwable_trace` uses
the return value only for `trace.len()`. Changing the return type to `usize` (or
storing first and returning the stored length) deletes a full
`Vec<StackTraceEntry>` allocation plus N `Arc` refcount bumps per capture.

### CR-3 — `vm/src/runtime/stackwalker.rs`: lazy line-number resolution

`entry_from_frame` resolves the source line eagerly for every frame of every
capture: `class_store.get` → `find_method(name, descriptor)` → decode `Code` →
linear `LineNumberTable` scan. For an exception that is caught immediately and
whose trace is never read — the dominant case in Spring/Hibernate/JUnit, where
exceptions are control flow — **all** of that work is discarded.

`capture_frames_no_lines` already exists in the same file and does exactly the
cheap thing (it records `class_id` + `byte_code_index` and leaves
`line_number = LINE_NUMBER_UNKNOWN`). Proposal: capture with
`capture_frames_no_lines` and resolve line numbers lazily in
`native_init_stack_trace_elements` / `get_stack_trace`, where the BCI and
`class_id` needed are already stored on each `StackTraceEntry`. **Fidelity is
preserved** — the same frames in the same order, with the same BCIs; only the
`line_number` field is filled in later, from the same table. This is the largest
remaining per-throw win and is squarely in the stackwalker owner's lane.

### CR-4 — `vm/src/vm/vm_init.rs`: `throwable_stacks` is a single global `RwLock`

`store_throwable_stack_trace` takes `threads.throwable_stacks.write()` on every
capture, so all throwing threads serialise on one lock. Sharding by
`identity_hash % N` (or moving to a per-thread store with a hand-off on
cross-thread inspection) would remove that contention. Note the GC remap hook
`remap_and_sweep_throwable_stack_traces` and the loader-unload sweep both walk
this map and would need to walk all shards.

Also useful and trivial: a `throwable_stack_trace_len(hash) -> Option<usize>`
accessor. `throwable_stack_trace` currently **clones the whole `Vec`** for every
reader, which is why `trace_already_captured_at_current_depth` had to prove
equality via the `depth` field instead of just comparing lengths.

### CR-5 — `vm/src/memory/roots.rs` + `vm/src/memory/gc.rs`: hooks for a generic-`CallSite` cache

To land the 2c fix, `invokedynamic.rs` needs a per-call-site `ObjectRef` cache
holding the **`CallSite` object** (not its target — `MutableCallSite` targets
must stay live). The exact pattern already exists in `invokedynamic.rs` for
`LAMBDA_SINGLETON_CACHE`; two one-line registrations mirror it:

* `roots.rs`, alongside the `gc_scan_lambda_singleton_roots(vm_identity, out)`
  call, add `gc_scan_generic_callsite_roots(vm_identity, out)`;
* `gc.rs`, alongside `gc_update_lambda_singleton_refs(vm_identity, pointer_map)`,
  add `gc_update_generic_callsite_refs(vm_identity, pointer_map)`.

`invokedynamic.rs` would then own the map (keyed `(vm_identity, ClassId, u16)`),
the `getTarget()`-per-execution discipline, and invalidation on redefine.
Happy to implement the `invokedynamic.rs` half once the two hook call sites
exist — ping this slug.

### CR-6 — `vm/src/runtime/interpreter.rs` and the native crates: eager `format!` on catchable throws

Review item 4's real occurrence is *outside* `exceptions.rs`. Callers build
`RuntimeError` variants with an eagerly formatted `String` message
(`ClassCastException { message: format!("{a} cannot be cast to {b}") }` and
similar) at the raising opcode, before anyone knows whether the exception will be
caught and discarded. Making the message lazy (a small enum of pre-resolved parts
rendered only when `getMessage()` / trace printing asks) would remove one heap
allocation + formatting pass per caught-and-discarded VM exception. Flagged, not
scoped — it touches `error.rs`'s `RuntimeError` shape and every raise site.

---

## Follow-ups (not landed here)

* **`parse_descriptor_args` per string-concat execution.** Returns a fresh
  `Vec<char>` on every `"a" + b`. The argument-type list is a property of the
  call site and could be precomputed into `ResolvedCallSite::StringConcat` at
  bootstrap (an `Arc<[char]>` field). Requires a shape change in
  `classloading/src/resolution.rs` — not owned by this pass, and small enough
  that it should ride along with whoever next touches that enum.
* **`functional_interface_id` permanent `None`** (2d). Proposed shape, for
  whoever owns the loader-identity area: on a cached-lambda dispatch where
  `functional_interface_id.is_none()`, re-attempt
  `get_loaded_class_id_for_requester` and, on success, patch both
  `resolution_cache` and `lambda_proxies`. This converts a permanent negative
  memo into one retried until it succeeds — the same fix shape a sibling applied
  to the native registry's cached-`None`. Must be validated against the Spring
  AOT `@CompileWithForkedClassLoader` suite, which is why it is not landed blind.
* **Lazy stack traces** — CR-3 is the enabling change; once
  `capture_frames_no_lines` feeds the throw path, `create_exception_object`'s
  capture becomes cheap enough that the `trace_already_captured_at_current_depth`
  guard could arguably be retired. Keep the guard until then.
