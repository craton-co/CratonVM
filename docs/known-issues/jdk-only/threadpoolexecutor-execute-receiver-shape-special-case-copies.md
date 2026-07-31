# The `ThreadPoolExecutor.execute` receiver-shape special case is copied **eight** times, not four

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. All copies
become removable once one native is reclassified — but the wave-1 marker set
names only four of them, so a mechanical "delete every `JDK-ONLY-WAVE2`
ThreadPoolExecutor site" sweep leaves half the duplication behind.

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

## The eight sites (verified 2026-07-31 against the current tree)

`vm/src/vm/vm_exec.rs` — probe written out inline via
`resolve_field_index_in_hierarchy(recv_class_id, "workers", &cm.class_store)`:

| ~Line | Context |
|---|---|
| 13182 | `invoke_or_native` |
| 20932 | `force_native_receiver_exempt`, guarding `should_force_registered_native_over_bytecode` |

`vm/src/runtime/interpreter/invoke.rs` — probe factored into
`threadpool_executor_has_real_workers` (defined at ~9691, itself documented as
*"Mirrors `native-builtins::executor_has_real_workers` (same check, same …)"*):

| ~Line | Enclosing function |
|---|---|
| 9968 | `intercept_force_registered_native` |
| 10119 | `intercept_force_registered_native_cached` |
| 11006 | `try_stackless_invoke`, step 1 (direct native lookup) |
| 11381 | `try_stackless_invoke`, step 6 (post-resolution "double-check for a native override") |
| 22733 | `populate_virtual_invoke_cache` — keeps the native shadow out of the inline cache |
| 23032 | `populate_virtual_invoke_cache`, the `force_native_over_real_jdk_bytecode` arm |

**Wave 1's markers name only four**: `vm_exec.rs`'s two and
`try_stackless_invoke`'s steps 1 and 6. The marker text is explicit —
*"the first is in `invoke_or_native`, a third is `try_stackless_invoke` step 1
and a fourth is its step 6 … All four disappear together"* — and it is an
undercount. `intercept_force_registered_native`,
`intercept_force_registered_native_cached` and both
`populate_virtual_invoke_cache` sites carry no `JDK-ONLY-WAVE2` marker.

There is also a ninth, *different* site: `invoke.rs` ~7582, inside
`force_native_over_real_jdk_bytecode`, which returns `true` for the
`(ThreadPoolExecutor, execute, (Ljava/lang/Runnable;)V)` triple with **no
receiver awareness at all**. That is the unconditional decision the other eight
exist to override. It must be deleted in the same change or the overrides cannot
be.

Beyond the `vm` crate, the same probe appears as `executor_has_real_workers` in
`native-builtins` (`lib.rs`, `lucene_es.rs`, `util_concurrent_ext.rs`,
`phases_late/concurrent.rs`) and `native-collections/src/lib.rs`, where the
natives themselves re-check and redirect a genuinely-real receiver. Those are
defence in depth *inside* the callee and are a separate cleanup; they are listed
here only so a wave-2 grep does not mistake them for dispatch sites.

## Why each copy exists (the history is worth keeping)

Each site was added to fix a distinct observed failure, which is why they
accumulated rather than being factored:

* `invoke_or_native` — calling `.execute()` on a genuinely-real executor from
  native code (`ctx.invoke_virtual`) recursed back into the same native forever:
  *"a real stack overflow, confirmed via gdb"*. See
  `docs/internal/fixed-suite-bugs/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`.
* `try_stackless_invoke` steps 1 and 6, and both `populate_virtual_invoke_cache`
  sites — without them a real `ThreadPoolExecutor` was shunted into
  `native_es_execute`'s "run inline" fallback, silently degrading async
  execution to synchronous. See
  `docs/internal/fixed-suite-bugs/threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md`.
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
