# `System.gc()` under Generational finalized every reachable object — FIXED 2026-09-10

| | |
|---|---|
| **Status** | ✅ **FIXED.** `sweep_young_non_moving` seeded the finalizer candidate list into the same mark bitmap as the real roots — making the two indistinguishable — and then returned `finalizer_addrs.to_vec()`, the whole candidate list, as its "these are dead, run `finalize()`" result. It never asked whether a candidate was reachable. Every caller treats that list as proven-dead and enqueues it on the finalizer thread, so one `System.gc()` finalized every pending finalizable object in the process, reachable or not. |
| **Scope** | `-XX:+UseGenerationalGC` only. Any `System.gc()` (which always routes Generational through this sweep via `explicit_full_gc`), and any cycle diverted to the same sweep by live conservative JIT roots. G1 and ZGC were never affected — both already made the check this sweep was missing, as did Generational's own **moving** (Cheney) young collector. |
| **The fix** | A dedicated finalizer-resurrection phase in `sweep_young_non_moving`, running *after* the root closure and both late-resolution passes, that reports only the candidates the mark did not reach. `mark_young` now returns whether it NEWLY claimed an object, which is that same "was it already marked?" test taken through the identical base resolution and header screening every other mark uses. |

## The evidence, in one table

`probes/ReachableFinalizeProbe.java` — 11 finalizable objects held by a static
field and a live local array, then five `System.gc()` rounds. It reads every
object back afterwards, so the objects are provably intact the whole time
(`sum=1045` on every arm, before and after).

| arm | objects finalized |
|---|---|
| Generational, **before the fix** | **11 of 11** |
| Generational, **after the fix** | **0** |
| G1 / ZGC / HotSpot 25, before and after | 0 |

Eleven strongly reachable, still-readable objects had `finalize()` run on them.
Nothing about the objects was unusual — a static field was enough.

The counter-check matters as much as the fix, because "report nothing as dead"
would also produce a clean first table. `probes/FinalizeOnceProbe.java` drops 20
finalizable objects and collects eight times:

| arm | created | finalized |
|---|---|---|
| Generational, after the fix | 20 | **20** |
| G1 / ZGC / HotSpot 25 | 20 | 20 |

Exactly once each — garbage is still collected and still finalized, and nothing
is finalized twice.

## The three-way asymmetry that named it

Every other implementation of this phase in the tree already asked the question,
and skipped a candidate the ordinary mark had reached:

| collector | the check | site |
|---|---|---|
| Generational, moving (Cheney) Phase 2.5 | `if pointer_map.contains_key(&old_addr) { continue }` | `gc/src/gen_heap.rs` |
| G1 `resurrect_dead_finalizers` | `if pointer_map.contains_key(&old_addr) { continue }` | `gc/src/g1.rs` |
| ZGC resurrection pass | `if self.mark_is_set(addr) { continue }` | `gc/src/zgc.rs` |
| **Generational, non-moving sweep** | **none — returned the whole input list** | `gc/src/gen_heap.rs` |

Three of the four are the same collector family reading the same intent; the
fourth is the one path `System.gc()` actually takes.

## How it was found

From the "Probe 1" row of
`docs/known-issues/netty/pooledbytebufallocatortest-threadlocal-value-never-collected-after-thread-death-20260910.md`
(on `claude/netty-bytebuf-cluster-20260910`), which recorded the anomaly and
deliberately left it open:

> **The Generational "before" cell is an anomaly, not a pass** [...] on the
> UNFIXED binary Generational reported `live=0` while the value was still held
> by a live JNI global root [...] Anyone touching Generational's finalizer
> discovery should start from this row.

That row was reproduced exactly on this branch's own build before any change
(`dev` @ `3e25a17f0`; Temurin 25.0.3+9), and the Generational and G1 figures
match the recorded ones digit-for-digit:

```text
                          measured here (pre-fix)          doc's "before" column
  -XX:+UseGenerationalGC   live=0   0.106 s    1 GC        live=0   0.11 s    1 GC
  -XX:+UseG1GC             live=11  30.01 s  287 GCs       live=11  30.05 s  287 GCs
  -XX:+UseZGC              live=11  30.06 s  199 GCs       live=11  30.02 s  159 GCs
```

**It was not a root-scan gap.** The natural reading of that row — that
Generational's root scan omits `jni_global_refs` on the path feeding finalizer
discovery — is wrong. `vm/src/memory/roots.rs` section 9 is walked through the
shared `collect_roots()`, identically for all three collectors; the root set
Generational's marker sees is the same one G1's and ZGC's do. The divergence is
entirely downstream, in deciding which *candidates* are dead.

`ThreadLocal` and JNI globals turned out to be incidental — they were just how
Probe 1 happened to hold its object. `ReachableFinalizeProbe` (a static field,
no `ThreadLocal`, no JNI, no dead thread) reproduces it identically, which is
what showed the blast radius is every finalizable object rather than one leaked
root.

After the fix, Probe 1's three collectors agree for the first time:

```text
  -XX:+UseGenerationalGC   live=11  30.00 s  279 GCs
  -XX:+UseG1GC             live=11  30.09 s  288 GCs
  -XX:+UseZGC              live=11  30.01 s  171 GCs
```

`live=11` is the correct answer *on this branch*: the `ThreadLocal` value
genuinely is still rooted here, because the unrelated JNI-global-root release
fix (`6b0de190e`) lives only on `claude/netty-bytebuf-cluster-20260910` and has
not landed on `dev`. Generational now agrees with the two collectors that were
always right, instead of finalizing a rooted object.

Note that `6b0de190e` makes this bug *invisible to Probe 1 specifically* without
touching it: once the root is properly released on thread death, "dead by the
correct algorithm" and "dead by the old unconditional answer" coincide. Probe 1
is not a regression test for this defect; `ReachableFinalizeProbe` is.

## Why it survived

`gc/src/gen_heap.rs` had **no finalizer test at all** — the word does not appear
in its test module. The two added with this fix are the first:

* `an_explicit_full_gc_finalizes_only_the_unreachable_candidate` — one rooted
  and one unrooted candidate through a real `request_major_gc()` cycle; asserts
  the rooted one is not reported and the unrooted one still is. Fails against
  the old return value, which contained both.
* `two_mutually_referencing_dead_finalizables_are_both_reported` — pins the
  resurrection drain to *after* the candidate loop. Draining inside it would let
  the first object's closure mark the second and hide it, breaking the invariant
  `reference.rs`'s `mutually_reachable_finalizers_both_enqueued` states.

## Verification

* `cargo test -p cratonvm-gc` — **1886 passed, 0 failed** (incl. the 2 new; all
  86 `reference::` tests green), plus every `gc/tests/` integration binary.
* `bash regression-suite/run.sh`, all three collectors (this change is in shared
  young-sweep machinery, so all three are the relevant surface):

```text
  default (ZGC)           92 passed, 0 failed   92/92 scheduled, 0 coverage errors
  -XX:+UseG1GC            92 passed, 0 failed   92/92 scheduled, 0 coverage errors
  -XX:+UseGenerationalGC  92 passed, 0 failed   92/92 scheduled, 0 coverage errors
```

* All three probes above, on Generational / G1 / ZGC, against HotSpot 25.
* The regression test was confirmed to FAIL against the old return value
  (`finalizer_addrs.to_vec()` restored temporarily): "a finalizable object still
  reachable from a root must NOT be reported dead". A test that passes both
  before and after would guard nothing.

## Known adjacent issue, NOT fixed here

`FinalizerThread::enqueue` refuses an address that has already been *finalized*
(`already_finalized`), but not one currently sitting *unrun in the queue*, and
`finalizable_roots()` deliberately includes `finalizer_thread.pending_addresses()`
so those objects stay rooted. An object that is queued but not yet run can
therefore be enqueued a second time. This predates the fix and is strictly
*reduced* by it — the old code re-reported every candidate on every explicit GC,
where the new one reports only unreachable candidates — and `FinalizeOnceProbe`
measures 20/20 (no double-finalization) on all three collectors. Closing it
properly means teaching the caller to separate "registered" from "already
claimed" candidates, which is a change to `finalizable_roots`'s data flow in
`vm/src/runtime/interpreter/gc_and_alloc.rs`, not to this sweep.
