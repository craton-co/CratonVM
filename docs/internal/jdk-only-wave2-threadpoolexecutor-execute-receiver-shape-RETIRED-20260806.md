# The `ThreadPoolExecutor.execute` receiver-shape special case — RETIRED 2026-08-06

**Status:** ✅ FIXED and retired. All nine dispatch sites are deleted, the probe
helper is deleted, and the class-scoped rule that replaced them is gated by a
test. Filed 2026-07-31 as JDK-only wave-2 item 7; census constant and partial-
sweep gate added 2026-08-04; closed 2026-08-06 together with wave-2 lanes
**L10** (real `ThreadPoolExecutor` field initialisation) and **L11 item 7**.

Previous location:
`docs/known-issues/jdk-only/threadpoolexecutor-execute-receiver-shape-special-case-copies.md`.

## What the record described

`native_es_execute` is a compatibility stand-in for CratonVM's synthetic 2-field
`Executors.new*ThreadPool()` objects. It was registered on
`java/util/concurrent/ThreadPoolExecutor.execute(Ljava/lang/Runnable;)V`, whose
real class bytecode is *always* loaded — so the general
`SyntheticStub`/`CRATONVM_REAL` yield logic could not disambiguate it, because
that logic is **class**-scoped and the question was per-**instance**.

The workaround was a receiver-shape probe — read the receiver's `workers` field;
a non-null object means the real `<init>` ran — duplicated at **eight** dispatch
sites across four files, overriding a **ninth**, receiver-blind
`force_native_over_real_jdk_bytecode` arm that forced the native unconditionally.

## What was done

### 1. The per-instance question is gone — L10, and at registration

The record's step 4 ("the real fix underneath") is
`docs/jdk-only-runtime-services.md`'s P1: make `Executors.new*ThreadPool()`
return objects built by the real `<init>`. **L10 landed that on 2026-08-06**
(`docs/internal/L10-blocker-threadpool-init-DONE-20260806.md`), and did it in
the one place a dispatch-side policy never has to be restated:
`NativeMethodRegistry::register` drops every `java/util/concurrent/Executors`
pool factory when `drop_real_layout_synthetic` is set. On a real-JDK image the
real `java.util.concurrent.Executors` bytecode constructs every executor and
CratonVM has **no code path** that can mint a half-built one.

That is the form of the claim that matters here. The three plain-pool factories
had driven the real `<init>` since 2026-07-10, but kept two
`stpe_legacy_slot_init` fallbacks that wrote the historical two-slot shape onto
a real-layout object and returned it as if construction had succeeded — so the
receiver-shape predicate was CONDITIONALLY true, and conditionally true is
exactly what blocks deleting the sites. L10 also recorded, in terms, that its
own `CRATONVM_DBG_TPE_SHAPE` reading (`true=62 false=0` / `true=38 false=0`,
both modes) was **identical on the pre-fix binary**: the predicate's answer did
not change, its domain did. These sites are deleted because a fabricated
receiver cannot be built — a statement about the code — not because two
workloads never produced one.

The behavioural oracle is `probes/L10ThreadPoolInitProbe`: public API only, so
the host JDK is the oracle, and it carries the three guards this record's own
*How to verify a fix* section demands (async, self-recursion, cache
poisoning). It is byte-identical to HotSpot 25 under `--real-jdk` and
`--jdk-only`, before and after this change.

One corroborating reading of its own: `--dump-native-registry` shows
`ThreadPoolExecutor.execute` at **0 invocations** on every probe run. On a real
JDK image `native_es_execute` was already unreachable; the eight probes were
keeping it that way one dispatch route at a time.

### 2. `native_es_execute` reclassified `NativeKind::SyntheticStub`

`native-builtins/src/util_concurrent_ext.rs`, both registrations
(`ExecutorService.execute` and `ThreadPoolExecutor.execute`), via
`register_with_kind` rather than the ambient category. It is not a bridge to
anything — it is a stand-in for a receiver shape.

### 3. `java/util/concurrent/ThreadPoolExecutor` added to the real-protected-stub allow-list

`real_protected_stub_class_common` in
`vm/src/runtime/interpreter/native_override.rs`. This is the class-scoped rule
that **replaces** all eight probes:

* on an image with real class bytes, `synthetic_stub_kind_should_yield_to_real_bytecode`
  finds a concrete, non-abstract, non-native `execute(Runnable)` body and yields
  the stub to it — for every receiver, with no field probe;
* on a synthetic-JDK image the class *is* a compatibility stub, the predicate
  finds no real body, and the native still runs. That is the only mode that
  still needs it;
* under `--jdk-only` the registration is refused outright at registration time.

Both dispatch paths consult the one predicate (centralised 2026-08-04), and
`revalidate_cached_native` re-asks it on every warm cache hit, so there is no
cold-path/warm-path split for a later change to rediscover.

### 4. All nine sites deleted

| file | site |
|---|---|
| `vm/src/vm/vm_exec.rs` | `invoke_or_native` |
| `vm/src/vm/vm_exec.rs` | `invoke_on_class_shared_inner` |
| `vm/src/runtime/interpreter/invoke.rs` | `try_stackless_invoke` step 1 |
| `vm/src/runtime/interpreter/invoke.rs` | `try_stackless_invoke` step 6 |
| `vm/src/runtime/interpreter/native_override.rs` | `intercept_force_registered_native` |
| `vm/src/runtime/interpreter/native_override.rs` | `intercept_force_registered_native_cached` |
| `vm/src/runtime/interpreter/dispatch_virtual.rs` | `populate_virtual_invoke_cache` |
| `vm/src/runtime/interpreter/dispatch_virtual.rs` | `populate_virtual_invoke_cache` force-native arm |
| `vm/src/runtime/interpreter/native_override.rs` | **the ninth**: `force_native_over_real_jdk_bytecode`'s receiver-blind arm |

`threadpool_executor_has_real_workers` and the
`THREADPOOL_EXECUTE_RECEIVER_SHAPE_SITES` census constant went with them — and
so did `CRATONVM_DBG_TPE_SHAPE` and `probes/L10ShapeInstrumentControlProbe`,
the flag L10 added to measure the predicate and the negative control that made
its `false` branch fire. An instrument for a decision the VM no longer makes
can only ever print nothing, which is the same silence-is-not-zero trap the
flag was designed around; L10's readings are kept in its own record.

**The native itself is NOT deleted, and must not be.** Strict mode declines to
ADMIT a native, it does not remove it, and the `--features synthetic-jdk` build
still registers and runs `native_es_execute` — that is the only build where the
real `execute()` bytecode this stand-in exists for is absent. Two wave-2
records that said it "can go away entirely" were corrected in place by L10.

### 5. One generalisation, deliberately

The `populate_virtual_invoke_cache` exemption did not simply disappear. That
site publishes a `VirtualNative` inline-cache target, and
`revalidate_cached_native` rejects a `SyntheticStub` that should yield — so
publishing one for an allow-listed class produces an entry that is evicted and
re-resolved on *every* call. The TPE-specific exemption is replaced by asking
the same arbitration once, at population time, for **any** allow-listed
`SyntheticStub`. `synthetic_stub_yields_with_cm` is the `&ClassManager`-taking
half of the existing predicate, split out because the call site already holds
the read guard and a nested `read()` there is a lock-order panic in debug and a
possible deadlock in release — the identical trap the deleted probe documented.

### 6. The gate that replaced the census constant

`every_threadpool_receiver_shape_site_is_gone` (zero call sites, zero
hand-inlined `workers` lookups, across the four files that carried them) and
`threadpool_executor_is_real_protected` (the allow-list entry is still there).
The two are a pair: without the allow-list entry the retagged native wins over
real bytecode for every receiver — the recursion the probes prevented; without
the retag the allow-list entry is inert, because the arbitration only fires for
`NativeKind::SyntheticStub`. The `NativeKind` half is pinned by
`native-builtins/tests/stub_ratchet.rs`, which builds the real boot registry.

The old gate's failure message is preserved in spirit: if a probe grows back,
the test says the fix is to repair whatever reintroduced a synthetic-layout
producer, not to teach one dispatch path to tell receivers apart again.

## Verification

Everything the record's *How to verify a fix* section asked for.

* **Coverage.** `rg 'ThreadPoolExecutor' vm/src | rg 'workers|has_real_workers'`
  → zero dispatch sites. Enforced by the gate above rather than by a grep.
* **The self-recursion guard, the async-degradation guard and the
  cache-poisoning guard** are all three carried by
  `probes/L10ThreadPoolInitProbe`, which L10 built for exactly this list:
  a task that calls `execute()` again from inside a worker (the case that used
  to be a native stack overflow and a **process abort**, not a catchable
  `StackOverflowError`); "the executing thread is not the submitter" asserted
  as an identity rather than a timing measurement, which an inline fallback
  fails and a "did the task run?" assertion does not; and several executor
  shapes alternating through one call site, which a monomorphic inline cache
  gets wrong. Byte-identical to HotSpot 25 in both modes, before and after.
* **The `CompletableFuture` / `ForkJoinPool` route** through
  `spawn_runnable_on_real_thread` — native code calling `.execute()` on the
  singleton async pool via `ctx.invoke_virtual`, the concrete instance of the
  recursion above — is exercised by `JdkOnlyCensusLoadProbe`'s `concurrent`
  section and was checked directly during this change (`supplyAsync`, a
  three-stage `thenApplyAsync` chain, `ForkJoinPool.commonPool().submit`):
  green in both modes, matching HotSpot.
* **`scripts/jdk-only-strict-probes.sh`** (HotSpot + `--real-jdk` + `--jdk-only`,
  one image, one set of class files): `RESULT: PASS`, 5 observed / 5 baselined.
  The first run reported a sixth section, a `[site-alias]` JIT
  address-recycling *diagnostic* line; it did not recur in four further
  A-B-B-A-interleaved runs and the control arm's own observed count varies 4–5,
  so it is load-driven noise, not this change.
* **Class-origin and native censuses, A/B on the same probe:**

  | | BASE (`origin/dev` a7a04421b) | after |
  |---|---|---|
  | registry `bridge` | 10434 | 10432 |
  | registry `synthetic-stub` | 755 | 757 |
  | registry total | 11876 | 11876 |
  | `ThreadPoolExecutor.execute` kind | bridge | synthetic-stub |
  | `--jdk-only` `synthetic_stub_invocations` | 0 | 0 |

  Exactly the two retagged registrations moved, nothing else.
* **Stub ratchet** re-frozen 553 → 555, with the explanation the gate demands:
  no new fake was added; two registrations that were mis-tagged `Bridge` are now
  counted where they belonged, at the moment they stopped being reachable on a
  real-JDK image.

## What was deliberately NOT done

`native-builtins`' own `executor_has_real_workers` (~8 call sites there, plus
the deliberately-separate twin in `native-collections`) is untouched. It is
defence in depth *inside the callee* — a genuinely-real receiver that reaches
the native anyway is re-checked and redirected to real bytecode. The record was
explicit that deleting the dispatch-side probes and their in-callee backstop in
one change removes both the check and its safety net; it remains a separate
cleanup, and it is what makes the synthetic-JDK path safe.

## Blast radius, restated for whoever changes this next

The class-scoped rule is correct **only while every `ThreadPoolExecutor`-tagged
object on a real-JDK image is genuinely real**. If a new producer allocates one
with `alloc_concurrent_synthetic` and does not drive
`initialize_real_thread_pool_executor`, real `execute()` bytecode will
dereference a null `ctl`/`mainLock`. Fix the producer. Reinstating a
receiver-shape probe at one dispatch site recreates the exact defect shape this
record was filed for, and the gate will say so.
