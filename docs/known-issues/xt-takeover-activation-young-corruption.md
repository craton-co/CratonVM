# xt cross-thread JIT takeover activation → young-gen header corruption + crashes (REGRESSION, dev@a00570c6)

Status: **OPEN, HIGH.** Attributed 2026-07-03 by env-gate A/B on the DoHead
repro; mitigation available (`CRATONVM_XT_JIT_ROOT_SCAN=0` or flipping the
default back to opt-in) pending a fix of the takeover machinery itself.

## Evidence (DoHead idx 38, `-Xmx500m`/150 s, 12 runs each, same binary where applicable)

| config | crashes | sweep-desync warns/run |
|---|---|---|
| dev@`11ca8abc` (xt takeover INERT — jit-side gate mirror stuck opt-in) | **0/12** | ~0 |
| dev@`a00570c6` default (takeover ACTIVE via `0ba9a05d` polarity fix) | **6/12** | up to 178 |
| dev@`a00570c6` + `CRATONVM_XT_JIT_ROOT_SCAN=0` | **1/12** | **0 in all 12** |
| dev@`a00570c6` + GC walk-hardening branch (containment only) | 4/12 | up to 42 |

The 1/12 xt-off crash matches the historical background rate (pre-drift
baseline 1/18, same `0x2_0000_0000`-family face).

Crash faces under xt-on: reads at `0x0000020000000000/04/18` (Value-cell
misdecode / stale receiver — fabricated pointer from `[payload32|disc]`
hybrid bits) in interpreter/native helpers on main-vm and workers, plus one
mark-BFS extent runaway (`0x3EA70008`; that face is separately clamped by
`fix/dohead-sweep-freelist` commit `8e64d9a5`). Massive young non-moving
sweep desync warnings accompany the corruption.

## Mechanism (hypothesis, consistent with all observations)

`0ba9a05d` ("fix/fork6-xt-helper-window-20260702") fixed a gate-polarity bug:
the jit-crate mirror of `CRATONVM_XT_JIT_ROOT_SCAN` had stayed opt-in when the
vm side flipped to default-on, so **no JIT code range was ever registered and
the entire cross-thread STW takeover was silently inert in every default-env
process** — including all of its historical "validation". The polarity fix
activated, for the first time at scale: forcible mid-JIT stops of peer
threads, takeover-side frame/root scans, BUG-03 TLAB reserved-tail windows,
and the helper-window scan added by the same branch. One or more of those
paths corrupts young-gen memory under Tomcat's thread churn (the memory note
for the branch itself records "residuals OPEN").

## Repro / verification

`apps/tomcat-suite-runner/run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real
-Category all -Start 38 -Count 1 -TimeoutSec 150 -Parallel 1 -MaxHeap 500m
-Exe <exe>`; compare N≥12 with and without `CRATONVM_XT_JIT_ROOT_SCAN=0`.
Results archived: `apps/tomcat/.suite/results/dhswdevpure3-*` (xt on) vs
`dhswxtoff-*` (xt off); binary `C:\craton\CratonVM-dohead-sweep\cvmdhsw-devpure3.exe`
(= dev@`a00570c6`).

## Recommended action

1. Short term: flip the takeover default back to opt-in (or ship
   `CRATONVM_XT_JIT_ROOT_SCAN=0` in the suite harnesses) — the fork6/A4
   improvement it buys is much smaller than the Tomcat corruption it costs.
2. Then debug the takeover paths under the DoHead repro with the GC
   walk-hardening branch's diagnostics (SWEEP_* counters + extent-clamp hex
   dumps identify the corrupt-header faces directly).
