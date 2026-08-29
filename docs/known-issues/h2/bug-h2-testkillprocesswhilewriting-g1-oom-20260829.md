# `-XX:+UseG1GC` fails `TestKillProcessWhileWriting` — a G1 `OutOfMemoryError` with no arena failure

## Status

**OPEN, split out 2026-08-29** from
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`, whose ZGC
defect is closed and which never owned this row. The class **passes under the
default collector**; only the explicit `-XX:+UseG1GC` arm fails.

## Why it is a different defect, measured rather than assumed

The parent page's whole subject is `zgc: arena allocation failed` —
fragmentation of the ZGC arena, reported by that collector's own guard. **This
arm produces no `arena allocation failed` line in either era**, before or after
that work, because G1 does not use the arena at all.

The numbers, from the parent page:

| era | outcome |
|---|---|
| when the parent page first measured it | 2 `OutOfMemoryError`, 31–43 s |
| on the fixed binary | 13 `OutOfMemoryError`, 1500 s cap |

**The face varies between runs**, so reproduce it several times before believing
any single one — a one-run reading here has already misled once (the parent page
had to run a five-arm interleaved A/B, `dev` binary against fixed binary, to
establish that the 2026-08-24 relocation work was not responsible; every column
came back identical).

## What to check first

G1 is an evacuating collector, so an `OutOfMemoryError` there is a
*to-space exhaustion or humongous-allocation* story, not a free-list
fragmentation one. The instruments are G1's own: the collection-set selection,
the humongous path (`is_humongous`, `pin_region_for_addr`) and whether a
completed concurrent mark cycle is reclaiming dead Old/humongous spans —
`last_ditch_reclaim` exists precisely because it is only a *finished* cycle's
cleanup that frees them.

Rerun it at least three times before drawing any conclusion from a count.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g -XX:+UseG1GC \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting
```

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of, and the A/B that separated the two.
