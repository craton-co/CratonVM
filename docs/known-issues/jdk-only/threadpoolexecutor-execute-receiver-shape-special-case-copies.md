# The `ThreadPoolExecutor.execute` receiver-shape special case is copied **eight** times, not four

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31, re-verified
against the re-landed tree the same day. All copies become removable once one
native is reclassified — but the marker set still names only four of them, so a
mechanical "delete every `JDK-ONLY-WAVE2` ThreadPoolExecutor site" sweep leaves
half the duplication behind. **The re-land did not change the undercount**;
that is why this item moved up the ranking rather than down.

## What changed on 2026-08-04 — the undercount is gone, the sites are not

The ranking hazard is retired. What made this item tier-1 was not the
duplication itself but that the *markers* named four of eight, so the obvious
mechanical sweep would have left half the duplication enforcing a policy the
other half no longer applied. That is now impossible:

* **One census, in code.** `THREADPOOL_EXECUTE_RECEIVER_SHAPE_SITES` in
  `vm/src/runtime/interpreter/native_override.rs` names all eight by
  `(file, enclosing function)`, plus the ninth receiver-blind site they exist to
  override, plus the four-step order the removal has to happen in — and why
  getting that order wrong aborts the process instead of throwing.
* **One implementation of the probe.** Two of the eight hand-inlined the
  `workers`-field lookup instead of calling
  `threadpool_executor_has_real_workers`, so the predicate had three bodies.
  Worse, both inlined copies took a plain `read()` where the helper documents
  that a nested `read_recursive()` is required — a lock-order panic in debug
  builds and a possible deadlock in release. Both call the helper now.
* **A gate that fails on a partial sweep.**
  `exactly_eight_dispatch_sites_probe_the_threadpool_receiver_shape` asserts the
  count, as an equality rather than a floor (this list only ever shrinks, and it
  shrinks all at once), and
  `no_hand_inlined_workers_probe_outside_the_helper` asserts nobody re-inlines
  the probe. Verified by injection: deleting one site reports *"found 7 call(s)
  … lists 8"*.

The record's own *Coverage first* verification step said not to rely on the
markers because they cover four of eight. The census constant and the gate are
what replace that instruction.

## What is still open — all four steps

Nothing above deletes a site, and deleting one early is the failure mode with
the worst blast radius in this directory (a native stack overflow and process
abort, not a catchable `StackOverflowError`). The order is unchanged:

1. give real `ThreadPoolExecutor` objects correct Java field initialisation so
   `Executors.new*ThreadPool()` returns objects built by the real `<init>`
   (`docs/jdk-only-runtime-services.md` P1). **This is the real work, and it
   gates everything else** — reclassifying before it lands drops
   `native_es_execute` under `CRATONVM_NO_STUBS` / `--jdk-only` and
   synthetic-receiver executors lose their only implementation;
2. reclassify `native_es_execute` as `NativeKind::SyntheticStub`;
3. delete the ninth, receiver-blind site;
4. delete the eight.

## What is wrong

`native_es_execute` is a compatibility stand-in for CratonVM's synthetic 2-field
`Executors.new*ThreadPool()` objects. It is registered on
`java/util/concurrent/ThreadPoolExecutor.execute(Ljava/lang/Runnable;)V`, whose
real class bytecode is *always* loaded — so the general
`SyntheticStub`/`CRATONVM_REAL` yield logic cannot disambiguate it, because that
logic is **class**-scoped and the question here is per-**instance**.

The workaround is a receiver-shape probe: read the receiver's `workers` field;
if it is a non-null object, the receiver was built by the real `<init>` and must
run real `execute()` bytecode; otherwise it is one of CratonVM's synthetic
stand-ins and the native must run.

That probe is duplicated at **eight** dispatch sites in the `vm` crate.

## The eight sites (re-verified 2026-07-31 against the re-landed tree)

`vm/src/vm/vm_exec.rs` — probe written out inline via
`resolve_field_index_in_hierarchy(recv_class_id, "workers", &cm.class_store)`:

| Line | Context | Marked? |
|---|---|---|
| 13524 | `invoke_or_native` | **yes** — marker at 13503, "COPY 1 OF 4" |
| 21363 | `force_native_receiver_exempt`, guarding `should_force_registered_native_over_bytecode` in `invoke_on_class_shared_inner` | **yes** — marker at 21352, "COPY 4 OF 4 — and the one that does NOT call `threadpool_executor_has_real_workers`" |

`vm/src/runtime/interpreter/invoke.rs` — probe factored into
`threadpool_executor_has_real_workers` (defined at 9724, reading the `workers`
field index at 9738):

| Line | Enclosing function | Marked? |
|---|---|---|
| 10001 | `intercept_force_registered_native` (fn at 9749) | no |
| 10152 | `intercept_force_registered_native_cached` (fn at 10043) | no |
| 11153 | `try_stackless_invoke` step 1, direct native lookup (fn at 10735) | **yes** — marker at 11147, "COPY 2 OF 4" |
| 11617 | `try_stackless_invoke` step 6, post-resolution double-check | **yes** — marker at 11609, "COPY 3 OF 4" |
| 23063 | `populate_virtual_invoke_cache` (fn at 22763) — keeps the native shadow out of the inline cache | no |
| 23362 | `populate_virtual_invoke_cache`, the `force_native_over_real_jdk_bytecode` arm | no |

**The markers name only four of eight.** COPY 1's text enumerates the other
three and gets one of them wrong on top of the undercount: it says all three
live in `invoke.rs`, when COPY 4 is in `vm_exec.rs`'s own
`invoke_on_class_shared_inner`. `intercept_force_registered_native`,
`intercept_force_registered_native_cached` and both
`populate_virtual_invoke_cache` sites carry no `JDK-ONLY-WAVE2` marker at all.

There is also a ninth, *different* site: `invoke.rs` 7615, inside
`force_native_over_real_jdk_bytecode`, which returns `true` for the
`(ThreadPoolExecutor, execute, (Ljava/lang/Runnable;)V)` triple with **no
receiver awareness at all**. That is the unconditional decision the other eight
exist to override. It must be deleted in the same change or the overrides cannot
be. The enclosing function's own doc marker (6946) does flag *"the
`ThreadPoolExecutor` family"* as one of two branches that cannot be removed on
their own, so the site is discoverable — but only from the function header, not
from the branch.

Beyond the `vm` crate, the same probe appears as `executor_has_real_workers`
(defined in `native-builtins/src/lib.rs` ~32950) with **eight call sites**
across `native-builtins` (`lib.rs`, `lucene_es.rs` ×3, `util_concurrent_ext.rs`
×2, `phases_late/concurrent.rs` ×2) plus a deliberately-separate twin in
`native-collections/src/lib.rs` (~47363, *"kept separate to avoid a"*
cross-crate dependency). There the natives themselves re-check and redirect a
genuinely-real receiver. Those are defence in depth *inside* the callee and are
a separate cleanup; they are listed here only so a wave-2 grep does not mistake
them for dispatch sites — and so nobody deletes them at the same time as the
dispatch-side probes, which would remove both the check and its backstop in one
change.

## Why each copy exists (the history is worth keeping)

Each site was added to fix a distinct observed failure, which is why they
accumulated rather than being factored:

* `invoke_or_native` — calling `.execute()` on a genuinely-real executor from
  native code (`ctx.invoke_virtual`) recursed back into the same native forever:
  *"a real stack overflow, confirmed via gdb"*. See
  `fixed-suite-bugs/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`.
* `try_stackless_invoke` steps 1 and 6, and both `populate_virtual_invoke_cache`
  sites — without them a real `ThreadPoolExecutor` was shunted into
  `native_es_execute`'s "run inline" fallback, silently degrading async
  execution to synchronous. See
  `fixed-suite-bugs/threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md`.
  Step 6 is called out as *"a SEPARATE, independent double-check … that runs
  even after real bytecode was already resolved at step 4/5."*
* The cache sites are the subtle ones: `populate_virtual_invoke_cache` had to
  learn the exemption because the cache is keyed by (call site, receiver class)
  and a poisoned entry keeps serving `VirtualNative` for every later receiver at
  that site.

## What specifically must change

Per the wave-1 marker, all of them collapse into one structural rule:

1. **Reclassify `native_es_execute` as `NativeKind::SyntheticStub` in
   `native-builtins`.** It is currently tagged such that the general yield logic
   does not apply to it; the marker calls it *"a `NativeKind::SyntheticStub`
   masquerading as a bridge on a class whose real bytecode is always loaded."*
2. Delete the ninth site (`force_native_over_real_jdk_bytecode`'s unconditional
   `true`) — otherwise step 3 below is unreachable.
3. Delete all eight receiver-shape blocks. Contract §7 step 3 ("concrete
   bytecode beats a registered `Bridge` or `SyntheticStub`") then produces the
   same answer **structurally, for every receiver, with no field probe and no
   class-name list**.
4. The real fix underneath, per `docs/jdk-only-runtime-services.md`'s P1 entry:
   *"Give real `ThreadPoolExecutor` objects correct Java field initialisation
   and execute their bytecode. No receiver-shape heuristics."* Once
   `Executors.new*ThreadPool()` returns objects built by the real `<init>`,
   there is no synthetic receiver to distinguish and `native_es_execute` can go
   away entirely.

## How to verify a fix

* **Coverage first:** `rg 'ThreadPoolExecutor' vm/src | rg 'workers|has_real_workers'`
  must return zero dispatch sites. Do not rely on the `JDK-ONLY-WAVE2` markers —
  they cover four of eight.
* **The self-recursion guard:** a native calling `.execute()` on a real,
  bytecode-constructed `ThreadPoolExecutor` via `ctx.invoke_virtual` must not
  recurse. This one aborts the process rather than throwing, so it must be an
  explicit test.
* **The async-degradation guard:** a real `ThreadPoolExecutor` must run tasks on
  a worker thread, not inline on the caller. Assert the executing thread differs
  from the submitting thread — an inline fallback passes any test that only
  checks the task ran.
* **The cache-poisoning guard:** the same call site must behave correctly when a
  synthetic executor and a real one alternate through it — that is what the
  `populate_virtual_invoke_cache` sites protect and what a monomorphic cache
  gets wrong.
* Existing coverage: both `FIXED` docs above name their original repros; both
  must stay green.

## Blast radius if done wrong

* Deleting the receiver-shape blocks **before** `native_es_execute` is
  reclassified restores the `ctx.invoke_virtual` self-recursion — a native stack
  overflow / process abort, not a catchable `StackOverflowError`.
* Deleting only the four marked sites leaves the four unmarked ones enforcing a
  policy the other four no longer apply, i.e. the same cold-path/warm-path split
  documented in
  [the forced-native `String` policy](forced-native-string-policy-two-lists-that-disagree.md).
* Reclassifying `native_es_execute` to `SyntheticStub` while
  `CRATONVM_NO_STUBS` / `--jdk-only` is in play **drops the registration
  entirely** (see
  [`NativeKind` is ambient](native-kind-is-ambient-and-defaults-to-syntheticstub.md)),
  so synthetic-receiver executors lose their only implementation. Step 4 (real
  field initialisation) must land first, or strict mode loses
  `Executors.new*ThreadPool()`.
