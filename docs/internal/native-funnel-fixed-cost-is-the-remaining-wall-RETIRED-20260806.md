# The native funnel, profiled — and it was the dispatch path around it

| | |
|---|---|
| **Status** | RETIRED — profiled per step, six terms removed, the leaf question answered NO with a reason |
| **Retires** | `native-funnel-fixed-cost-is-the-remaining-wall-20260805.md` |
| **Companion** | [`native-call-funnel-per-call-floor-item2-20260805.md`](native-call-funnel-per-call-floor-item2-20260805.md), which profiled the funnel *body* |
| **Landed** | 2026-08-06, `claude/native-funnel-fixed-cost-20260806` |

The retired record's one demand was explicit:

> **Nobody has profiled it. This document asserts a bound, not a line.** Anyone
> taking it on should produce a per-line or per-step attribution first, and
> should expect the answer to be unevenly distributed.

It is unevenly distributed, and none of the three terms the record named as
"each plausible" — the two `record_transition` calls, `catch_unwind`'s landing
pads, the pin push/truncate — is where the time was. Two of them had already
been priced at 1-12 ns by the companion record. The unmeasured half was
everything `jit_invoke_dispatch` does *around* the funnel, and that is where
five of the six terms below live.

## The instrument

`vm/src/jit/helpers.rs`, module `jit_native_dispatch_profile`, `#[ignore]`d
because it is a measurement and a timing threshold here would be a flake on a
shared host:

```bash
cargo test --release -p cratonvm-vm --lib jit_native_dispatch -- --ignored --nocapture
```

It drives each step of a compiled-code native call separately against a bare
`SharedVm` with a real heap object as the receiver. Its sibling
`vm/src/vm/vm_exec.rs::native_funnel_profile` does the same for the funnel body;
together they account for a compiled native call end to end.

## The profile, before

Last of four passes, Azure host, `AtomicInteger.get()`-shaped site (instance
receiver, no arguments) unless noted:

| step | ns | |
|---|---:|---|
| `decode_dispatch_values` — 1 receiver | **20.2** | of which `SmallVec::with_capacity` was 10.3 |
| `decode_dispatch_values` — receiver + 1 object | **25.6** | |
| `safe_native_call_prevalidated_objects` — 1 object | 24.6 | the funnel body; the companion record's number |
| `safe_native_call_leaf` — 1 object | 16.3 | |
| `set_jmx_owned_synchronizer` | **22.7** | per `setExclusiveOwnerThread`; 45.4 for an acquire+release pair |
| `forward_jit_reference_args` — 1 receiver | 6.0 | 13.0 for receiver + 1 object |
| `heap.class_id_of` | **5.3** | 4.7 of it a *second* `is_object_address` |
| `heap.is_object_address` | 3.5 | and it ran **three times** per instance native call |
| `heap.load_and_forward` | 3.5 | |
| `note_site_identity` — switched OFF | 2.2 | |
| `class_was_redefined` + `class_id_or_name_was_redefined` | 2.6 | one atomic load each, twice |
| `NATIVE_SITE_CACHE` probe (hit) | 0.9 | the `FxHashMap` the record suspected |
| `jit_thread_mut`, `jit_site_key`, `generation()`, `native_site_cache_enabled` | 0.2 each | |
| `note_jit_boundary`, `record_invocation`, `flush_raw_entry_dispatch_caches` | 0.0-0.2 | |

Two entries the retired record singled out are at the bottom of that table. The
site-cache probe it called out by name — "the site-cache probe (`FxHashMap` on a
2-word key)" — is **0.9 ns**. The 233 ns it attributed to the leaf path was
never in the cache.

## What was actually paying

### 1. The receiver's header was conservatively validated three times

`VmHeap::class_id_of` opens with a full `is_object_address` — region
containment, kind and element tags, header plausibility, and an
extent-fits-the-arena re-scan of the region table. That guard is there for a
reason (`KINDOF-SENTINEL`: a `0xFFFF…FFFF` reached the dispatch unchecked), and
3.5 ns is nothing against a corrupted read. It is everything when it is the
third time the same address has been through it in one call:

1. `try_jit_site_cached_native_dispatch` validates the receiver to guard the site;
2. it then calls `class_id_of`, which validates it again;
3. `decode_dispatch_values` validates it a third time, from the raw word.

`VmHeap::class_id_of_validated` is `class_id_of` for a caller that still holds
the `ObjectRef` `is_object_address` just returned — **5.3 → 0.6 ns** — and the
resolved receiver is now threaded into the decode instead of the raw word. One
validation per call. Five other `is_object_address`-then-`class_id_of` pairs on
JIT dispatch probes were collapsed the same way.

### 2. `SmallVec::with_capacity` was half the argument decode

`JitDecodedArgs` is a `SmallVec<[Value; 8]>` — eight 16-byte `Value`s plus a
capacity word, ~144 bytes. The zero-argument rung measured **10.5 ns** with no
argument work to do at all.

The first guess was the by-value return. It was wrong, and the second run said
so: building the buffer with `new()` in a caller-owned local took the same rung
to **0.2 ns** with the return still by value. `with_capacity` is an outlined
call, so the whole value has to be materialised in memory for it; `new()`
inlines and a decode this small stays in registers. Recorded because the
by-value shape reads like the expensive part and is not.

A first cut also hoisted the buffer into a per-thread take/give-back `Cell`.
That was reverted from the same measurement: `take + give_back` is **4.0 ns**,
against a 5.7 ns decode. It was solving the problem that did not exist.

Decode, after: **20.2 → 5.7 ns** for an instance site, **25.6 → 10.4 ns** with a
second object argument, **10.5 → 0.2 ns** for a static one.

### 3. The funnel copied its arguments whether or not any had moved

`safe_native_call_impl` and `safe_native_call_leaf` both opened by copying every
argument into an `[Value; 8]` scratch buffer so the GC-forwarding barrier could
rewrite it in place. A collection between the frame read and the funnel is the
exceptional case; the ordinary one is that every object argument forwards to
itself and the copy is discarded unchanged. The companion record measured the
two scratch arrays at 7.3 ns against a ~23 ns funnel.

Both now ask first and copy only from the first argument that actually moved.

### 4. The per-argument root-index array served a branch that never runs

The funnel kept an `[Option<usize>; 8]` — 128 bytes of stores per native call —
recording which pin index each argument took. It is read in exactly one place:
the remap-arguments-after-GC rebuild, reachable only when one of the three GC
hooks between the two points actually collected. It is now a `u32` bitmask, and
the rebuild reconstructs each index by counting set bits.

### 5. Two redefinition gates, two identical atomic loads

`class_id_or_name_was_redefined` and `class_was_redefined` both open with the
process-global `any_class_redefined()` and answer `false` for every run in which
nothing was ever redefined. One hoisted test now covers both.

### 6. A diagnostic that was 2.2 ns switched off

`note_site_identity` (`CRATONVM_DBG_SITE_ALIAS`) held its own gate, so an all-off
run still made the call. The gate moved to the call site and the function is
`#[cold] #[inline(never)]`.

## The leaf question, answered: NO — and the record's own fix does not change it

The retired record left this open deliberately and asked for it to be settled
rather than assumed:

> Whether that is enough to satisfy the contract, or whether the contract should
> distinguish "blocks on something a safepoint can hold" from "takes a short
> internal mutex", is the design question and it should be settled explicitly
> rather than by relaxing the wording.

The contract does not need relaxing, and the distinction the record offers is
already in it. `NativeMethodRegistry::set_leaf` item 2 reads "no lock on a
structure another thread holds across a safepoint" — which is precisely the
"blocks on something a safepoint can hold" side of that distinction, already
excluded, already for this reason.

So the question is a factual one: does anything hold these structures across a
safepoint? It does, and it is the collector:

* `thread_registry.rs:1935` — `collect_all_root_snapshots` takes `threads.read()`
  and every entry's `jmx_locked_synchronizers` mutex to scan them as roots.
* `thread_registry.rs:2017` — `update_thread_objs_after_gc` takes the same two
  plus `synchronizer_owner`, to rewrite the addresses a moving collection
  relocated and rekey the index by them.

A leaf runs with the thread still recorded as `JavaRunning` — that is what
skipping the two `record_transition` calls means — so the STW census **waits**
for it to reach a safepoint. A leaf that blocks on a lock the collector is
holding, while the collector waits for that leaf to safepoint, is the deadlock
item 2 exists to prevent. All three of `setExclusiveOwnerThread`'s locks are of
that kind.

The record's suggested fix — "giving `JvmThread` a direct
`Arc<Mutex<Vec<ObjectRef>>>` handle to its own entry's synchronizer list" —
removes the registry lookup but leaves exactly the mutex the collector takes at
`:2017`. It does not move the answer.

The same reasoning disposes of the `AtomicInteger`/`AtomicLong` CAS and
fetch-add family, which the existing leaf claims already exclude by hand:
`compare_and_swap_field` takes `monitors.with_cas_lock` and
`cratonvm_gc::collector::volatile_stripe_lock`, and the second is the
collector's own.

**What the handle IS worth, on its own merits.** `set_jmx_owned_synchronizer`
measured 22.7 ns per `setExclusiveOwnerThread` call — 45.4 ns per uncontended
lock/unlock pair — for a global mutex, a registry-wide `RwLock` read, a
`ThreadId` hash, and a per-thread mutex. Both transitions AQS performs (acquire
passing this thread, release passing null) change only the calling thread's own
list, so `ThreadRegistry::set_jmx_owned_synchronizer_own` takes the handle and
skips the `RwLock` and the hash. A genuine cross-thread steal still takes the
registry route for the peer's list, so the recorded state is identical either
way — pinned by a six-case table test over every (previous owner, new owner)
combination, including the two AQS never generates, because those are the arms
where a divergence would silently name two owners for one lock.

## The end-to-end A/B, and what this host can and cannot resolve

Both arms built from the same fork point (`841e9399b`) with
`CARGO_PROFILE_RELEASE_LTO=off`: fat LTO + `codegen-units=1` makes this binary a
~30-minute, 3 GB-RSS link, five other sessions were doing the same link, and the
first attempt was killed by its own timeout mid-LTO. Both arms get the same
profile, so the comparison holds and only the absolute numbers move.

`ab-natfunnel.sh NativeShapeProbe A B 10` — A-B-B-A per round, ten rounds, so
**20 observations per arm per rung**. Median as the headline, and the four rungs
this branch does not touch as the calibration:

| rung | A med | B med | **B/A** |
|---|---:|---:|---:|
| control: no call | 0.8 | 0.8 | **1.00** ← untouched |
| `Thread.onSpinWait` — interpreter-answered | 4.7 | 4.3 | **0.94** ← untouched |
| `String.length()` — intrinsic | 23.9 | 23.5 | **0.98** ← untouched |
| `Thread.currentThread` — direct-bound | 11.2 | 11.1 | **0.98** ← untouched |
| `Math.abs(int)` | 89.0 | 72.5 | **0.82** |
| `System.nanoTime` — static, no arg | 107.5 | 95.1 | **0.88** |
| `System.identityHashCode` — static, 1 object arg | 241.2 | 192.6 | **0.80** |
| `AtomicInteger.get` — leaf, instance | 225.6 | 183.2 | **0.81** |
| `AtomicInteger.CAS` — non-leaf, instance | 408.9 | 329.0 | **0.80** |

The untouched rungs span **0.94-1.00**. That is this run's noise floor, and every
touched rung sits clear of it at 0.80-0.88. For scale, the companion record's A/B
had `String.length()` — also untouched — moving 1.54x, which is why it could not
rest any claim on its table and this one can.

### Why the median, and what a four-round run got wrong

The first attempt ran four rounds and reported minima, on the reasoning that
every error source on a shared host is additive so the smallest observation is
closest to the truth. That is right about the estimator and wrong about the
sample size, in two opposite ways, and both are worth recording because the
run *looked* clean:

* **At 4 rounds the minimum was undersampled.** Printing the full observation
  list rather than a summary statistic showed the samples were *bimodal* — a few
  from a quiet window, the rest from a loaded one — and that arm B drew 2 quiet
  samples to arm A's 3 on **every** rung. Not luck: A-B-B-A puts B in the middle
  of each round and the middle was the loaded part, so the bias is systematic and
  against B. `identityHashCode` then read as a **1.12 regression** off arm B's
  single quiet sample. It is 0.80 here.
* **At 10 rounds the minimum is over-sampled.** With 20 draws, one fluke-quiet
  window sets the minimum: arm B's `Atomic.CAS` minimum is 161.6 against a
  299.7-374 bulk, and its `identityHashCode` minimum is 101.0 against 181-230.
  Ratios of minima report those flukes as 0.47 and 0.50. The medians — 0.80 and
  0.80 — are what the change actually did.

So the guard that matters is not a choice of statistic but the habit of printing
the raw per-arm observation list and calibrating against rungs the change cannot
have touched. `ab4.py` does both.

The in-process per-step deltas remain the mechanism for each individual term
(arms in one process, one VM, four passes, flat across passes); this table is the
independent end-to-end confirmation that they add up.

## Suites

| | |
|---|---|
| `cargo test --release -p cratonvm-vm --lib` | 2419 / 0 |
| `cargo test --release -p cratonvm-gc --lib` | 972 / 0 |
| `cargo check --workspace --features synthetic-jdk` | rc=0 |
| `cargo check --all-targets --features synthetic-jdk -p cratonvm-vm -p cratonvm-native-builtins` | rc=0 |
| `cargo test -p cratonvm-native-builtins --lib --features synthetic-jdk` | 3459 / 0 |
| `cargo test -p cratonvm-vm --lib --features synthetic-jdk` | 3941 / **1** |

That one is `vm::tests::logger_get_and_info`, and it is **not this change**. It
is the same single failure, with the same symptom (`Logger.getName` returns a
non-string), that
[`preconditions-ignores-the-exception-formatter-FIXED-20260805.md`](preconditions-ignores-the-exception-formatter-FIXED-20260805.md)
recorded at 3934/1 the day before, having verified it against a pristine
`origin/dev` worktree.

Re-verified here rather than inherited, because a differential recorded against
someone else's tip is evidence about their tip. A pristine detached worktree at
**this branch's own fork point** (`841e9399b`, `git status` clean) fails the same
single test with the same panic, at the same line:

```
thread 'vm::tests::logger_get_and_info' panicked at vm/src/vm.rs:31970:17:
expected string
test result: FAILED. 0 passed; 1 failed; 4053 filtered out
```

Two independent branches, two pristine differentials, one pre-existing failure.
See also `synthetic-jdk-vm-gate-red-on-dev-20260805.md`.

### After merging `origin/dev` (59 commits, `83af58878`)

Re-run on the merged tree, because a merge is its own experiment and "both halves
passed alone" is not a result about the combination:

| | |
|---|---|
| `cargo check --workspace` | rc=0 |
| `cargo test --release -p cratonvm-vm --lib` | 2428 / 0 |
| `cargo test --release -p cratonvm-gc --lib` | 977 / 0 |
| `cargo test -p cratonvm-vm --lib --features synthetic-jdk` | 3949 / **2** |

The count went from 1 to 2: `vm::tests::preferences_name_and_path` joined
(`assertion failed: tss.contains("myNode")`, `vm.rs:54454`). It arrived with the
59 commits, and both were checked rather than argued about — a pristine detached
worktree at exactly `83af58878` fails **both**, 0 passed / 2 failed. So neither
is this change and neither is an interaction with it; the gate is red on that dev
tip for two reasons that predate this branch.

## The two inherited items were already closed

The retired record's header claims to inherit
`TestAsyncMessagesPerformance.testAsyncTiming` and "the `SmokeTests`
concurrency ceiling as a suspected relative". Neither is a live inheritance,
and nothing here should be read as taking them on:

* `SmokeTests#testQueryConcurrency` was **RETIRED 2026-08-05** —
  `internal/fixed-suite-bugs/hibernate/smoketests-concurrent-query-throughput-20260723-RETIRED.md`,
  which re-measured the ratio at ~22x rather than 38.6x and found the gap needed
  ~1.3x, not 5.2x. `known-issues/hibernate/README.md` records the strike-through.
* `TestAsyncMessagesPerformance.testAsyncTiming`'s own records are closed in
  `internal/tomcat/` and `internal/fixed-suite-bugs/tomcat/`
  (`websocket-async-send-interframe-latency-CLOSED-20260803.md`,
  `32-doc04-residual-perf-assertions-CLOSED.md`).

Both were closed at or before the moment the record was written, so the
"Inherits" line was stale on both halves the day it was filed. Recorded here so
the next reader does not re-open two closed pages on the strength of it.

## What is left

* **`forward_jit_reference_args` forwards, then the funnel forwards again.**
  3.5 ns per object argument, paid twice, with no safepoint between the two —
  the site-cache probe and a hash lookup cannot collect. Extending
  `safe_native_call_prevalidated_objects`'s contract from "already
  heap-validated" to "already forwarded" would close it. Not done here: the
  argument-forwarding barrier has a bug history (`cce0079`) and the funnel is
  shared with paths whose arguments come straight off an operand stack, so it
  wants its own change and its own audit of every caller.
* **`is_object_address` itself, at 3.5 ns.** Two passes over the region table
  with `Acquire` loads, the second only to confirm the extent fits the arena it
  matched. A caller holding a pointer the JIT ABI already guarantees is a
  descriptor-declared live reference is paying for a conservative-root
  validator. Worth a cheaper "validate a known object start" entry point — but
  that is a GC-side change and the sentinel history above is the reason it was
  not attempted opportunistically.
* **The remaining absolute gap is not the funnel.** An ordinary Java instance
  call is ~15 ns on this host against HotSpot's ~0.5, and an uncontended
  `ReentrantLock` pair is ~1,200 ns against 10.3. After this change the
  per-native-call overhead is a minority of that pair. The next question is the
  16 nested Java calls and the interpreter/JIT call floor, not the native
  boundary — and `AqsAttributionProbe`'s `UNATTRIBUTED` line is where to start,
  not the censused parts.
