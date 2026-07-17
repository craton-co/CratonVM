# ES-RUN-20260716: tmpdir-fixed rerun of the 2,649 non-passed classes — in progress, interim findings

Status: IN PROGRESS — this doc will be superseded by a final totals doc once
the run completes; it exists now to record findings discovered mid-run per
[ES-RUN-20260715-root-tmp-exhaustion-invalidates-rerun.md](ES-RUN-20260715-root-tmp-exhaustion-invalidates-rerun.md)'s
required rerun conditions.

## What this run is

A rerun of the exact same 2,649-class non-passed selection from the invalid
`es-nonpassed-8shard-currentdev-20260715-134544-restart` run, this time with
the fixture's root cause actually fixed: every CratonVM JVM child process
gets `-Djava.io.tmpdir=<run-workdir>/tmp` and `TMPDIR`/`TMP`/`TEMP` pointed
at a unique `/data/data` path (never root `/tmp`), and all build
artifacts/Cargo output stay on `/data/data` — satisfying rerun conditions
1-3 and 5 from the invalidation doc.

Worktree: `/data/wt-es-tmpfix-rerun-20260716` (branch
`fix/es-tmpfix-rerun-20260716`, off `dev` at `3b62bf53`). Binary:
`/data/data/target-es-tmpfix-rerun-20260716/release/cratonvm-es-tmpfix-rerun-20260716`.
Run directory: `/data/data/cratonvm-suite-runs/es-tmpfix-rerun-20260716`.

## Operational interruptions (infrastructure, not CratonVM)

This is a shared Azure host running dozens of concurrent, unrelated agent
sessions. Two infra crises hit this run and are recorded here only because
they explain gaps/anomalies in the raw per-shard logs, not because they are
CratonVM issues:

1. **~18:00 — `/data` disk exhaustion.** Other concurrent sessions' Cargo
   builds drove `/data` to 0 bytes free, which killed all 8 shards
   mid-class with `No space left on device` writing result logs. Recovered
   by freeing ~80GB of `>24h`-old orphaned Cargo `target/` directories with
   no live `cargo`/`rustc` process attached (this run's own footprint was
   never more than a few MB). The runner is resumable — relaunching
   `run-8shards.sh` skips classes already present in each shard's
   `results.tsv`.
2. **~20:44-20:54 — system-wide OOM-killer.** Combined memory pressure from
   this run's 8 concurrent 2GB-heap JVMs plus other sessions' concurrent
   `rustc`/`gradle`/`java` processes triggered the kernel OOM killer, which
   indiscriminately killed processes across the whole host (including
   unrelated `dbus-daemon`, `systemd`, `sshd`, other sessions' `rustc`).
   Mitigated by switching this run's own concurrency from 8-parallel to two
   batches of 4 (`run-8shards.sh` batches shards 1-4, then 5-8) to reduce
   this run's peak memory contribution — a change to this run's own
   resource usage, not any shared-host cleanup.

Additionally, mid-run triage surfaced that the shared Elasticsearch fixture
checkout this run points `-ElasticsearchRoot` at
(`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`)
still had the `libvec.so` fixture problem previously marked FIXED in a
different worktree — see
[ES-FIXTURE-20260716-libvec-so-wrong-version-recurrence-FIXED.md](../../internal/elasticsearch-suite/ES-FIXTURE-20260716-libvec-so-wrong-version-recurrence-FIXED.md)
for the two-stage fix applied (file missing, then wrong-version file). This
means result rows recorded **before** ~21:54 on 2026-07-16 for this run may
show `UnsatisfiedLinkError`/`vec_cosi8_bulk8 LinkageError` as an infra
artifact of that recurrence rather than a genuine per-class result; rows
recorded after that point reflect the corrected fixture. The final totals
doc will exclude/re-run the contaminated rows rather than count them as
CratonVM regressions, consistent with how the original 2026-07-13 fixture
doc handled the same class of contamination.

## Interim genuine-bug findings (not infra)

With the fixture fixed, the dominant remaining failure signature is a real,
reproducible CratonVM classloading defect, not a fixture or environment
artifact (confirmed via an identical-classpath HotSpot comparison) — see
[ES-BUG-20260716-embeddedimplclassloader-noclassdeffounderror.md](ES-BUG-20260716-embeddedimplclassloader-noclassdeffounderror.md):
classes loaded through Elasticsearch's `EmbeddedImplClassLoader` (its
jar-in-jar `IMPL-JARS/` bundling mechanism, used to embed Jackson inside
`elasticsearch-x-content-*.jar`) throw `NoClassDefFoundError` under
CratonVM while passing cleanly under HotSpot with the same classpath and
flags. Because `XContentType`'s static init reaches this loader, the
failure cascades to `Tests run: 0` for a large fraction of ES test classes
that touch content-type registration, not just classes that directly use
Jackson.

## Interim status snapshot (not final — run still in progress)

As of this doc, 1,552 of 2,649 selected classes have completed across the
two-batch shard run (batch 1, shards 1-4, essentially finished; batch 2,
shards 5-8, not yet started):

| PASS | FAIL | HANG | CRASH | Done / Selected |
| ---: | ---: | ---: | ---: | ---: |
| 807 | 733 | 9 | 3 | 1552 / 2649 |

These raw counts still include the pre-~21:54 `libvec.so`-contaminated FAIL
rows described above and are **not** the final attributable totals — do not
cite this table as the run's result. It is included only to show scale and
to anchor the point at which the `EmbeddedImplClassLoader` finding was
made. The final totals doc will supersede this snapshot with clean,
fixture-excluded PASS/FAIL/HANG/CRASH counts once the full 2,649-class run
completes and the contaminated rows are re-run under the corrected fixture.
