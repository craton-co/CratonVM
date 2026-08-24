# A registered native that ALWAYS yields to real bytecode still cost its method the JIT, its callers the seal, and every call the name-keyed dispatch tail

**Status: FIXED 2026-08-22 on `perf/webclient-reactive-20260821`, branched from
`dev` `651fa3256`.** Five gates across `dispatch_virtual.rs`, `jit_bridge.rs`
and `interpreter.rs` used `native_methods.find(class, name, desc).is_some()` as
a stand-in for *"a native runs here, so the bytecode must not be compiled or
cached"*. For the twelve classes on `real_protected_stub_class_common`'s
allow-list that stand-in is **always wrong**: their `SyntheticStub` natives lose
the arbitration to the real JDK body at every dispatch site, so nothing was
being protected and the method, its callers and its call sites all paid.

Found while characterising
`internal/performance/webclient-integration-tests-reactive-exchange-gap-RETIRED-20260823.md`.
It is **not** that page's cause — see "What this did not fix" below, which is
the part worth reading before spending time here again.

## The measurement that names it

Two methods of the SAME class, 200k-iteration loops each in its own method so
invocation-count tier-up applies, HotSpot 25 control alongside:

| call | before | after | HotSpot |
|---|---:|---:|---:|
| `Instant.getNano()` — native registered, always yielded | 1386 ns | **24.0 ns** | 2.3 ns |
| `Instant.compareTo()` — same class, **no** native | 39 ns | 39 ns | 2.9 ns |
| `Instant.isBefore()` | 1827 ns | **31.2 ns** | 6.1 ns |
| `Instant.now()` (static) | 3780 ns | **511 ns** | 34.8 ns |
| `AtomicBoolean.get()` | 1442 ns | **24.8 ns** | 3.4 ns |
| `AtomicBoolean.compareAndSet()` | 4049 ns | **243 ns** | 5.5 ns |
| `StringJoiner.length()` | 2140 ns | **30.0 ns** | 5.2 ns |
| control — `Duration.getSeconds()`, no native | 24.0 ns | 24.4 ns | 2.3 ns |
| control — `ArrayList.size()`, native that DOES run | 351 ns | 352 ns | 2.3 ns |

`getNano` and `compareTo` are both one-line accessors on `java.time.Instant`.
The only difference between 1386 ns and 39 ns is that one of them has a
registered native — a native which, on a real-JDK image, never executes.
`probes/ShadowProbe2.java` is the arm set; ABBA-interleaved, both controls flat
across every round, which is what says the change is scoped.

## The chain

`java/time/Instant` is on `real_protected_stub_class_common`'s allow-list, and
its sixteen natives are tagged `SyntheticStub`. `--dump-native-registry`
confirms the outcome directly: all sixteen rows read
`kind=synthetic-stub owns_slot=true` and **`invocations=0`** after a run that
called them hundreds of thousands of times. They never run. What ran instead:

1. **The method never compiles.** Four JIT gates (`try_jit_compile_callee_slow`,
   `try_jit_upgrade_with_gate`, the OSR entry gate, and the first-call gate in
   `interpreter.rs`) each refuse a method with a registered native. So
   `Instant.getNano` is not merely uncompiled — it is never *tracked*:
   `CRATONVM_DBG=jit-method-stats` reported `1 distinct methods tracked, 0 ever
   invoked` for a run that executed it 500 000 times.
2. **Its callers get sealed too.** `jit_invoke_targets_native_shadow` scans a
   prospective caller's bytecode and seals it if it calls a native-shadowed
   target, counted as `calls-native-shadowed-method` in the skip-seal census.
3. **So the inline cache has nothing to publish.** `CRATONVM_DBG=mic-prof` on
   the `getNano` loop: `mic_calls=506000 hit_entry=0 hit_noentry=505996
   pub_published=0`, with `cyc_invoke` at 89% of `cyc_mic_total`. Every entryless
   hit falls into the `invoke_or_native` tail.
4. **Which resolves the callee by NAME, per call.**
   `CRATONVM_DBG=dispatch-tally` over 1 048 576 records:
   `521954 invoke_or_native java/time/Instant.getNano()I` and
   `521953 invoke_on_class_shared_inner .getNano()I` — i.e. **every** call, with
   `invoke_on_class_shared_inner` the top symbol in `perf` at 14.7% self.
5. **And the arbitration itself re-ran per call.** `CRATONVM_DBG_STUB_YIELD=1`
   over 11 000 `getNano()` calls logged **7 003** `yield=true — real bytecode
   wins` lines: a class-manager read lock, a name-keyed class lookup and a
   `find_method_recursive` walk, uncached, on ~64% of calls.

The interpreter alone was never the problem — with `--nojit` the same loop runs
at ~250-430 ns and `dispatch-tally` records no generic-dispatch traffic at all,
because `execute_invokevirtual_vtable_fast` and the populate path serve it. The
cost appears only once the CALLER is compiled, which is exactly when it should
have got faster.

## The fix

One predicate, `native_override::registered_native_will_run`, and five call
sites that now ask it instead of `find(..).is_some()`:

* `dispatch_virtual.rs` — the vtable fast path's `direct_native_shadow` probe
  and its parent-chain twin. Both used to `remember_vtable_native_shadow(..,
  true)` and return `CacheMiss`, and that memo is **permanent for the call
  site**, so the site could never warm up.
* `jit_bridge.rs` — `jit_invoke_targets_native_shadow`'s `direct` and
  `inherited` arms (the caller seal), `try_jit_upgrade_with_gate`, the OSR entry
  gate, and `try_jit_compile_callee_slow`.
* `interpreter.rs` — the first-call compile path's `native_skip`.

The two dispatch_virtual sites spell the predicate out locally rather than
calling the shared helper, because they run **under** the class-manager read
guard their caller already holds and the shared helper takes its own `read()` —
the nested-read writer-starvation trap `synthetic_stub_yields_with_cm` was split
out to avoid in the first place.

**Why relaxing a correctness gate is sound here.** Every one of those gates
exists because "a compiled direct call bypasses the interpreter's
native-vs-bytecode decision". That is precisely the premise the new predicate
checks: when the arbitration says the real bytecode wins, compiling that
bytecode *is* the interpreter's decision, not a bypass of it. The relaxation is
also one-way: every term `synthetic_stub_yields_with_cm` reads is monotone in
the direction that matters (a class becomes loaded, a `Code` attribute becomes
decoded), so a `yield = true` verdict cannot revert mid-process; the one thing
that could revert it, a JVMTI redefine, already quiesces and invalidates
compiled code through `any_class_redefined` / `JitCache::invalidate_matching`.

A second, independent change rides along in `InvokeCache::get`
(`classloading/src/resolution.rs`): it hashed the same key **twice** per hit —
once to test staleness through a borrow it dropped, once to return the value —
and it is the interpreter's most frequent map lookup (2.65-2.95% of all samples,
ahead of `execute_frame_from_index`). The staleness `remove` it did was an
optimisation, not a correctness term: a stale entry is still reported as a miss,
and the miss sends the site down the slow path to `populate_invoke_cache` ->
`put`, which overwrites the same key. One probe now, self-healing on the next
call.

## What this did not fix, and the trap in the sizing

`WebClientIntegrationTests` itself **did not move**: ABBA-interleaved, four runs
each, `29.6 s` CPU before and `29.9 s` after. Do not read the table at the top
as a suite result.

The mis-sizing is worth recording because it is easy to repeat. `Instant.now()`
is genuinely the hottest method in a `WebClientIntegrationTests` run —
`1 240 308` invocations, 475x the next entry — and priced from the
JIT-compiled loop above at ~7 µs it "accounts for" 8.7 s of a 19 s run. It does
not. In the suite that code is reached from **interpreted** callers, where it
costs ~250-500 ns, i.e. ~0.3-0.6 s, and HotSpot pays the same 1.24 M calls at
43 ns. **A per-call cost measured from a tight JIT-compiled loop is not the cost
that workload pays**; check which door the callers came through
([[a-benchmark-loop-in-main-measures-interpreted-code]] is the sibling trap)
before multiplying by an invocation count.

The fix's real reach is any workload where these classes are called from
compiled code — `java.time` in logging/metrics/HTTP-date paths,
`AtomicBoolean`, `StringJoiner`, `ThreadPoolExecutor`, `LinkedBlockingDeque`,
`EnumSet`, `FileInputStream`, `Cleaner`. `ReentrantLock` and
`LinkedBlockingDeque` did *not* move on the probe and that is correct: the
registry dump shows they have **no** registered natives at all, so their own
~1600 ns is a different defect.

## Verification

* `cargo test --release -p cratonvm-classloading --lib` — 797 passed, 0 failed.
* `cargo test --release -p cratonvm-vm --lib`, `-p cratonvm-jit --lib` — see the
  branch's commit message for counts.
* `WebClientIntegrationTests` — `169/170 succ, 0 fail, 1 skip` on the fixed
  binary, same as the base binary's good runs. The class has a pre-existing
  1-3 test flake band (`postLargeTextFile [1] Reactor Netty` and friends) that
  is present identically on both binaries and on `dev`.
* Both probe controls (`Duration.getSeconds`, `ArrayList.size`) flat to within
  1 ns across every interleaved round.
