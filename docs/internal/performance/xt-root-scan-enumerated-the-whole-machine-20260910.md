# The cross-thread JIT root scan enumerated every thread on the MACHINE, and that still was not what made it slow

**Date:** 2026-09-10
**Area:** `vm/src/jit/xt_root_scan.rs` (Windows arm), `vm/src/runtime/interpreter/gc_and_alloc.rs`
**Status:** enumeration fixed; the per-collection cost it was blamed for is NOT fixed and is not the enumeration

## VERDICT

`CRATONVM_XT_JIT_ROOT_SCAN=0` finishes `VthreadGcStress` in ~9 s; leaving the
scan on costs ~26-33 s. The scan was assumed to be paying for its
`SuspendThread` / `GetThreadContext` / `ResumeThread` traffic. It was not.

Two separate things were true, and only the first got fixed:

| | claim | status |
|---|---|---|
| 1 | both Windows passes enumerated threads with `CreateToolhelp32Snapshot`, which walks the whole SYSTEM, to find this process's ~40 | **fixed** — real, measured, ~20 s of CPU per run |
| 2 | that enumeration is why the scan costs 3x the floor | **false** — removing it moved the median by −1.5 s against a spread of ~35 s |

## 1. What the enumeration cost

Measured standalone, independently of the VM
(`tools/thread-enum-cost/snaptime.rs`, 50 calls, this dev box):

```
threads on system = 6952      in this process = 44
CreateToolhelp32Snapshot alone : 9.9 ms per call
snapshot + Thread32Next walk   : 83.2 ms per call
```

`take_over_pass` runs once per barrier round and `helper_window_pass` once per
collection. One `VthreadGcStress` run: 174 + 65 = 239 passes x 83 ms ~ **20 s
of CPU spent walking the machine's thread table**.

`helper_window_pass` was the starker of the two: its filter was already
`blocked_os_tids.contains(&tid)` — it walked 6,952 system-wide entries purely
to intersect with a list passed to it as an argument. Iterating that list is
exactly equivalent.

The Linux arm never had this: it reads `/proc/self/task`, which is
process-local. The fix restores the two platforms to asking the same question.

## 2. Why the roster is complete

`take_over_pass` now takes a roster (`ThreadRegistry::alive_count_and_os_tids`,
re-read every round so a thread registering mid-collection is picked up).
A peer this pass does not freeze is a peer whose registers and spill slots go
unscanned — a missed conservative root and a use-after-free — so the roster
must cover every thread that can have `Rip` in a registered JIT code range.

It does: compiled code is entered only via `JitEntryGuard::enter_with_compiled`
/ `enter_with_compiled_at`, whose production call sites are all on Java
execution paths (`runtime::interpreter`, `interpreter::jit_bridge`,
`jit::helpers`); `memory::roots` uses the plain `enter`, which records a chain
entry and transfers to nothing. The roster's `alive && stw_ready` gate excludes
only threads the registry already documents as unable to execute Java.

Because that argument rots silently as call sites are added, it is also
checked: `CRATONVM_XT_ROOT_SCAN_AUDIT=1` re-walks the full system snapshot and
reports any in-process thread absent from the roster whose `Rip` is in JIT
code. **0 holes across 147 passes.** The audit is deliberately NOT on the
`xt-jit-root-scan` debug token — that walk is the cost being removed, and
bundling them is what hid the coupling below.

## 3. The measurement that refuted the premise

Interleaved, 6 rounds, alternating binaries in one window so load drift hits
both arms:

| arm | samples (s) | median |
|---|---|---|
| pre-fix, scan on | 40, 50, 26, 49, 19, 19 | 33 s |
| post-fix, scan on | 29, 18, 83, 12, 23, 47 | 26 s |
| scan off (floor) | 16, 13, 7, 8, 7, 10 | **9 s** |

Post-fix is faster in 3 of 6 paired rounds; mean paired difference −1.5 s
against a per-arm spread of ~35 s. **Not significant.** Removing 20 s of CPU
did not move wall clock, so that CPU was not on the critical path — it
overlapped the barrier wait, which is bounded by the slowest mutator.

Peer examinations halved (6,960 → 3,473) with no wall-clock effect either.

## 4. What the scan DOES still cost, and what was ruled out

The remaining ~17-24 s is the suspension of mutators itself: the pass suspends
peers the barrier is simultaneously waiting on. Three cheaper gates were tried
and all are dead:

* **`any_thread_in_jit()` at round 0** — the counter includes the CALLER's own
  entries, and the caller commonly arrives via `maybe_gc` from compiled code.
  Correcting it to subtract self (`peer_thread_in_jit`) moved passes 93 → 148.
  Reverted: no benefit, and it adds a skip condition to a path where a miss is
  a use-after-free.
* **Gating every round on that hint** — 123-137 passes, still 0 taken over.
* **`ThreadExecState::CompiledUninterruptible` census** — reads 0 on every
  pass and looks strictly more precise. **It is unsound as a gate.**
  `leave_blocked_region_flagged` records `JavaRunning` on the blocked→running
  edge (an approximation its own comment documents) while the `Rip` returns
  into compiled code. It reads 0 because it UNDER-reports, which is exactly
  why gating on it would drop a peer that must be scanned.

The untried avenue with a sound direction: skip peers positively recorded
`SafepointParked`. That is the safe polarity — skip only what is affirmatively
parked, rather than skip anything not affirmatively in JIT — and the barrier's
park sites save and restore the prior state correctly. The barrier tracks
`arrived` as a count, not identities, so the census is the only source for it.
Not attempted here.

## 5. A behaviour change that is not a speedup

Pre-fix, takeovers were **0 across 12,200 peer examinations** (2 runs).
Post-fix: **3 across 14,973** (4 runs), distributed `2, 0, 1, 0` — so the scan
now sometimes freezes a peer where before it never did in anything sampled.

State that carefully, because a first single run showed `2` and it was
tempting to read it as a clean 0 → 2 improvement. It is not: three of the four
post-fix runs returned 0 or 1. The effect is real in direction and small in
size, and four runs cannot separate "the walk let peers escape" from noise
around a rare event.

The mechanism is at least plausible: the 83 ms walk ran BEFORE the suspends, so
a peer classified at the end of it had had 83 ms to leave JIT. If that is what
was happening, the feature was partly disabled by its own slowness and now does
more of what it says — which means more frozen peers, more conservative roots
and more pinning than before. **The risk in this change is that, not the
enumeration**, and it is why this lands only behind a green full suite rather
than on the benchmark.
