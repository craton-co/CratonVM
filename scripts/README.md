# Repository automation

`scripts/` is the canonical location for operator commands. `tools/` holds
validation helpers, compiler probes, benchmark sources, and their maintained
fixtures. Proposals, machine-specific experiments, and unreferenced campaign
artifacts belong in the ignored local `docs/internal/` archive.

| Task | Entry point |
|---|---|
| CPU/GPU builds on Windows | `build-cpu.bat`, `build-gpu.bat`, `find-vcvars.bat` |
| Embedding builds | `build-libcratonvm.ps1` |
| Generic application launch | `run-app.sh`, `run-app.ps1` |
| Local CI shards | `ci-docker.sh` |
| Registry metadata and dry-runs | `release-crates-dry-run.ps1` |
| HotSpot baseline capture | `capture-hotspot-baseline.sh`, `capture-hotspot-baseline.ps1` |
| Profile-guided builds | `pgo.sh`, `pgo.cmd` |
| Framework result comparison | [verify/README.md](verify/README.md) |
| Native registry census | `jdk-only-census.sh` |
| Foundational execution gates | `jdk-only-strict-probes.sh`, `jdk-only-blast-radius.sh` |
| Safety audits | `stale-receiver-audit.py`, `unpinned-native-local-audit.py`, `untyped-alloc-ratchet.sh` |
| Documentation | `python tools/check_markdown_links.py` from the repository root |

Real-JDK execution is the launcher default. Pass `--compatible` when explicitly
measuring the optimization layer. Script filenames and machine-readable mode
identifiers retain their established names; commands use `--real-jdk`.
`scripts/baselines/` contains checked-in gate inputs and JDK surface tables.
External framework checkouts must be supplied by the operator under `apps/`.

## Application and local CI commands

`run-app.sh` / `run-app.ps1` build a classpath from an explicit classpath file,
an application-local `craton-testcp.txt`, or compiled classes and library JARs.
They capture stdout, stderr, exit status, and elapsed time for one main class or
JAR. Framework-specific discovery and sharding remain in the suite runners.

`ci-docker.sh` requires Docker. It runs three crate-test shards and a fourth
format/lint/build/documentation shard, with separate target volumes. Use
`--shard <1-4>` for one shard or `--sequential` to limit parallelism. It does
not reproduce CI jobs requiring JDK-image matrices, nightly tooling, or secrets;
passing the local shards does not establish those jobs passed.

## Maven and framework smoke tests

[tools/sync-cratonvm-maven-jdk.ps1](../tools/sync-cratonvm-maven-jdk.ps1) copies
the selected `java.exe` launcher into `target/cratonvm-maven-jdk/bin/java.exe`
for Maven/Surefire's JDK path check. `sync-maven-java-shim.ps1` is its wrapper.
Use the canonical implementation for behavior changes.

`scripts/smoke/` contains framework smoke lanes; `common.sh` owns shared setup.
The lane scripts describe their external fixture prerequisites. A successful
smoke run covers that workload and is not a complete framework-suite result.
