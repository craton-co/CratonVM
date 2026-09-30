# Proposal: make a linked generic `invokedynamic` cheap to execute, not just cheap to link

**Status: open, stage 1 and stage 2a landed — filed 2026-09-23 by interpreter round i1 wave 2, lane L6.** (proposal)

## Progress (wave 16, 2026-09-25, lane L1)

Re-derived from the current code. Wave 15 already made `invoke_shared` pin
the arguments once and copy them only when a load moved one
(`load_class_pinning_args`, `args_refreshed_if_moved`). What a linked generic
execution (`invoke_generic_call_site`) still did before the call itself: a
second pin pass over every reference argument (all of them already pinned by
`invoke_generic_call_site`), and a `load_class_concurrent` read-lock probe
plus an Fx hash of `"java/lang/invoke/MethodHandle"`.

* **Stage 2a landed** (`vm/src/runtime/invokedynamic.rs`):
  `GenericIndyShape::method_handle_class` resolves `MethodHandle` once per
  linked site (a `OnceLock<ClassId>`; a shape belongs to one row and a row to
  one VM, and a bootstrap class's id never changes), BEFORE the pin refresh,
  since the first resolution can load. The call is then
  `invoke_by_class_id_shared(shared, thread, mh_class, "invoke", desc, ..)` —
  exactly `invoke_shared`'s tail, with this function's pins held across it.
  A load failure returns the same error as before, pins released. Test:
  `invokedynamic::i16_l1_indy_site_tests::a_generic_shape_resolves_method_handle_once`.
* A site that is NOT cached (Groovy's `MutableCallSite`s under the default
  `CRATONVM_INDY_CALLSITE_CACHE`) builds a fresh shape per execution
  (`link_generic_call_site`), so it resolves the class per execution, as it
  did.
* The `Arc<GenericIndyShape>` clone and the method-key hash of the global
  `GENERIC_INDY_SITES` probe remain for `Linked` rows; next to the
  `MethodHandle.invoke` adaptation they are small, so they wait for the
  Groovy timing too.

Next stage, unchanged in substance: stage 2 proper (the pre-resolved
`MethodHandle.invoke` callback id on `GenericIndyShape`, re-admitted per call
like `lambda::lambda_receiver_native_id`) needs the trace of what
`invoke_on_class_shared(MethodHandle, "invoke", desc)` ends in and the Groovy
`each` loop timing (`--nojit`, `=all` and unset); after that,
`canonical_boolean`'s per-`Z`-operand `class_manager` read and static-field
walk can be memoised on the shape the same way (keyed by the resolution epoch,
since a synthetic `Boolean` can be upgraded in place).

## Progress (wave 8, 2026-09-24, lane L6)

No stage landed. Code reading of what stage 2 would remove, so the Groovy
timing can be read against it:

* `invoke_generic_call_site` already pins every operand and the target, and
  re-reads them right before the call. `crate::vm::invoke_shared` then does
  the same work again per execution: one pin push per reference argument (the
  target handle at least), `load_class_concurrent("java/lang/invoke/MethodHandle")`
  (read-lock fast path), `ensure_class_initialized_shared`, and an
  `args.to_vec()` heap allocation (always taken: the target is a reference),
  before `invoke_on_class_shared` routes the signature-polymorphic `invoke`.
  The class is necessarily loaded and initialized once a target handle
  exists, so a stage "2a" can call `invoke_on_class_shared` with the
  `MethodHandle` class id directly and keep this function's pins alive across
  the call. That saves one allocation and a few locked probes per execution,
  which is small next to the `MethodHandle.invoke` adaptation itself; worth
  landing only together with stage 2 (the callback id), after the Groovy loop
  timing says where the time goes.
* Class unloading now also drops the lambda rows keyed by an unloaded host's
  proxies (`forget_unloaded_lambda_proxy_rows`, wave 8); `GENERIC_INDY_SITES`
  rows were already dropped with their class.

## Progress (wave 7, 2026-09-24, lane L6)

* **Inherited open decision: Groovy's `MutableCallSite`s.** The bug page
  `interpreter-L6-generic-indy-rebootstraps-every-execution` was retired
  (`docs/internal/fixed-bugs/interpreter-L6-generic-indy-rebootstraps-every-execution-RETIRED-20260924.md`):
  every non-Groovy bootstrap links once per instruction. What it left is the
  `GenericIndyCacheMode::Constant` carve-out — a `MutableCallSite` /
  `VolatileCallSite` from Groovy's `IndyInterface` (`vmplugin/v8`, `v7`) is
  still re-bootstrapped per execution by default; `CRATONVM_INDY_CALLSITE_CACHE=all`
  already gives the JVMS behaviour. To decide: run the Groovy / Spring Boot
  Groovy config / Thymeleaf LayoutDialect suites with `=all` and unset; if
  they agree, drop `is_groovy_indy_bootstrap` from
  `caches_mutable_call_site_of` (then this plan's steady state is Groovy's
  every call, which is when stage 2 pays).
* The per-thread front table the wave-6 note mentions landed for the
  heap-free shapes (`ThreadInstructionSites`; shared lambda entries and
  Groovy's `cast:(Object)Z`), see the wave-7 note on
  `interpreter-L6-proposal-per-instruction-indy-site-cache-FIXED-20260930.md`. A `Linked`
  row (a `CallSite` `ObjectRef`) still takes the global table: copying it
  per thread needs a per-thread GC root, i.e. a `JvmThread` field in BOTH
  `memory::roots::push_off_frame_thread_roots` and
  `memory::gc::remap_thread_off_frame_refs`.

## Progress (wave 6, 2026-09-24)

* The default cache mode now publishes a `MutableCallSite` from any bootstrap
  but Groovy's (`GenericIndyCacheMode::caches_mutable_call_site_of`; see the
  wave-6 note on `docs/internal/fixed-bugs/interpreter-L6-generic-indy-rebootstraps-every-execution-RETIRED-20260924.md`),
  so for those runtimes the steady state this proposal prices — probe, target
  read, `MethodHandle.invoke` — is now the path every execution takes, not
  only under `=all`.
* `GENERIC_INDY_SITES` also holds the per-instruction rows of shared lambda
  entries (`GenericIndyLink::Lambda`) since wave 6; the "key once" item and a
  per-thread front table (see the wave-6 note on
  `interpreter-L6-proposal-per-instruction-indy-site-cache-FIXED-20260930.md`) would serve
  both. Stage 2 unchanged: trace the `MethodHandle.invoke` callback first.

## Progress (wave 5, 2026-09-24)

* Not a stage of this plan, but the same path: the bootstrap-side decode an
  uncached generic site repeats per execution no longer renders the static
  arguments as concat text (see the wave-5 note on
  `docs/internal/fixed-bugs/interpreter-L6-generic-indy-rebootstraps-every-execution-RETIRED-20260924.md`).
* Stage 2 (pre-resolved `MethodHandle.invoke`) was looked at and left. The
  by-name `invoke_shared` routes a signature-polymorphic method specially
  and applies the §7 dispatch policy, so a callback cached at linkage would
  have to reproduce that routing, the admission and the `--jdk-only` census
  on every execution. The lambda receiver-native memo solved the same shape
  by caching only the registry id and re-running the admission per call
  (`lambda::lambda_receiver_native_id`); trace exactly which callback
  `invoke_shared("java/lang/invoke/MethodHandle", "invoke", desc)` ends in
  first, then do the same with the id on `GenericIndyShape`, stamped with
  the registry generation. Needs the Groovy loop timing first.

## Progress (wave 3)

* **Stage 1 landed** (`vm/src/runtime/invokedynamic.rs`,
  `invoke_generic_call_site`):
  * a `Z` operand is boxed to `Boolean.TRUE` / `Boolean.FALSE` read straight
    from `Boolean`'s statics (`canonical_boolean`: one `class_manager` read,
    a short static-field walk, `get_static_shared`; no Java, no allocation,
    not a safepoint). The `Boolean.valueOf` call stays as the fallback while
    `Boolean` is unloaded or its `<clinit>` has not stored the two
    instances. The static is pinned like any other operand, since a later
    `Z` slot's fallback call is a safepoint. Test:
    `canonical_boolean_reads_the_statics_and_declines_before_clinit`.
  * `dyn_args`, `dyn_pins` and `invoke_args` are `SmallVec<[_; 8]>`.
* **Remaining:** stage 2 (pre-resolved `MethodHandle.invoke` callback in
  `GenericIndyShape`), stage 3 (per-`(method, pc)` slot, blocked on the same
  decoded-method side-table hook as
  `interpreter-L6-proposal-per-instruction-indy-site-cache-FIXED-20260930.md`), and the
  `ConstantCallSite` target shortcut. The method-key recomputation per
  probe (item 1 of "Why") is also still there. None of the stage-1 wins has
  been measured yet: time a Groovy `each` loop interpreted, interleaved, per
  the microbench-noise note.

## Why

Wave 2 stopped re-running the bootstrap: a linked `ConstantCallSite` (and,
under `CRATONVM_INDY_CALLSITE_CACHE=all`, any `CallSite`) is published per
instruction (`GENERIC_INDY_SITES`, `vm/src/runtime/invokedynamic.rs`; see
`docs/internal/fixed-bugs/interpreter-L6-generic-indy-rebootstraps-every-execution-RETIRED-20260924.md`). What each
execution still does in `invoke_generic_call_site`:

1. a global `RwLock` read + Fx hash of `(vm, class, cp, method key, pc)`,
   where the method key is itself an Fx hash of the frame's method name and
   descriptor, recomputed per execution;
2. a by-name field-slot lookup for `CallSite.target` (thread-local ring hit);
3. three `Vec` allocations (`dyn_args`, `dyn_pins`, `invoke_args`);
4. **one Java call per `boolean` operand** (`Boolean.valueOf`) — Groovy call
   sites pass trailing `Z, Z` flags, so two extra interpreted calls per
   execution;
5. `invoke_shared("java/lang/invoke/MethodHandle", "invoke", desc, ..)`: a
   by-name method resolution on every execution, then the signature-polymorphic
   native adapting each argument.

For a Groovy/JRuby loop body this is still several microseconds per call site
execution, before the target does any work.

## Design

* **Key once.** Compute the method key when the frame is built (or memoize it on
  the decoded method), so the probe is one hash of five words. Better: store
  the published `GenericIndyLink` in a per-`(method, pc)` slot, as the
  per-instruction site-cache proposal
  (`interpreter-L6-proposal-per-instruction-indy-site-cache-FIXED-20260930.md`) describes for
  the JDK-factory shapes, and drop the global table from the steady state.
* **Resolve the invoker once.** Pre-resolve `MethodHandle.invoke` for the
  site's descriptor to its native callback at linkage and store it in
  `GenericIndyShape`; call it through `safe_native_call` directly.
* **Box booleans without Java.** `Boolean.valueOf` returns one of two cached
  instances; read `Boolean.TRUE` / `Boolean.FALSE` once per VM (statics of an
  initialized class) and reuse them. Keep the Java call only as the fallback
  before `Boolean` is initialized.
* **Inline storage.** `SmallVec<[Value; 8]>` for the three vectors, as the
  concat path already does.
* **`ConstantCallSite` target.** Its target can never change, so the linked
  row can hold the target handle itself and skip step 2.

## Staged plan

1. `Boolean.TRUE`/`FALSE` reuse + `SmallVec`s (local to `invokedynamic.rs`).
2. Pre-resolved `MethodHandle.invoke` callback in `GenericIndyShape`.
3. Per-`(method, pc)` slot (shared with the JDK-factory proposal).

## Expected benefit

Groovy `(1..1_000_000).each { x += it }` and JRuby string interpolation, both
interpreted: two fewer interpreted calls and three fewer allocations per call
site execution after stage 1.

## Verify

`tools/probes/interp/L6/GenericIndyLinkProbe.java` for correctness (with
`CRATONVM_INDY_CALLSITE_CACHE` unset and `=all`); a Groovy script loop timed
before/after, interleaved per the microbench-noise note.
