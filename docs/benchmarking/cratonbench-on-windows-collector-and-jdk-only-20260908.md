# CratonBench on the Windows dev box: two localised costs

**2026-09-08.** The perf GATE cannot run here — it exits
`FATAL: taskset required (Linux bench host)` and its baseline is the Azure EPYC
host — so this is the benchmark run directly, not a gate verdict. Every number
below is a median of 3-7 fresh processes, pinned, checksum-verified on every
run. No verdict is claimed against the Azure baseline: different machine,
different OS.

## The measurement is only worth reading because it is pinned

Unpinned, every phase blew the gate's own `--max-cv 5` ceiling — 7.1% to
**42.3%** (hashmap ranged 8.4 s to 23.2 s). Pinning the process to one core
(.NET `ProcessorAffinity`, the Windows equivalent of `taskset`) brought that to
**1.3%-6.1%**, and moved `stringregex`'s median from 614 ms to 220 ms. That is
this repository's own pinning requirement, reproduced from the other side: an
unpinned number on this host is not a number.

## Finding 1 — the default collector costs 6.3x on `bintrees`, and only there

Same box, same pin, standard mode:

| phase | default (ZGC) | G1 | G1 gain |
|---|---:|---:|---:|
| arithmetic | 5218 | 5101 | 1.0x |
| fib | 8835 | 8723 | 1.0x |
| sieve | 4711 | 4777 | 1.0x |
| matrix | 2891 | 2600 | 1.1x |
| hashmap | 8675 | 6563 | 1.3x |
| stringregex | 237 | 311 | 0.8x |
| **bintrees** | **10447** | **1664** | **6.3x** |

Explicit `-XX:+UseZGC` reproduces the default (10028 ms), confirming which
collector the default selects; `Generational` sits between at 2277 ms.

It is **not** a broad collector cost — six of seven phases are within 30%. It is
specific to the allocation-heavy binary-trees workload, which is what that
benchmark exists to stress.

Two things follow. First, the same-host HotSpot control puts this in
proportion: CratonVM/HotSpot on this box is 1.1x on sieve, 1.2x on matrix,
2.5x-3.9x on four others, and **26x on bintrees** under the default — an
outlier of a different kind, not "this host is slow". Under G1 that 26x becomes
about 4x, in line with the rest.

Second, and worth a look by someone who owns the gate: the Azure baseline for
`bintrees` is **1550 ms, status `anchored`**, and its note cites a document
dated 2026-07-24 — before ZGC became the default on 08-10. G1 here measures
1664 ms, essentially that baseline; the default measures 10447. Whether the
gate's bintrees row still describes the configuration the gate actually runs is
a question this data raises and cannot answer from a different machine.

## Finding 2 — `--jdk-only` costs ~3x on `hashmap`, in every collector

| phase | std G1 | jdk-only G1 | jdk-only Gen | jdk-only ZGC |
|---|---:|---:|---:|---:|
| arithmetic | 5101 | 6824 | 5221 | 5105 |
| fib | 8723 | 9065 | 8454 | 9547 |
| sieve | 4777 | 4950 | 4937 | 4416 |
| matrix | 2600 | 2096 | 2083 | 2242 |
| **hashmap** | **6563** | **23803** | **26346** | **25244** |
| stringregex | 311 | 381 | 393 | 314 |
| bintrees | 1664 | 1645 | 2405 | 10231 |

`hashmap` is 3.6x-4.0x slower under `--jdk-only`, and the three collectors agree
(23.8 s / 26.3 s / 25.2 s against 6.6 s), so this is collector-INDEPENDENT and
about the JDK-only class implementations rather than memory management.

The two findings are orthogonal and both survive the crossing: bintrees keeps
its ZGC penalty inside `--jdk-only` (1645 G1 against 10231 ZGC), and hashmap
keeps its jdk-only penalty under every collector.

## What is not claimed

Magnitudes are approximate — median of 3 for the sweep, on a box at 5-15% of 32
cores. Both effects (6x, 3.6x) are far larger than the 1.3-6.1% pinned noise
floor, so the DIRECTIONS are solid and the exact figures are not. Nothing here
is a regression finding: there is no local baseline and no prior local
measurement to compare against. It is a profile of where this VM's costs sit on
this host, and two places worth someone's attention.

All 21 phase/collector/mode combinations verified their exact checksum.
