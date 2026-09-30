# Proposal: stop waiting for the coverage slot once nothing per-pause is shared

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 41
> of 54).** Not built. d7 has no `orphan_coverage_writes` reading (no row ran
> `CRATONVM_DBG_STW_CENSUS=1`). **Gate:** `orphan_coverage_writes=0` on
> `MtChurnProbe`, `ConcurrencyUnderGcSweep`, `G1ChurnPauseProbe`, and the
> two-VM `COVERAGE_SLOT_WAITS` count. **Size:** S.

Status: PROPOSAL (filed gc-common round 2026-09-23, wave 21, lane E)
Area: VM <-> GC protocol, multi-VM processes (`libcratonvm` embedders, the
`vm` unit-test binary)

## Where it stands

`vm/src/threading/gc_barrier.rs` `COVERAGE_SLOT_OWNER` (gc-common w6-a)
serialises stop-the-world pauses ACROSS VMs. `GcBarrier::take_coverage_slot`
makes a request of VM B wait for VM A's pause to reach `complete_gc`. The wait
spins, yields, then sleeps in 100 µs steps, up to `COVERAGE_SLOT_WAIT_LIMIT` =
250 ms. The slot existed because the per-pause coverage rows in
`gc/src/gc_quiescence.rs` were process statics that every accepted request
reset, so B's request erased A's verdict, pins and ledger mid-pause.

As of gc-common w21-e no row the slot was introduced for is a process static
of any VM's pause:

- w8-d: proven and pinned depths, take-over times;
- w9-f: take-over counts;
- w10-g: take-over verdict;
- w18-c: peer stack and register captures;
- w21-e: coverage verdict, reasons, un-rewritable flag, scan count and
  helper-window pins, in `CoverageCycle`.

Each lives in its VM's `PauseLedger`. What the slot still serialises:

1. **The orphan coverage rows** (`gc_quiescence::ORPHAN_COVERAGE`). These are
   writes from threads bound to no ledger, shared across VMs exactly as before
   w21-e. Until `handoff-w21e-bind-the-safepoint-root-deposit` is applied, a
   parking peer's root deposit writes there. After it, only a collection that
   runs with no barrier request should. `coverage_orphan_writes()`
   (`orphan_coverage_writes=` on the `[stw-ttsp]` line under
   `CRATONVM_DBG_STW_CENSUS=1`) measures this.
2. **The young pin-word ledger stamp** (`YOUNG_PIN_LEDGER`). It is
   process-wide, and the w9-f analysis shows an unsound path under
   overlapping pauses. That path is reachable only with
   `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` or `CRATONVM_GEN_PINNED_YOUNG_COPY`,
   both off by default.

So in the default configuration, once the handoff is applied, the slot costs a
multi-VM process up to 250 ms per overlapping request and protects nothing.
Every pause of VM B that overlaps a long pause of VM A (a full G1 or ZGC
collection of a large heap) waits.

## Proposal

When `coverage_orphan_writes()` stays at 0 across the probe battery with the
handoff applied, make the wait conditional:

- wait only while either young pin-ledger consumer flag is on;
- otherwise take the slot opportunistically (one compare-exchange, never
  wait), keeping it for the census/diagnostic owner field;
- or remove it, after `YOUNG_PIN_LEDGER` is per VM (JIT round: per-VM
  `JIT_ACTIVE_DEPTH`).

The owner, the release accounting and the abandoned-hold logic
(`COVERAGE_SLOT_GAVE_UP_*`) then become dead code on the default path.

## Evidence to gather first

- `CRATONVM_DBG_STW_CENSUS=1` on `MtChurnProbe`, `ConcurrencyUnderGcSweep` and
  `G1ChurnPauseProbe` (all three backends): `orphan_coverage_writes` must stay
  at 0 after the handoff.
- A two-VM embedder run (or the `vm` test binary's
  `a_second_vms_pause_waits_for_the_first_vms_coverage_cycle`) measuring
  `COVERAGE_SLOT_WAITS` / `COVERAGE_SLOT_TIMEOUTS` before and after.

## Confirmation

```
rg -n "take_coverage_slot\(COVERAGE_SLOT_WAIT_LIMIT\)" vm/src/threading/gc_barrier.rs
```

## Retire when

The request path no longer waits for another VM's pause in the default
configuration, or a measurement shows the orphan rows are still written in
production (then this proposal is wrong and should be retired with that
reading).
