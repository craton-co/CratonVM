# The reference-store arms on a real application: engaged, and not a win

*2026-09-03. Measured on `origin/dev` at `0860d4dea` — no code change; this
page is the measurement three write-ups in a row said was owed.*

## The debt

`ir-tier-ref-store-and-the-gate-that-was-a-constant-20260902.md` and
`singlepass-ref-store-two-shapes-and-the-fourth-door-20260902.md` both ended
with the same residual: *"a barrier-heavy A/B on a real application is still
owed. This page has a probe and a null result on bt18; neither is a
workload."* Everything those pages measured was `bench/RefStoreLoopProbe` (a
synthetic loop written for the purpose) and `BinTreesClassic`.

The workload here is H2's own test corpus at `C:/craton/h2corpus` —
`org.h2.test.unit.TestCache`, a ~28-second run through H2's cache, MVStore and
JDBC layers, which exits 0 only when its own assertions hold.

## Engagement: yes, comfortably

Both arms reach real application code, on both publishing collectors.
`org.h2.test.store.TestCacheLIRS` (a shorter run, both censuses on):

| tier | sites | executions |
|---|---|---|
| single-pass | `gated=28 declined=0` | `inline=176,914 helper=0` |
| optimizing | `gated=10 declined=0` | `inline=120,074 helper=0` |

~297,000 inline reference stores and not one helper call, on Generational and
ZGC alike. On the larger `TestCache` the site counts are ~340 single-pass and
~155 optimizing per run. So this is not a probe-only feature: it is broadly
engaged on ordinary Java.

## Throughput: no, and slightly negative

`org.h2.test.unit.TestCache` under `-XX:+UseGenerationalGC`, one binary, order
alternated by round, `rc=0` as the oracle:

| round | arms on | arms off |
|---|---|---|
| 1 | 28883 | 28134 |
| 2 | 27086 | 26662 |
| 3 | 29128 | 26708 |
| 4 | 27049 | 26850 |

Four clean pairs, and the arms are **slower in all four** — by 0.7% to 9%.
(Rounds 5 and 6 ran while the host became heavily loaded — 37.5 s and 47.5 s
for the off arm — and are not usable; round 5's on-arm run also segfaulted,
discussed below.)

This does not contradict the probe results, it bounds them. `RefStoreLoopProbe`
is ~3.2x faster with the optimizing-tier arm because it is a loop of almost
nothing but reference stores into young receivers. `TestCache` spends its time
in H2's cache logic, map lookups, file I/O and GC; its ~300k reference stores
are a rounding error against that, so what remains visible is the gate
sequence's own cost on the stores that would not have needed a barrier call
anyway.

**The honest summary is that these arms are worth a great deal on
store-dominated code and slightly negative on an ordinary application**, and
the second half was not known until this page.

## A segfault, unattributed

One of the twelve `TestCache` runs — arms on, round 5, while the host was
loaded — died with `EXCEPTION_ACCESS_VIOLATION`, a read at
`0x000002004C150008`, in `MVStore.close` on the shutdown path, with the crash
report noting *"the faulting thread had an UNREGISTERED JIT frame on its native
stack in the last root-gathering pass"*.

It did not reproduce: 0 failures in 6 further runs with the arms on and 0 in 6
with them off. **It is not attributed to these arms**, and it may well be the
same host-load flakiness that produced the two unusable timing rounds. It is
recorded because one crash is one crash, and because a future occurrence should
find this note rather than start from zero.

While looking for a mechanism, one worth writing down: **every compact
field-access emitter bakes its cell offset with no layout-replace guard.** Both
*allocation* emitters carry one (`emit_inline_tlab_new`,
`emit_inline_tlab_new_ir`) and document why — a replaced layout otherwise
leaves an already-compiled site allocating at the old size, *"confirmed heap
corruption"*. The field-access side —
`emit_inline_compact_getfield`, `emit_inline_body_compact_ref_putfield`,
`emit_inline_fresh_ctor_compact_ref_putfield`, and both gated reference stores
— has no such guard. That exposure predates this week's work, but the gated
arms widen it: they removed the conditions (published store bounds, a null old
value) that previously kept those paths rarely taken. Adding the same
three-instruction guard is cheap and mirrors a proven pattern.

## A pre-existing H2 failure, for the record

`org.h2.test.store.TestCacheLIRS` passes on HotSpot and fails on CratonVM with
`AssertionError: Expected: 0 actual: 5` at `TestCacheLIRS.java:27` — identically
with `CRATONVM_JIT_GATED_REF_STORE=0 CRATONVM_JIT_IR_REF_STORE=0`, so it is
nothing to do with the barrier work. Noted because it is a real-application
correctness defect that this corpus surfaces in three seconds.

## Workloads that run clean

For anyone repeating this, from the same corpus, on the default collector:

| class | wall | rc |
|---|---|---|
| `org.h2.test.unit.TestCache` | 33.6 s | 0 |
| `org.h2.test.unit.TestIntPerfectHash` | 9.1 s | 0 |
| `org.h2.test.store.TestDataUtils` | 6.3 s | 0 |
| `org.h2.test.unit.TestBitStream` | 4.8 s | 0 |
| `org.h2.test.store.TestObjectDataType` | 0.8 s | 0 |

`org.h2.test.store.TestMVStore` and `org.h2.test.store.TestMVRTree` fail on
HotSpot too, so they are no use as oracles.
