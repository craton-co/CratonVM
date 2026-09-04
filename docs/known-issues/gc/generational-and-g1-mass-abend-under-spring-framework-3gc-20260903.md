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

## Update 2026-09-04: confirmed real, root-caused, and already fixed — on an unmerged branch

This doc's own open question — contention artifact vs. real per-collector
defect — is answered. **Real.** Independently confirmed on two more, wholly
unrelated codebases, with a mechanism that a fix already exists for.

### Independent reproduction on H2 and Spring Boot

Two full-suite, 3-GC-sharded runs (H2 Database, 218 classes; Spring Boot,
1991 classes), same host, same `cvm-h2serial-20260813` binary
(dev tip `d9d3eb336`, 2026-09-03):

| Suite | Generational CRASH | G1 CRASH | ZGC CRASH |
|---|---:|---:|---:|
| H2 (218 classes) | 14 | 2 | 5 |
| Spring Boot (1991 classes) | **347 (17.4%)** | 0 | 1 |

Every one of these 361 crashes is `rc=139` (SIGSEGV) with a full CratonVM
crash banner, and — checked across all 361 — **every single one faults at the
exact same low-order instruction-pointer offset** (`pc & 0xfff == 0x6ac`
relative to each run's ASLR-shifted load base). `addr2line` against the
binary resolves that offset to:

```
<cratonvm_gc::gen_heap::GenerationalHeap>::is_object_address
```

Several sampled register dumps also show `rax`/`rcx`/`rdx` holding
round, evenly-strided addresses (`0x...×000000`, stride `0x20000000`) — the
generational arenas' own region-boundary values — with the faulting `rsi`
matching one of those boundaries plus a small header-field offset, consistent
with a conservative-root candidate that looks in-range but isn't backed by
mapped memory.

**Why this rules out the host-contention hypothesis this doc raised**: an
OOM-killer SIGKILL leaves no crash signature at all (that's exactly this
doc's own `ABEND` classification — silently gone, no diagnostic trail).
These are the opposite: a clean SIGSEGV banner, register state, and a
**single, deterministic instruction pointer hit 361/361 times across two
codebases that share no code with Spring Framework or with each other.**
Memory pressure cannot converge two independent multi-hundred-class suites
onto the identical byte offset. This is a real, deterministic,
Generational-GC-specific defect, not contention noise — even though (worth
flagging honestly) these H2/Spring Boot runs were *also* launched as 3
concurrent shards on one host, same as this doc's original Spring Framework
run. The signature itself, not the run's isolation, is what settles it here.

### Root cause, and it's already fixed

`git log --all` on the shared checkout turns up the exact fix, already
landed on an **unmerged** branch:

```
commit 5dc49baf5 (claude/gc-jit-issues-retirement-aab58b, 2026-09-03 12:38 -0300)
fix(gc): the generational validator dereferenced reserved-but-uncommitted heap
```

Its own commit message names the precise mechanism: `region_bounds`
publishes `[base, base + CAPACITY)`, where `CAPACITY` is *reserved* address
space, but the young arenas commit lazily in 2 MiB granules
(`gc/src/reservation.rs`). A conservative root-scan candidate that merely
looks like a young-gen pointer passes `is_object_address`'s region check
against the full reserved range, then faults dereferencing the header in a
page that was never actually committed. The same commit message reports
`CRATONVM_GC_RESERVE=0` (forcing the wholly-committed fallback store) turning
every reproduced crash into a clean pass — the A/B that named the mechanism.

That commit's own measurement (Spring Framework, 2848 classes): **ZGC 0,
Generational 976, G1 887** — matching this doc's original numbers almost
exactly, and explicitly attributing ZGC's immunity to its validator
consulting "a live-base registry" rather than speculatively dereferencing.

### One open discrepancy worth a follow-up

The fix commit's own Spring Framework measurement shows **G1 badly affected
(887/2848, 31%)** — but my H2 and Spring Boot runs show G1 nearly clean (2
and 0 crashes respectively). Two non-exclusive explanations, neither checked
yet:

- G1 has its *own* separate `is_object_address` (`gc/src/g1.rs:16137`) —
  a structurally similar but independently-implemented function. It may
  carry the same reserved-vs-committed confusion, but be far less likely to
  trigger under H2/Spring Boot's allocation shape than under Spring
  Framework's.
- The fix branch has moved since (`19854a573 fix(zgc): the relocation
  slides wrote into granules the give-back had returned` is a later commit
  on the same branch) — it's possible a G1-side fix landed there too and
  simply wasn't captured in this doc's original numbers, which predate it.

Not investigated here — flagged for whoever picks this back up.

### Practical implication

The fix exists, is not yet on `dev`, and this doc's original 976/887 ABEND
counts should be read as **confirmed real**, not a suspected contention
artifact. Landing `claude/gc-jit-issues-retirement-aab58b` (or cherry-picking
`5dc49baf5` and whatever G1-side work followed it) onto `dev` is the
concrete next step, ahead of any further investigation from this doc's
original "Not yet done" list.
