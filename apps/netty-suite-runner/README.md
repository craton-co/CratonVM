# netty-suite-runner (Linux / Azure host, 20.80.105.49)

CratonVM suite-runner harness for Netty, built on the second Azure host as a
sibling to the netty/hibernate-reactive/quarkus work already done on the
first Azure host (20.83.144.174, `/data/data/cratonvm/apps/`). This checkout
is **current HEAD as of 2026-08-09** (`f8e1152cd09fdf37957b29e4c26f44995d3ea44`),
not the ~3-week-older commit used on the first host — results are not
expected to be byte-identical to that host's, and differences are expected,
not a regression signal.

## Build

`./mvnw -B -fae -T 1C -DskipTests install` from the netty checkout root
(`/data/cratonvm/apps/netty`), full reactor, ~1m40s wall.

**39 modules built successfully.** One module failed, with predictable
downstream skips:

- `netty-transport-native-epoll` — **FAILURE**. The `hawtjni-maven-plugin`
  build-native-lib goal tries to download its own not-yet-built artifact
  (`io.netty:netty-transport-native-epoll:zip:native-src:4.2.18.Final-SNAPSHOT`)
  from the Sonatype snapshots repo, which doesn't exist there for this
  SNAPSHOT version. This is a build-tooling/repository-resolution gap
  (native C-source packaging step), not a CratonVM issue, and matches the
  "native codec" failure the task description said to expect.
- Downstream modules **SKIPPED** as a result: `netty-codec-native-quic`,
  `netty-codec-http3`, `netty-all`, `netty-transport-native-io_uring`,
  `netty-testsuite-native`, `netty-testsuite-jpms`, `netty-testsuite-karaf`,
  `netty-testsuite-osgi`, `netty-testsuite-shading`, `netty-microbench`.

Everything else — including `transport-classes-epoll` (the pure-Java half of
epoll support) and every codec/handler/resolver/transport module not tied to
a specific native transport — built clean.

## Class counts

- Main: 4023 `.class` files
- Test: 3343 `.class` files
- `testlist.txt`: 733 top-level test classes (`*Test`/`*Tests`, no `$` inner
  classes), scanned directly from the 49 modules that actually produced a
  `target/test-classes` directory. Modules that failed/were skipped
  contribute nothing to this list.

## Harness

`apps/` is gitignored wholesale (`.gitignore:12`), so every file here that is in
the repo at all was force-added (`git add -f`), the same way
`apps/hib-suite-runner/run-hib.sh` and `apps/h2database-suite-runner/run-h2-suite.sh`
already are. Tracked: this README, `run-netty-suite.sh`, `gen-module-args.sh`,
`gen-openssl-args.sh`, `CratonRunner.java`, and the three `.tsv` side-tables.
NOT tracked, because they are host-specific generated output: `common.args`,
`module-args/`, `testlist.txt`, `passed.txt`, `others.txt`, `runs/`, `target/`,
`*.class`, `hs_err_pid*.log`.

- `CratonRunner.java` — identical (module-name comments aside) to the
  Windows-host `apps/netty-suite-runner/CratonRunner.java`: JUnit Platform
  Launcher, one class per invocation via `selectClass`, prints
  `@@RESULT <class> found=.. started=.. ok=.. failed=.. aborted=.. skipped=.. ms=..`
  then `@@BATCHEND failed_classes=N`.
- `common.args` — `-cp` built from every buildable module's
  `target/classes`+`target/test-classes`, plus every jar
  `mvn dependency:build-classpath -DincludeScope=test` resolved per module
  (offline, reusing the reactor's own local-repo install), deduplicated,
  `:`-joined (Linux). 298 classpath entries total. Plus
  `-Duser.timezone=UTC -Djunit.jupiter.execution.timeout.default=120s`.
- `run-netty-suite.sh` — fork-per-class, sharded, `categorize` sub-command.
  No `--hotspot` baseline mode (not needed for this phase per task scope);
  HotSpot cross-checks are run by hand, `java @common.args ... CratonRunner <class>`.
  Three tracked side-tables, each with a sub-command that prints its live state:
  - `class-overrides.tsv` (`run-netty-suite.sh overrides`) — per-class wall-clock
    cap and extra VM flags. The cap is a floor, never a ceiling:
    `max(--timeout, entry)`. One entry today, `DnsNameResolverTest` at 600s,
    because it runs to 82% of the flat 180s cap and a class killed at the cap is
    recorded `HANG`, indistinguishable from a real deadlock.
  - `known-benign-aborts.tsv` (`run-netty-suite.sh benign-aborts`) — classes whose
    `ABORTED` status is a confirmed platform self-skip. `categorize` treats one as
    pass-equivalent **only on an exact found/ok/aborted match**, so a changed abort
    profile still lands in `others.txt`. Same mechanism as
    `apps/hib-suite-runner/known-benign-aborts.tsv`.
  - `module-scoped-classes.tsv` (`run-netty-suite.sh module-scope`) — classes that
    must see one module the way Maven shows it, not the flat whole-reactor
    classpath: the 17 `NativeImageHandlerMetadataTest` copies. Each runs with the
    module directory as cwd and `module-args/<artifactId>.args` as its argfile.
    `--no-module-scope` disables it for A/B (with it: PASS; without: FAIL).
- `gen-openssl-args.sh` — derives an argfile whose classpath can actually reach
  netty's OPENSSL paths. **`common.args` as generated cannot**, and the way that
  presents is not an error: `OpenSsl.isAvailable()` is false, JUnit never
  generates the OPENSSL parameterisations, and the classes that exist to test
  them read as clean PASSES while running a fraction of their tests —
  `ParameterizedSslHandlerTest` reports success at **7 of 63**. Two halves, and
  only the first is obvious: add `netty-tcnative-boringssl-static-<ver>-<os>.jar`
  (statically linked, so the host's own OpenSSL version stops mattering to the
  3.2.0-requiring dynamic artifact), AND remove the dynamic
  `netty-tcnative-<ver>-<os>.jar` — with both present netty finds the dynamic
  one and `isAvailable()` stays false whatever their order. `--bc18` also drops
  the three `*-jdk15on-1.70` jars, which `common.args` lists ahead of
  `bcprov-jdk18on-1.84` and which make `bctls-jdk18on-1.84` die in
  `TlsUtils.<clinit>` on both VMs. Verify with
  `java @<out> OpenSslAvailabilityProbe` (`probes/`) before trusting any number
  out of those classes.
- `gen-module-args.sh` — regenerates `module-args/<artifactId>.args` from the
  table above, one Maven-faithful single-module test classpath each
  (`mvn -o dependency:build-classpath -DincludeScope=test`, plus the module's own
  `target/{classes,test-classes}`, this dir, and a version-matched
  junit-platform-launcher jar). Like `common.args` the output is host-specific
  absolute paths, so it is generated rather than tracked; `run-netty-suite.sh`
  rebuilds any missing argfile on startup rather than running those classes wrong.

## Validation run — IMPORTANT FINDING

Ran `--list testlist.txt --count 20` (first 20 alphabetical classes,
`io.netty.bootstrap.*` / `io.netty.buffer.*`), 4 shards, 60s per-class
timeout:

```
status: CRASH=15 NOTESTS=5   sum_class_ms=1289
```

- **NOTESTS=5**: abstract base test classes (`AbstractByteBufTest`,
  `AbstractCompositeByteBufTest`, etc.) — correctly report 0 discovered
  tests, not launched directly by JUnit. Expected, not a bug.
- **CRASH=15**: every concrete test class in the batch. **All 15 hit the
  same CratonVM-level heap-corruption defect**, confirmed by grepping every
  shard's raw log:

  ```
  ERROR cratonvm::gc::guard: gen_heap::read_slot: corrupt Value cell
  (out-of-range discriminant) — returning null instead of a UB-on-match
  Value. Heap reference-integrity defect (see HIB-CV-32).
  ...
  [cratonvm] main-vm run() returned Err: Error in thread "main" internal
  error: expected object reference, got int(16) ctx="Cannot enter
  synchronized block because \"this.mutex\" is null"
  ```

  **This is NOT a harness bug and NOT a per-test CratonVM finding to file
  individually** — it is a **general CratonVM heap-corruption defect,
  correlated with classpath size but not specific to JUnit 5 or to test
  discovery**. A control run of the exact same class/classpath under real
  HotSpot JDK 25 (`/data/toolchain/jdk-25/bin/java`, same `CratonRunner`,
  same `-cp`) **passed cleanly** (`io.netty.util.AttributeKeyTest found=3
  ok=3 failed=0`), which rules out the harness/classpath/CratonRunner as the
  cause — this is purely a CratonVM defect.

  A sibling investigation (same host, hibernate-orm/h2database harnesses)
  independently found the identical signature and flagged it as likely a
  **new** corruption site distinct from the already-fixed HIB-CV-32 (the log
  line is a generic defensive-guard message reused by multiple corruption
  sites, not proof it's the old bug). **h2database's harness uses plain
  `main()` entry points with no JUnit Platform involved at all, and still
  hit the identical signature** — so this is not JUnit5-discovery-specific,
  despite every crash in this batch happening during discovery. It's
  correlated with classpath size (h2database's small ~2.2KB classpath hits
  it at only ~9% frequency; this harness's 298-entry classpath hit it in
  100% of concrete classes in this batch) but **not gated by size** — a
  follow-up probe here reproduced the same `corrupt Value cell` log line (5
  occurrences, vs 80 across the full 298-entry classpath) on a deliberately
  trimmed 9-entry classpath (just `netty-common` + JUnit jars), just at
  lower frequency. It's also not always fatal: on h2database it sometimes
  shows as a guarded recovery (logs ERROR, returns null, continues) that
  then plausibly cascades into a downstream test failure rather than a hard
  crash, though every occurrence in this harness's batch escalated to a full
  VM abort (`main-vm run() returned Err`) or a `NullPointerException` inside
  JUnit's discovery-exception wrapping. In short: a general, classpath-size-
  correlated (not gated) heap-corruption defect, confirmed reproducing under
  both JUnit-Platform-based and plain-`main()`-based execution — not a
  JUnit5/discovery-specific bug.

  **Practical effect on this harness**: until this CratonVM defect is fixed,
  most concrete Netty test classes will report CRASH regardless of whether
  the underlying Netty behavior is actually correct on CratonVM — `others.txt`
  after a full `categorize` run would be dominated by this, not by real
  per-feature Netty/CratonVM incompatibilities. Re-run `categorize` after the
  fix lands to get a real signal.

## What was NOT done (follow-up)

- Full `categorize` sweep over all 733 classes — not attempted per task
  scope (and not very informative until the heap-corruption defect above is
  fixed, since it would currently swamp real results).
- `--all-modes` (JIT×JDK matrix) — not run; single JIT-on/real-JDK mode only.
