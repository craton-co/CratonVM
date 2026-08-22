# The H2 UPDATE path exhausts the heap under the JIT: every young collection is a non-moving sweep

## Status
**FIXED 2026-08-07** in `0ea21c07a`. Root cause and measurements are in
`docs/internal/fixed-suite-bugs/vm/jit-young-heap-exhaustion-after-header-16-FIXED-20260807.md`:
a JIT-allocated `new Object()` has an all-zero 16-byte header since the
`HEADER_SIZE` 24 -> 16 shrink, the young sweep read that as walk desync, and
abandoned the rest of the arena. Both questions this page asked are answered
there -- and the first ("why the fallback rate jumped 3 -> 361") was the wrong
question: the fallback FRACTION barely moved (75% -> 99.4%), what exploded was
the number of collections.

## Status (as filed)
**OPEN (2026-08-07).** A hard failure, not a slow one: `H2UpdateScaleProbe`'s
`testConcurrentUpdate` shape dies with H2 `Out of memory` at `--Xmx 1g` and 2g
where the pre-landing tip completes, and at 4g once the update count is raised.
Split out of [`h2-update-path-throughput-RETIRED-20260821.md`](h2-update-path-throughput-RETIRED-20260821.md),
whose "not heap pressure" and "not the JIT-root path" bullets this refutes.

## Severity
**HIGH.** It is the reason that page's own Reproducing block no longer runs,
and the reason the thread-scaling experiment it asks for cannot be performed at
the size it specifies — the VM exhausts the heap before the arm finishes. Any
H2 measurement taken at a heap size that survives is measuring a VM that is
collecting ~90x more often than it should.

## The chain, measured

Same binary, same `--Xmx 1g`, same 4-thread x 2500-update shape, one flag apart:

| arm | result | minor GCs | young decisions |
|---|---|---|---|
| `--nojit` | **rc=0, completes** | 7 | **moving=7, non_moving=0** |
| JIT on (default) | **Out of memory** | 238 | moving=2, **non_moving=236** |

It is not a small-heap artifact. Raise the work to 60 000 updates and give it
four times the heap (`--Xmx 4g`) and the same flag still decides it:

| arm (60 000 updates, `--Xmx 4g`) | result | young decisions |
|---|---|---|
| `--nojit` | **completes**, 572 CPU-s | **moving=7, non_moving=0** |
| JIT on | Out of memory | moving=2, **non_moving=1321** |

Seven moving collections carry 60 000 updates; 1 321 non-moving ones cannot.

Every fallback carries the same reason:

```
[moving-young] fallback: reason=innermost-rbp-belongs-to-unguarded-callee —
a live JIT frame could not prove a complete rewritable root map, so this young
collection runs the NON-MOVING sweep (no compaction, free-list allocation).
```

So: a live JIT frame blocks the moving young collector → the young generation
is swept in place instead of copied → nothing is compacted → the old generation
degenerates into a free list → allocation fails. `--nojit` removes the JIT
frame, the moving collector runs every cycle, and 7 collections suffice for the
work 238 could not.

## Against the pre-landing tip

`1082eb446` (before the object-header / compact-ref-field landing) vs current
`dev`, both with the JIT on, both `--Xmx 1g`, same shape:

| | pre-landing | dev |
|---|---|---|
| result | completes (2/2) | **OOM** (2/2) |
| minor / major GCs | 4 / 0 | 363 / 4 |
| young decisions | moving=1, non_moving=3 | moving=2, **non_moving=361** |
| old→young edges | 0 | **1 048 704** |
| card refinement | 21 ms | **7 837 ms** |
| old-gen blocks merged | — | **21 410 741** |
| objects allocated | 1 488 380 | 807 609 (died early) |

dev allocates FEWER objects, collects 90x more often, and still runs out. The
21-million-block coalesce is the shape of the damage: an old generation that is
all free list and no space.

`-XX:+UseG1GC` on the SAME dev binary completes the 1g shape. The defect is in
the generational collector's young path, not in the workload.

## What this refutes in the parent page

* **"Not heap pressure (`--Xmx` 1g/2g/4g/8g: no trend)"** — 1g and 2g now fail
  outright, and 4g fails at 60 000 updates. Heap size is the difference between
  a result and no result.
* **"Not the JIT-root path (`--nojit` scales identically)"** — `--nojit` is the
  difference between completing and exhausting the heap.

Both were true when measured. Neither is now, which is the argument for
re-running a ruled-out list against the current tip rather than inheriting it.

## Relationship to the closed fallback write-up

The retired `gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED`
write-up covers the same fallback reason under Tomcat. This is not that page
reopening on a technicality: there the fallback was a throughput tax, here it
is a hard OOM, and the fallback RATE against the same workload has gone from 3
to 361. Whatever closed it does not hold for this shape on this tip.

## Reproducing

The probe is `probes/H2UpdateScaleProbe.java`. It was NOT missing when this was
written — it sat in `docs/internal/repros/h2-insert-scale-20260731/`, which the
parent page's Reproducing block does not name and which `apps/` being gitignored
makes invisible from the obvious place; it is moved here so it survives that
directory's deletion. Its third argument is `objectCount`, not the lock timeout.

```bash
javac -cp <h2>/target/classes -d probe probes/H2UpdateScaleProbe.java
# fails:
<cratonvm> --java-home <jdk25> --Xmx 1g --verbose:gc \
    -c "<h2>/target/classes:probe" -Dprobe.dir=./db H2UpdateScaleProbe 4 2500 10000
# completes, same binary:
<cratonvm> --java-home <jdk25> --Xmx 1g --nojit -c "<h2>/target/classes:probe" \
    -Dprobe.dir=./db H2UpdateScaleProbe 4 2500 10000
```

`--verbose:gc` prints the `decision histogram` and `generational: minor=/major=`
lines the tables above are read from; they are the fastest way to tell this
apart from an ordinary slow run.

## Where to look

`innermost-rbp-belongs-to-unguarded-callee` is raised by the conservative root
scan when it cannot bound the innermost JIT frame. Two questions worth keeping
separate:

1. **Why the fallback rate jumped** from 3 to 361 for the same workload across
   the header landing. The landing moved every field offset (`HEADER_SIZE`
   24 → 16) and the JIT bakes displacements, so a frame-descriptor or
   root-map assumption that shifted with it is the first place to look.
2. **Why a persistent fallback is fatal rather than merely slow.** The
   non-moving sweep is documented as a correctness-preserving degradation; an
   old generation that coalesces 21 million blocks and then cannot satisfy an
   allocation is a second defect on top of it, and it is the one that turns
   this into an OOM.
