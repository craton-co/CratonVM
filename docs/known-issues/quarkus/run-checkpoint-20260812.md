# quarkus full-scope 3-GC-variant run — stopped 2026-08-12, resume state

**Status:** Run deliberately stopped to free host resources; not a failure.
This records exactly where each variant/shard stopped so the remaining
work can resume without re-running already-completed classes or
re-deriving the stop point from scratch.

## Why stopped

The full-scope run (1400/1400 quarkus modules built, 6,377-class harness,
3 GC variants × 2 shards = 6 concurrent CratonVM forks) had been running
for ~12 hours on Azure host `azureuser@20.80.105.49` and was projected to
take ~3 days per variant to reach all 6,377 classes. Results so far were
overwhelmingly clean (99.5%+ PASS, only HANGs, zero FAIL) with no sign
that continuing unattended for days would surface materially different
signal soon — stopped to free the shared host rather than let it run
indefinitely. This is a pause, not a conclusion; ~87% of the suite was
never reached.

## Progress at stop time

| variant | attempted (of 6,377) | PASS | HANG | FAIL |
|---|---|---|---|---|
| default (`-XX:+UseGenerationalGC`) | 911 | 908 | 3 | 0 |
| g1 (`-XX:+UseG1GC`) | 827 | 822 | 5 | 0 |
| zgc (`-XX:+UseZGC`) | 907 | 904 | 3 | 0 |

7 unique classes hit HANG across all 3 variants combined (see
[investigate-batch-01.md](investigate-batch-01.md)); zero FAIL anywhere.

## Exact stop point per shard

The harness stripes `testlist.txt` (6,377 lines) across 2 shards via
`NR%2==r`, so shard-0/shard-1 input is identical across all 3 GC variants
— only how far each shard got differs (each GC variant runs at a
different speed).

| variant | shard | classes done | last class attempted |
|---|---|---|---|
| default | 0 | 453 | `io.quarkus.bootstrap.resolver.maven.test.PomReposMirroredTest` |
| default | 1 | 459 | `io.quarkus.bootstrap.resolver.test.ConditionalDependenciesDevModelTestCase` |
| g1 | 0 | 414 | `io.quarkus.arc.test.unproxyable.RequestScopedFinalMethodsTest` |
| g1 | 1 | 414 | `io.quarkus.arc.test.unproxyable.ProducerAddMissingNoargsConstructorTest` |
| zgc | 0 | 455 | `io.quarkus.bootstrap.resolver.maven.test.SplitLocalRepositoryTest` |
| zgc | 1 | 452 | `io.quarkus.bootstrap.resolver.maven.test.ChainedLocalRepositoryManagerTest` |

Processes were stopped cleanly (`kill -TERM` on the orchestrators'
process group, then explicit `kill -9` on the still-running per-class
`timeout`/CratonVM forks, which spawn their own process group and
survive a parent-group signal) — no partial/corrupt results.tsv rows;
the "last class attempted" row in each shard's `results.tsv` completed
normally before the stop.

## How to resume

Per-variant, per-shard **remaining-class lists** (everything after the
stop point, in original testlist order) are saved on the host at:

```
/data/cratonvm/apps/quarkus-suite-runner/resume-checkpoints-20260812/
  remaining-default-shard0.txt   (2735 classes)
  remaining-default-shard1.txt   (2730 classes)
  remaining-g1-shard0.txt        (2774 classes)
  remaining-g1-shard1.txt        (2775 classes)
  remaining-zgc-shard0.txt       (2733 classes)
  remaining-zgc-shard1.txt       (2737 classes)
  ALL-remaining-union.txt        (5549 unique classes -- union across all variants/shards)
```

To resume a single variant exactly where it left off (2 shards, matching
the original run):

```bash
cd /data/cratonvm/apps/quarkus-suite-runner
cat resume-checkpoints-20260812/remaining-default-shard0.txt \
    resume-checkpoints-20260812/remaining-default-shard1.txt > /tmp/resume-default.txt
./run-quarkus-suite.sh --list /tmp/resume-default.txt --gc default --shards 2 --timeout 180 \
  --bin /data/cratonvm/target-zgc/release/cratonvm-quarkus-default-wrapper.sh \
  --out runs-fullscope/default-resume
# repeat for g1 / zgc with their own remaining-*-shard*.txt files
```

The existing GC-variant binaries/wrappers
(`cratonvm-quarkus-{default,g1,zgc}[-wrapper.sh]` under
`/data/cratonvm/target-zgc/release/`) are untouched and still valid to
reuse — no rebuild needed to resume, only if `dev` has moved and a fresh
build is specifically wanted first (in which case rebuild, recopy the
wrapper binaries, and resume as above).

## Related

- [investigate-INDEX.md](investigate-INDEX.md) / [investigate-batch-01.md](investigate-batch-01.md)
  — the 7 classes that showed HANG in the coverage attempted so far.
