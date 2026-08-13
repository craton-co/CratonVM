# The per-call floor is the NATIVE funnel — RETIRED 2026-08-05

| | |
|---|---|
| **Status** | RETIRED — the three items it named are done or answered |
| **Opened** | 2026-08-03, taking on [`aqs-thread-handoff-latency`](aqs-thread-handoff-latency-RETIRED-20260805.md) |
| **Closed by** | `perf/aqs-native-funnel-20260804` |
| **Residual** | one, and it is NOT this doc's: see the AQS closeout's "What is still open" |

The original brief is reproduced at the bottom. Its central measurement stands:
an ordinary Java call is ~8 ns and anything entering `safe_native_call` costs
two orders of magnitude more. Its **item 1 — extend the bypass to the JIT
side, as a class rather than another hand-written case — is what this branch
did**, and it was worth considerably more than the brief expected, because the
brief mis-identified where compiled code's cost actually was.

## What the brief got wrong about the JIT half

> "Compiled code dispatches natives through `vm/src/jit/helpers.rs`, which has
> its own path into `safe_native_call_prevalidated_objects`. Since hot code is
> compiled, that is where the remaining ~400 ns per native lives."

Half right. It **is** in `helpers.rs`, but only a fraction of it is the funnel.
A compiled native call reaches `jit_invoke_dispatch`, whose tail is
`vm_exec::invoke_or_native` — a ~27-gate cascade of hand-written
`method_name == "…"` / `effective_class == "…"` tests, then a three-string
registry hash, then `real_protected_stub_class`, then
`resolve_native_dispatch_wave1`, then `check_native_dispatch_capability`, and
only then the funnel. Resolving that once per call site instead of once per
call is most of the win below; `safe_native_call_leaf` is the rest.

And there were **two** entry points, not one. That cost a build cycle and is
the most reusable thing here:

> The x64 backend emits `jit_invoke_virtual_mic` for `invokevirtual` /
> `invokeinterface` and reserves `jit_invoke_dispatch` for static/special sites
> and bailouts.

Hooking only the dispatcher left every *instance* native — the whole
`AtomicInteger`/`AtomicLong` accessor set, i.e. the AQS doc's item 2 — still
paying the full funnel. `Math.abs` (static) went 330 → 84 ns in that build
while `AtomicInteger.get` sat at 1026 → 926, i.e. unmoved. Both hooks are in
now.

## The mechanism

A registration-level predicate, which is what the brief asked for
("that predicate wants to live on the registration … not in a growing `match`
in `helpers.rs`"):

* **`NativeMethodRegistry::set_leaf`** — a scoped ambient claim captured into
  the slot beside `NativeKind`. Its doc states the four-part contract: no
  Java-heap allocation, no safepoint or block, no collection, no JNI-pending
  exception. It rides on the **callback**, not the triple, so a later phase
  that re-registers a triple without opting in demotes it back to the funnel.
  That is not decoration — `native-builtins` re-registers the atomics three
  times, with phase 54's buggy versions in between, and a
  `mark_leaf(class, method, descriptor)` post-pass would have kept asserting
  the contract about whichever body won the slot last. Pinned by
  `a_re_registration_that_does_not_opt_in_drops_the_leaf_claim`.
* **`safe_native_call_leaf`** — the funnel minus argument pinning, the STW
  probe, both GC-pressure hooks, the two `record_transition`s, the JNI drain
  and the pin ring. It keeps the argument/return forwarding barrier and
  `catch_unwind`. The table in its doc comment says why each dropped step is
  dead for a leaf.
* **`LeafNativeDispatchCache`** — per-JIT-call-site resolution, generation-keyed
  on the native registry so a lazily-registered native is still seen. Every
  obligation is discharged once at fill time: invoke kind, the
  `invoke_or_native` special cases, `SyntheticStub` arbitration, the capability
  policy, `--jdk-only` admission. Only the receiver-class guard is re-tested
  per hit.
* **`Thread.currentThread`** gets its own arm and is **not** marked leaf: its
  registered body allocates the thread mirror on first call. Compiled code
  serves the warm case from `java_thread_obj` and falls through otherwise,
  exactly as the interpreter already did.

The leaf set is audited one entry at a time, and what is *excluded* is the
interesting part: the CAS and fetch-add members of the atomics take the monitor
table's per-object CAS lock, which contract item 2 forbids. `String.isEmpty`
is excluded too — it decodes the whole string through `ctx.read_string`, which
breaks no rule but is not the cheap thing the claim would advertise.

## The measurement

`probes/NativeShapeProbe.java`, real-JDK mode, JIT on, last of four passes.
**A-B-B-A interleaved**, so no arm sits in one stretch of host load — which
mattered, because this host drifted by ~2x during the session.

| rung | base | base | **fixed** | **fixed** | HotSpot |
|---|---:|---:|---:|---:|---:|
| control: no call | 1.2 | 0.9 | 1.1 | 1.5 | 0.4 |
| `Math.abs(int)` | 426 | 380 | **116** | **122** | 0.8 |
| `String.length()` | 1056 | 530 | **351** | **328** | 0.2 |
| `System.nanoTime()` | 518 | 289 | **80** | **138** | 32.3 |
| `Thread.currentThread()` | 479 | 485 | **61** | **47** | 0.1 |
| `AtomicInteger.get()` | 672 | 959 | **425** | **183** | 0.4 |

Every fixed run beats every baseline run on every leaf rung, in both orders.
On the quietest single pair of runs the ratios are 4.6x / 5.0x / 3.9x / 10.5x /
4.4x respectively.

The two rungs that are **not** leaf-registered are the control, and they do not
move: `System.identityHashCode` 654 → 555 and `AtomicInteger.compareAndSet`
1061 → 1161, both inside this host's drift.

### The acceptance criterion is the counter, not the ns/op

`CRATONVM_DBG=intrinsic-stats` now reports compiled leaf dispatches beside the
interpreter's own counter, and prints a per-reason tally of the sites the fast
path declined. The final run: **39,972,036 compiled leaf-native dispatches** —
five rungs at 2M rounds x 4 passes — and two refusals.

Both numbers earned their place the hard way:

1. The first build measured **0 hits** with the ns/op essentially unchanged.
   The cause was a blanket `if capabilities().is_some() { refuse }`: `vm_init`
   installs a capability policy **unconditionally** (default `Permissive`, so
   `capability_audit` can report), so that gate refused every site in every
   configuration. Timings alone said "no faster"; only the counter said "never
   ran". The gate is now `classify_native(class, method).is_some()`, which is
   the same question the dispatch-site gate actually asks first.
2. The second build measured 24M hits — and `AtomicInteger.get` still unmoved,
   with **no** refusal recorded for it. That is what pointed at the second
   entry point: the site was not being refused, it was never arriving. Every
   bail is counted now, including the pre-resolution ones, so a future zero is
   never ambiguous again.

## Item 2 — reduce the fixed funnel cost — answered, not done

The brief said "~180-330 ns for zero arguments is ~1000 cycles of bookkeeping,
most of it diagnostics that are off by default. Nobody has profiled it."

Nobody profiled it here either, and it is now much less interesting: ARCH-A3
had already collapsed the funnel's thirteen diagnostic gates into one word
(`native_diag_mask`), so on a diagnostics-off run the remaining cost is pinning,
the STW/GC probes, two thread-state transitions and `catch_unwind` — and for the
bodies where that dominated, the leaf path removes all of it rather than
shaving it. What is left on the funnel is the population that genuinely needs
it. Trimming that further is a real but separate piece of work, and it should
start from a profile, not from this document's assertion.

## Item 3 — `Unsafe.compareAndSetInt`

Untouched, and correctly so: it is capability-classified (`RawMemory`), so the
leaf path refuses it by construction, and it must reach a real CAS. Its 808 ns
is still mostly funnel. Left open deliberately.

---

## Original brief, 2026-08-03

<details>
<summary>Reproduced verbatim.</summary>

The brief's tables and reasoning are preserved in git history at
`docs/known-issues/vm/native-call-funnel-is-the-per-call-floor-20260803.md`
(removed by the branch that produced this closeout). Its measured shape —
ordinary Java call 8.4 ns, interpreter-answered `Thread.onSpinWait` 4.4 ns,
anything entering `safe_native_call` 180-810 ns, with a per-argument slope of
~100-150 ns — is unchanged by this work and remains the reason the leaf
mechanism exists.

Its `LockNativeCensusProbe` result also stands: an uncontended `ReentrantLock`
`lock()`+`unlock()` pair makes exactly five native calls —
`Thread.currentThread()` x2, `AbstractOwnableSynchronizer
.setExclusiveOwnerThread(Thread)` x2, `jdk/internal/misc/Unsafe
.compareAndSetInt` x1. What did **not** survive is the inference drawn from it;
see the AQS closeout.

</details>
