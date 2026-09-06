# `residency-gc.sh` is RED under Generational: a deterministic SIGSEGV in the `int[]` arm

| | |
|---|---|
| **Status** | OPEN. Deterministic — 3/3 on a PRISTINE dev binary, interleaved against a modified one, and 3/3 again after merging `154d9e845`, which carried the day's `gc/src/heap.rs`, `gc/src/zgc.rs` and `jit/src/x64/safepoint.rs` traffic. Not a flake and not new to any branch. |
| **Scope** | `-XX:+UseGenerationalGC` **and** `--gpu` **and** the JIT **and** an offload that actually happens. ZGC, G1, `--nojit`, and `--gpu` with the threshold raised out of reach all pass. |
| **Reproducer** | one local fixture, ~10 seconds |
| **Found** | 2026-09-06, while re-verifying the battery for two unrelated GPU pages |
| **Probably** | the same open defect as [the Generational non-moving young sweep zeroing a live `FileChannelImpl`](../springboot/generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md) — see "Why this is probably not a new defect", and the one thing that does not fit |

## The defect

`bench-gpu/residency-gc.sh` fails 6 checks, identically on every run. The
`-XX:+UseGenerationalGC` GPU arm produces **no output at all**, because the
VM dies:

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF737209519
#  Faulting access: read at address 0x000001EA5A800888
#  thread: "main-vm"
#  gc collector: generational
#  gc young-gen policy: moving (Cheney young copy)
#  gc young-gen actual: 0 moving cycle(s), 4 cycle(s) diverted to the NON-MOVING sweep
#  jit: faulting pc is not inside any compiled code range
```

`gc_short=` and `gc_byte=` print first; it dies inside `roundsInt`. So it is
the `int[]` arm, after the `short[]` and `byte[]` arms have completed.

Every one of the four young collections in the run is diverted:

```
[moving-young] fallback #1..#4: reason=unregistered-jit-frame-on-stack — a live
JIT frame could not prove a complete rewritable root map, so this young
collection runs the NON-MOVING sweep (no compaction, free-list allocation).
```

## Repro

```bash
CV=target-gpu/release/cratonvm.exe
"$CV" --java-home <jdk25> -cp test_classes/gpu -Xmx64m \
      --gpu-min-work 64 -XX:+UseGenerationalGC --gpu GpuResidencyGc 0 1024 60
```

The `-Xmx64m` and `--gpu-min-work 64` are `residency-gc.sh`'s own defaults and
both are load-bearing — that script's header explains why. With the obvious
defaults it drives zero collections and nothing happens at all.

## It is NOT from any branch

Established before anything else, because it was found on a working branch and
"my change broke it" is the cheapest hypothesis to hold and the most expensive
one to be wrong about.

A pristine binary was built from a detached checkout of `0458f0def` — the exact
commit the branch was cut from — and the two binaries were run **interleaved**,
same host, same minute:

| run | pristine `0458f0def` | branch binary |
|---|---|---|
| 1 | **CRASH** | CRASH |
| 2 | **CRASH** | CRASH |
| 3 | **CRASH** | CRASH |

It is also not a `--gpu`-side regression from earlier the same day: the branch's
own kill switch, `CRATONVM_GPU_JIT_GATE_LATE_REGISTER=0`, restores the old
compiled-site memo and the crash is unchanged, and the census confirms the
branch's new registration path never fires for this fixture
(`late-registered kernels=0` — `GpuResidencyGc` declares its kernels in its own
class, so the caller scan registers all of them the old way).

It IS newer than 2026-09-05: that day's write-up of `residency-gc.sh` records it
as passing, with one intermittent failure in 25 runs of a different kind (a G1
forwarding assert, since fixed). A deterministic Generational SIGSEGV would not
have been described that way. Somewhere in the day's traffic on `dev` — which
includes a good deal of Generational and JIT root-map work — this appeared.
Bisecting it costs a ~35-minute release build per step and was not attempted.

## Ablation

One binary — the pristine `0458f0def` one — one fixture, one lever at a time.

| arm | result |
|---|---|
| default (repro) | **CRASH** |
| `-XX:+UseZGC` | ok |
| `-XX:+UseG1GC` | ok |
| no `--gpu` | ok |
| `--gpu --nojit` | ok |
| `--gpu --gpu-min-work 1000000000` (nothing clears the threshold) | ok |
| `CRATONVM_JIT_OSR=0` | ok |
| `CRATONVM_JIT_GPU_ARRAY_BARRIER=0` | ok |
| `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow` (the residency cache stands down) | ok |
| `CRATONVM_GPU_JIT_ARRAY_WRITERS=keep` (writers refused the JIT, cache stays live) | **CRASH** |
| `CRATONVM_GC_NO_MOVING_YOUNG=1` | **CRASH** |
| `CRATONVM_GPU_JIT_GATE_CALLERS=block` (callers of kernels stay interpreted) | ok |

Two rows are worth more than the rest.

**`NO_MOVING_YOUNG=1` still crashes.** That lever forces every young collection
down the non-moving sweep — which is what the fallback was already doing on all
four cycles. So the moving young path is not implicated, and a fix that restores
compaction would not touch this.

**`ARRAY_WRITERS=allow` is the only GPU-side lever that clears it, and it is the
one that turns the residency cache OFF** (`input_cache::disable_for_jit_array_writer`).
Its sibling `keep` — which refuses the array writers the JIT and keeps the cache
live — still crashes. So the discriminator is the LIVE CACHE, not whether the
array-writing method is compiled.

## Why this is probably not a new defect

Every scope line matches the OPEN page
[`generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md`](../springboot/generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md):
Generational with the JIT on fails; HotSpot, ZGC and Generational `--nojit` all
pass; and the mechanism it names is the non-moving young sweep reclaiming an
object whose only reference "was a register/native-stack root the marker
missed". `gen_heap.rs`'s own test
`non_moving_old_sweep_frees_in_place_and_is_addr_live_says_so` documents the
same shape from the other side: the non-moving sweep frees blocks IN PLACE,
which several `is_addr_live` consumers were written not to expect.

The GPU input-residency cache is a plausible instance of exactly that missed
root — it holds Java arrays by raw address, and `remap_and_sweep` was built to
re-key them across a RELOCATION, which is the moving case. Whether the
non-moving sweep can free an array the cache still points at is the question to
ask first.

**What does not fit, and is the reason this says "probably".** That page's probe
is `CRATONVM_DBG=sweep-zero`, which is documented to name the victim by class in
seconds, and on this reproducer it prints **nothing** — no `RECLAIMED-LIVE` line
in a run that crashes. The probe reports a zeroed object at the moment it is
INVOKED as a receiver, and an `int[]` is never a receiver, so silence is
consistent with the same defect on a victim the probe cannot narrate. It is
equally consistent with a second defect. Nothing here separates them, and
"lives in the same subsystem" is not attribution.

## What to try next, cheapest first

1. **Does the residency cache hold the victim?** Log the cache's key set at each
   non-moving sweep and check whether any key names a block the sweep freed.
   That is one printf and it either confirms the hypothesis above or kills it.
2. **Make `sweep-zero` cover arrays.** It reports on receiver invoke; an
   `int[]` element read is where this one dies. Extending it to array access
   would either name the victim here or prove the victim is not a swept object.
3. **Only then bisect.** It is deterministic, so a bisect is reliable — it is
   just expensive, at one release build per step.

## What it blocks

`residency-gc.sh`, one of the five scripts `gpu-selfhosted.yml` runs, with no
`continue-on-error`. The weekly self-hosted GPU job is RED until this is fixed
or the Generational arm is skipped. The other four scripts and `ci-gate.sh` are
green, so this is the only thing standing between that job and a pass.
