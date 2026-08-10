# H2 full-suite GC-variant run (default / G1 / ZGC) — 1 shared SIGSEGV site, 1 GC-specific corruption guard, 55-57 non-passing classes per variant

**Status:** OPEN (2026-08-10). Full 218-class H2 suite (`org.h2.test.*`), 3 GC
variants (`-XX:+UseG1GC`, `-XX:+UseZGC`, default/Generational), 1 shard each,
real JDK 25, JIT on. Binary: one `--features zgc` build serving all three
variants via runtime flag, built post-merge of `origin/dev` into `main`
(commit `681b5c1f1`, was `70bf05ed3` pre-merge). Results:
`apps/h2database-suite-runner/out/gcvariant-{default,g1,zgc}-jit-real-all-20260810-*/results.tsv`
(full 218-class sweep) and
`apps/h2database-suite-runner/out/gcvariant-{default,g1,zgc}-jit-real-nonpassed-postmerge-*/results.tsv`
(post-merge targeted rerun of just the non-passing classes, which is what
this doc's per-class evidence is drawn from — it reflects current, not
stale pre-merge, status).

## Headline counts

| variant | PASS | FAIL | CRASH | HANG | total |
|---|---:|---:|---:|---:|---:|
| default | 165 | 16 | 18 | 19 | 218 |
| G1 | 163 | 15 | 20 | 20 | 218 |
| ZGC | 163 | 22 | 18 | 15 | 218 |

(PASS counts include the 2-per-variant classes that newly passed after the
`dev` merge — see `../../internal/fixed-suite-bugs/h2-suite-bugs/gc-corruption-guard-fixed-by-dev-merge-20260810.md` in
this same folder for that story; this doc covers what's *still* broken.)

## Two bugs the old pre-merge investigation conflated — now separable

Before the `dev` merge, essentially every non-passing class carried some
flavor of `cratonvm::gc::guard` log line, and it was hard to tell how many
distinct bugs that represented. Post-merge, with the (apparently already-
fixed-on-dev) `HIB-CV-32`-labeled corrupt-Value-cell guard now producing
**zero hits across all three variants**, two things become visible
separately:

### 1. A SIGSEGV with one shared fault site — 18-22 classes per variant, all three GC modes

Every H2 CRASH this session sampled has the identical shape:

```
#  SIGSEGV at pc=0x<ASLR-base>a152a, addr=0x10, pid=<pid>, tid=<tid>
#  jdk mode: real-jdk
#  r10=0xfffffffffffffe33 r11=0x1 rsp=0x... rbp=0x0
#  fault pc is in NO recently freed code buffer
#  fault pc is in NO live registered code buffer
#  maps: fault pc IS MAPPED - perms are on the `here` line
#    here: ...  r-xp ...  /usr/lib/x86_64-linux-gnu/libc.so.6
#  slot[r10]: UNREADABLE (r10 is not a readable pointer)
```

Four independently-launched crashes (`TestIndex`, `TestMVStore`, and two
others from the default-variant rerun) all fault at an address ending in
`...a152a` — the low bits of the PC are identical across separate ASLR'd
process launches, which for a PIE binary only happens when the fault is at
the *same offset inside the same shared library* (here, `libc.so.6`, not
CratonVM's own code or JIT-compiled code). `addr=0x10` — a read 16 bytes
past a null pointer — with `r10` holding `0xfffffffffffffe33` (not a valid
pointer, looks like a sign-extended small negative int) suggests a native
call is being handed a corrupted/wrong-typed argument that a libc routine
then dereferences near-null. **Read as one defect, not eighteen** — same
diagnostic pattern the sibling `hibernate/g1-collector-fullsuite-*.md` doc
used for its shared-fault-site finding.

Affected classes (default variant sample; G1/ZGC counts are similar,
overlapping but not identical sets — see each variant's `results.tsv` for
the authoritative list): `TestCluster`, `TestIndex`,
`TestMultiThreadedKernel`, `TestSpaceReuse`, `TestKillProcessWhileWriting`,
`TestMVStore`, `TestMVStoreCachePerformance`, `TestRandomMapOps`,
`TestDiskFull`, `TestKill`, `TestPowerOffFs`, `TestAutoReconnect`,
`TestFileLockProcess`, `TestFileSystem`, `TestPageStoreCoverage`,
`TestReopen`, plus `TestScript`/`TestCrashAPI` (these two report `CRASH`
with exit code 1 rather than 139 — a different signal worth checking
separately before assuming they're the same libc-fault mechanism).

**Not yet symbolized** — this needs `addr2line`/offline symbolization
against the exact binary
(`/data/cratonvm/target-zgc/release/cratonvm-h2-default-postmerge`) to name
the libc routine and, from there, the CratonVM native call site that
invokes it with a bad argument. That's the natural next step; this doc
only establishes that it's one bug, not many.

### 2. A young-non-moving-sweep phantom-header guard — default GC only, 8 classes

Distinct from the SIGSEGV above, and **only observed under the default
(Generational) collector** — zero hits under G1 or ZGC, which makes sense
since "young non-moving sweep" is specific to the Generational collector's
own sweep implementation:

```
ERROR cratonvm::gc::guard: young non-moving sweep was about to ZERO a span
containing a LIVE (marked) object — the walk left the object grid and a
phantom header subsumed it. The span has been RETAINED instead of freed.
Without this check the object's header would now read all-zero and the next
use of it would fail as `java.lang.Object cannot be cast to ...`.
spans=1 victim="0x20063144550" span_bytes=197216 span_head_class_id=4044482304
```

Hit repeatedly (the same `victim` address, ms apart) in `TestMultiThread`'s
default-variant run, and seen across 8 distinct classes total in that
variant's postmerge rerun (`TestOpenClose`, `TestTempTables`,
`TestCachedQueryResults`, `TestBenchmark`, `TestMVStoreBenchmark`,
`TestMultiThreaded`, plus 2 more — grep
`apps/h2database-suite-runner/out/gcvariant-default*postmerge*/logs/` for
`'was about to ZERO a span'` for the authoritative list). Every hit shows
the guard doing its job (retaining the span instead of freeing it), so this
is a **near-miss caught by defensive code**, not a confirmed live crash —
but the guard existing at all means the young non-moving sweep's object
grid can desync from a span's real contents, which is the same *family* of
"object grid doesn't match what's actually in the region" defect the
sibling `hibernate/g1-collector-fullsuite-*.md` doc's Eden-region-desync
finding describes for G1's source walk — worth comparing mechanisms.

**Every affected class also HANGs** (all 8 are `HANG` in the results, not
a clean pass) — consistent with the guard's retry/retain path being
expensive enough, or looping, to blow the 300s per-class cap rather than
resolving. Not yet confirmed whether removing/relaxing the guard would
change HANG to CRASH (the real bug) or whether the retained span eventually
gets reclaimed correctly on a later cycle.

## Two bugs from the *original* (pre-merge) investigation that are now GONE

Both confirmed absent in every postmerge log across all three variants —
worth recording so a future run doesn't waste time re-investigating them:

- The `sun.nio.ch.FileChannelImpl.tryLock`→`FileLockTable.add` NPE
  (previously H2's dominant FAIL cause on file-backed MVStore opens) — **0
  occurrences post-merge.**
- The `HIB-CV-32`-labeled corrupt-Value-cell guard — **0 occurrences
  post-merge**, corroborating the `dev`-branch build's earlier clean
  30,000-class-run sweep. See
  `../../internal/fixed-suite-bugs/h2-suite-bugs/gc-corruption-guard-fixed-by-dev-merge-20260810.md` for the full story
  (14 classes across both apps newly pass because of this).

## FAILs not yet categorized

15-22 FAIL-status classes per variant remain, with a mix of real assertion
failures (`java.lang.AssertionError: Expected: X actual: Y` —
`TestFunctions`, `TestTransaction`, `TestBnf`, `TestMemoryUnmapper`),
H2-internal JDBC exceptions wrapping various causes (`TestLargeBlob`,
`TestPowerOffFs2`, `TestSynth`, `TestMulti`), and a few environment-shaped
ones (`TestJoin`/`TestPgServer` reference `org.postgresql.Driver`
connection errors — likely a Postgres-not-reachable environment gap, not a
CratonVM bug, same category as the hibernate-reactive suite's DB-required
classes elsewhere in this project). None of these were individually
triaged this session — see each variant's `results.tsv` for the complete,
current list (`grep -v PASS`).

## Related

- `../../internal/fixed-suite-bugs/h2-suite-bugs/gc-corruption-guard-fixed-by-dev-merge-20260810.md` (this folder) — the
  before/after merge story for both h2 and spring-framework.
- `../hibernate/g1-collector-fullsuite-crashes-hangs-fails-20260806.md` —
  the closest prior investigation of this shape (shared-fault-site SIGSEGV
  under G1, object-grid/region-walk desync family). Worth reading before
  starting the symbolization work above; the diagnostic playbook there
  (region-aligned fault address, `addr2line` against the exact binary,
  guard-then-measure) applies directly here.
- `!bug-h2-suite-fail-cluster-likely-not-cratonvm-bugs-20260807.md` — an
  earlier H2 fail-cluster triage; check for overlap before re-triaging the
  FAILs listed above.
