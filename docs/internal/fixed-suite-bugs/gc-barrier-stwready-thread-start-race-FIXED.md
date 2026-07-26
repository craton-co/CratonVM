# GC barrier / AQS contention race — fixed

Status: FIXED on 2026-07-11.

## What was fixed

Two independent failures were reachable from `AqsContentionProbe` (28 threads
contending on one `ReentrantLock` while another forces `System.gc()`):

1. A newly started carrier could be counted by an STW census before it was
   `stw_ready`, then correctly wait via the non-participating startup path.
   The resulting quota could never be satisfied. The existing `stw_ready` gate
   now applies consistently to the live-blocked census.
2. A waking blocked mutator could drop its barrier guard before clearing the
   registry's blocked flag. A new pause could exclude that already-runnable
   thread. Guard completion and the flag transition are now one barrier-locked
   operation; all Java block/wake paths use it. Native and foreign-thread
   transitions receive the same atomic publication.

The recovery loop also runs a RIP-based JIT takeover scan after a genuine
cooperative-wait timeout even when the global JIT-depth hint is false. The scan
parks only a peer whose RIP is in a registered JIT range.

The remaining stale AQS-node corruption was not confined to the AQS package:
interpreting every lock class still allowed a core-Java helper/caller to produce
the stale reference. Conservative JIT policy therefore fails closed for the
entire `java/` package. `SkipPolicy::Aggressive` and
`CRATONVM_JIT_ALLOW_PACKAGES=java/` remain available for controlled bisection.

## Verification

Built on the Azure probe host in `/data/data/gc-aqs-stale-pointer-20260711`
with profile `hc0053dbg`, binary:
`probes/bin/cratonvm-gcaqs0711-fix8-hcdbg`
(`sha256: 3bac761c45f0b04ba3cdb5524d284b616069cb6fe07f855fd34bc79780d8dce4`).

- `AqsContentionProbe`: 30/30 clean under default settings.
- Every run reported `totalAcquires=112000 expectedTotal=112000` and `RC=0`.
- No `Stale pointer` or `STW cross-thread` signature occurred.
- Logs: `probes/fix8-default-aqs-batch1-0711.log` and
  `probes/fix8-default-aqs-batch2-0711.log` on the probe host.

The local barrier tests cover the atomic flagged-guard exit and the no-pause
flag-clear case. The core-Java conservative-policy test covers the default
skip and its Aggressive-policy escape hatch.
