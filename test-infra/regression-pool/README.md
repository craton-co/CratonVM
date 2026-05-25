# CratonVM regression pool

A **permanent** test pool of real Java apps used to:

1. **Catch regressions** — every commit can run `run.sh` and compare against
   committed baselines (per-app `expected.txt` + duration envelope) to flag
   "passed before, fails or got slower now."
2. **Benchmark** — wall-clock + peak heap recorded per probe so JIT / GC /
   intrinsic changes can be evaluated against a stable workload set.
3. **Provide a stable counter-balance to `apps/`** — the gauntlet's `apps/`
   directory is auto-managed by orchestrator agents and apps get deleted
   on pass. The regression pool **never deletes** apps; staged distros
   live under `test-infra/regression-pool/apps/` and probes live under
   `test-infra/regression-pool/probes/` (or reuse `test-infra/probes/`).

## Layout

```
test-infra/regression-pool/
├── README.md           # this file
├── pool.tsv            # tab-separated table: one row per (app, probe) pair
├── stage.sh            # idempotent: downloads any missing app distros into apps/
├── run.sh              # runs every row in pool.tsv; emits results/<timestamp>.tsv
├── bench.sh            # wall-clock + RSS per probe; emits results/<timestamp>.bench.tsv
├── compare.sh          # diffs current run vs baselines/ — prints regressions
├── apps/               # gitignored, all distros land here (multi-GB total)
├── baselines/          # committed expected output + max duration per probe
│   └── <name>.expected.txt
└── results/            # gitignored per-run reports
```

## pool.tsv format

Tab-separated, one row per probe. Columns:

| # | Column           | Notes |
|---|------------------|-------|
| 1 | `name`           | Short stable id — used for filenames + baseline lookup. |
| 2 | `app`            | App+version key — first column of `stage.sh`'s download table. |
| 3 | `probe_class`    | Java main class. |
| 4 | `probe_dir`      | Path to the dir containing the probe `.class` files (relative to repo root). |
| 5 | `classpath_glob` | Glob pattern (semicolon-separated) of jars to add to the probe's CP. May reference `$APPS_ROOT` (= test-infra/regression-pool/apps). |
| 6 | `args`           | Args passed to the probe (use `-` for none). |
| 7 | `max_seconds`    | Soft upper bound on wall time; runs longer than this flag as a perf regression. |
| 8 | `baseline_file`  | Filename in `baselines/`. Compare against `expected.txt` after stripping volatile prefixes. |

Lines starting with `#` are comments. Lines with empty `name` are skipped.

## Adding an app

1. Add a download line to `stage.sh`'s `URLS` table (curl URL + extract path).
2. Add 1+ probe row(s) to `pool.tsv` (reuse `test-infra/probes/<name>_probe/`
   if one exists, else create one).
3. Run `bash run.sh --record-baseline <name>` once on a known-good build to
   capture `baselines/<name>.expected.txt`.
4. Commit the new `pool.tsv` row + `baselines/<name>.expected.txt` (NOT the
   downloaded distros — those are gitignored).

## Running

```
# stage all distros (idempotent; skips ones already on disk)
bash test-infra/regression-pool/stage.sh

# run every probe; compare against baselines
bash test-infra/regression-pool/run.sh
# → prints PASS/REGRESS/SLOWER per row, writes results/<ts>.tsv

# benchmark mode (5 iterations per probe, records peak RSS via tasklist)
bash test-infra/regression-pool/bench.sh
```

## What's in the pool today

| Name | App | Probe |
|---|---|---|
| wildfly32-modload | wildfly-32.0.1.Final | LocalModuleLoader.loadModule("org.jboss.logging") |
| wildfly40-modload | wildfly-40.0.0.Final | LocalModuleLoader.loadModule("org.jboss.logging") |
| kc16-modload | keycloak-16.1.1 | LocalModuleLoader.loadModule("org.keycloak.keycloak-services") |
| kc26-version | keycloak-26.2.4 | Version.NAME + Version.VERSION + Profile.Feature.values() |
| kafka-codec | kafka_2.13-3.7.0 | AppInfoParser + StringSerializer round-trip + ConfigDef |
| hadoop-conf | apache-hadoop-3.4.0 | Configuration set/get round-trip for 4 type kinds |
| hbase-conf | apache-hbase-2.5.10 | HBase config with namespaced keys |
| spring-boot-run | spring-boot 4.0.6 + 3.2.0 | SpringApplicationBuilder.run() with @Bean lookup |
| jenkins-load | jenkins-core 2.452.3 | jenkins.ClassLoaderReflectionToolkit |
| tomcat-server-info | apache-tomcat-10.1.31 | ServerInfo + WebXml + UDecoder |
| solr-doc | solr-9.5.0 | SolrInputDocument + DocumentObjectBinder |
| cassandra-util | apache-cassandra-4.1.4 | FBUtilities + UUIDGen + ByteBufferUtil round-trips |
| activemq-openwire | apache-activemq-5.18.3 | ProducerId/MessageId/ActiveMQTextMessage marshal round-trip |
| felix-lifecycle | felix-framework-7.0.5 | FrameworkFactory.init().start()...stop() — ACTIVE state check |

## Why this exists

The `apps/` gauntlet rule says "when an app passes, delete it from `apps/`
and refill from the master list." That makes the gauntlet a moving target:
yesterday's passing app is gone today, so a *new* regression in JIT or GC
won't be caught against any of the apps that have ever passed.

The regression pool fixes that: every app that ever passed stays here
forever, with a committed baseline. Future runs flag any deviation from
"the way this worked when we shipped it."
