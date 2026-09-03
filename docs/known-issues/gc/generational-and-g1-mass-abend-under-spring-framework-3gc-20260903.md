# Spring Framework full suite: Generational and G1 abend on ~35% of classes, ZGC on none

## Status
New finding, 2026-09-03, Azure host (`vm1`). Reproduced once per GC, not yet
isolated from a real confound (see below) — **not yet confirmed as a
GC-specific defect** rather than a shared-host memory-contention artifact.

## The numbers

Full Spring Framework suite (2848 classes), fork-per-class (`CratonRunner`,
one JVM per class), one full pass per collector, launched within seconds of
each other and run **concurrently** on the same 8-core host:

| GC | OK | ABEND | TIMEOUT | FAIL | LOADERR | Wall |
|---|---:|---:|---:|---:|---:|---:|
| **ZGC** | 2832 (99.4%) | **0** | 2 | 5 | 9 | 14394s (4h00m) |
| Generational | 1853 (65.1%) | **976** | 9 | 1 | 9 | 20222s (5h37m) |
| G1 | 1932 (67.8%) | **887** | 18 | 2 | 9 | 25576s (7h06m) |

`ABEND` is the harness's classification (`run-suite.sh:476`) for a process
that exited non-zero **without** a recognizable crash signature in stderr
(`panicked at`, `not yet implemented`, `unreachable`, `index out of bounds`,
`EXCEPTION_ACCESS`, `STATUS_`, `fatal runtime`) — i.e. the process just died,
silently, with no diagnostic trail. That shape (no panic text, no signal
name, just gone) is the classic signature of an external `SIGKILL` — most
commonly the Linux OOM killer — rather than a CratonVM-internal panic.

## Why this is not yet a confirmed GC-specific defect

All three collectors' full passes were launched within ~6 seconds of each
other and ran **the entire time in parallel**, sharing one 8-core host that
was also carrying other users' unrelated work (`uptime` showed load
9.5–14.8 and 30+ other logged-in users across the run). Three simultaneous
full JVM-per-class Spring Framework suites — each doing real allocation,
each running its own collector — is a plausible way to get one arm
starved of memory well before another, independent of any actual
per-collector defect. `dmesg` was not readable (no sudo) to directly confirm
OOM-kill, so the mechanism is inferred from the ABEND shape, not proven.

**What would distinguish the two hypotheses**: rerun each collector
**alone**, sequentially, on an otherwise-quiet host. If Generational/G1
still abend on ~35% of classes with nothing else running, that is real
per-collector instability under normal JIT/GC load. If the abend rate drops
close to ZGC's near-zero, this was host memory contention amplified by
running three heavy suites at once, and specifically punishing whichever
collector(s) have a larger or less compaction-eager memory footprint than
ZGC's.

## What is consistent with either hypothesis

- The wall-clock ordering (ZGC fastest, then Generational, then G1) is at
  least consistent with ZGC finishing enough of its own work early that it
  stopped contending for memory sooner, letting the other two run under
  less pressure than they would have alone — but is equally consistent with
  ZGC simply being faster on this workload regardless of contention.
- `TIMEOUT` and `LOADERR` counts (both small, both roughly comparable
  across arms) are not obviously implicated either way.

## Not yet done

- Sequential, isolated single-collector reruns (Generational alone, G1
  alone, ZGC alone, on a quiet host) — the direct test of the confound
  above. This is the next step before treating the 976/887 ABEND counts as
  a real defect count.
- Identifying even one concrete ABEND'd class's actual failure mode (peak
  RSS at time of death, whether `dmesg`/journal on this host has OOM
  records once sudo/log access is available, or reproducing standalone
  with `/usr/bin/time -v` to capture max RSS).
- Checking whether the ABEND'd classes cluster in any way (e.g. specific
  Spring modules, specific allocation-heavy test shapes) or are spread
  uniformly across the suite — clustering would argue for a real per-class
  mechanism rather than uniform memory pressure.

## Repro

```bash
cd apps/spring-suite-runner
# One collector at a time, nothing else running on the host:
CRATONVM_BIN=/tmp/cratonvm-gen-wrapper.sh JDK25=/data/toolchain/jdk-25 \
  ./run-suite.sh run --category all --tag gen-solo
# cratonvm-gen-wrapper.sh: exec <cratonvm> -XX:+UseGenerationalGC "$@"
# repeat with a G1 wrapper (-XX:+UseG1GC) and no wrapper (ZGC is default)
```
