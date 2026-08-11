# H2 under `-XX:+UseG1GC` — SIGSEGV at a byte-identical heap address, two classes, one shared fault site

**Status:** OPEN (2026-08-11). Found during a targeted rerun of H2's still-non-passing
classes post `dev`-merge (see
`gc-corruption-guard-fixed-by-dev-merge-20260810.md` and
`gc-variant-fullsuite-crashes-hangs-fails-20260810.md` in this folder for
the broader context this surfaced from). G1 variant only — 0 crashes this
round under the default (Generational) or ZGC collectors on the same
binary/commit. Binary: `/data/cratonvm/target-zgc/release/cratonvm-h2-g1-postmerge2`
(`--features zgc` build, commit `4c4fb3902`, `-XX:+UseG1GC`).

## The crash

Two classes, `org.h2.test.store.TestKillProcessWhileWriting` and
`org.h2.test.store.TestRandomMapOps`, both SIGSEGV while dereferencing the
**exact same address**:

```
#  SIGSEGV at pc=0x6238e2eaaab0, addr=0x20084400000, ...  # TestKillProcessWhileWriting
#  SIGSEGV at pc=0x583a2aaddaab, addr=0x20084400000, ...  # TestRandomMapOps
```

`addr=0x20084400000` is byte-for-byte identical across two independent
process launches with independent ASLR bases — that rules out coincidence;
whatever both crashes touch, they touch the *same* absolute address, not
just a similarly-shaped one. The address itself (`...400000`, 4MiB-aligned
within a much larger, equally round base) reads as a **heap region
boundary**, not an object — the same "region base, not an object" shape the
sibling `../hibernate/g1-collector-fullsuite-crashes-hangs-fails-20260806.md`
doc found for its own G1 shared-fault-site crash.

The two fault PCs, resolved against each binary's own mapped-executable
segment (`here:` line in the crash report), land within a handful of bytes
of each other:

```
TestKillProcessWhileWriting: pc 0x6238e2eaaab0, here starts 0x6238e2c2e000 -> offset 0x27caab0
TestRandomMapOps:             pc 0x583a2aaddaab, here starts 0x583a2a861000 -> offset 0x27caab
```

(Both binaries are the same build, `cratonvm-h2-g1-postmerge2`, just two
separate process launches — the small offset delta is consistent with two
adjacent instructions in the same routine reading the same/adjacent field,
same pattern the hibernate G1 doc used to read its own crash as "one
defect, not many.") **Read as one shared crash site, not two independent
bugs.**

## Confirmed CratonVM-specific, not a fixture issue

Both classes run clean under plain HotSpot (real JDK 25, same classpath):

```
TestKillProcessWhileWriting: exit=0, no failures
TestRandomMapOps: 7 passes completed cleanly ("Done pass #0" .. "#6"), exit=0
```

## Not yet symbolized

Same gap as the hibernate G1 doc had at its own starting point: no
`addr2line`/offline symbolization run against
`cratonvm-h2-g1-postmerge2` yet to name the faulting routine. Given the
region-boundary shape of the address and that this is G1-specific (0
crashes on the same classes under the default collector or ZGC), the
natural first hypothesis — worth checking against, not assuming — is the
same G1 evacuation/region-walk family the hibernate doc already
root-caused as far as "the evacuator is being handed reclaimed memory" and
"a full Eden region is not walkable object-by-object" (see that doc's
2026-08-07/08 updates). Both `TestKillProcessWhileWriting` and
`TestRandomMapOps` are heavy concurrent/randomized store-stress workloads
(the class names describe exactly that), which fits the profile of the
kind of allocation pressure that doc's finding needs to reproduce.

### Repro

```
cd apps/h2database-suite-runner
CRATONVM_BIN=/data/cratonvm/target-zgc/release/cratonvm-h2-g1-postmerge2 \
  ./run-h2-suite.sh run --category all \
  --only '(^|\t)org\.h2\.test\.store\.(TestKillProcessWhileWriting|TestRandomMapOps)$' \
  --jit on --jdk real
```
(H2's runner has no native `EXTRA_VM_ARGS`-style passthrough for the GC
flag — `-XX:+UseG1GC` is baked into which binary you point `CRATONVM_BIN`
at, per the `cratonvm-h2-g1-postmerge2` naming convention established this
session.) Only observed once each so far (one run per class this round) —
not yet confirmed deterministic across repeated launches.

## Recommended next steps

1. Symbolize: `addr2line -e cratonvm-h2-g1-postmerge2 <offset>` (or
   equivalent for this binary's build) against the fault PC offset above to
   name the routine, same first move the hibernate doc made.
2. Repeat the repro 3-5x to establish whether it's deterministic (the
   hibernate doc's equivalent crash was 4/5, not 5/5) before assuming
   every run of these two classes crashes.
3. If the routine names to G1's evacuation/ref-scan path, read
   `../hibernate/g1-collector-fullsuite-crashes-hangs-fails-20260806.md`'s
   2026-08-07/08 updates in full before starting a fresh investigation —
   the guards and hypotheses that doc already tested (candidate/holder
   region-membership guards, the SATB-staleness theory it refuted, the
   Eden-region-desync finding it left open) likely transfer directly and
   would save re-deriving them from scratch.

## Related

- `../hibernate/g1-collector-fullsuite-crashes-hangs-fails-20260806.md` —
  the closest prior investigation of this exact shape (shared G1
  fault-site SIGSEGV, region-aligned address, not yet fully root-caused as
  of its last update). Read first.
- `gc-corruption-guard-fixed-by-dev-merge-20260810.md` (this folder) — the
  `dev`-merge context this rerun came from; confirms this is a *different*,
  still-open defect, not a recurrence of the now-fixed corrupt-Value-cell
  guard (that guard: 0 hits in every log from this round, checked).
- `gc-variant-fullsuite-crashes-hangs-fails-20260810.md` (this folder) —
  round-1's H2 findings, including a *different* SIGSEGV shape
  (`addr=0x10`, fault in `libc.so.6`, reproduces on all 3 GC variants) —
  do not conflate the two; different address, different variant scope,
  almost certainly different root cause.
