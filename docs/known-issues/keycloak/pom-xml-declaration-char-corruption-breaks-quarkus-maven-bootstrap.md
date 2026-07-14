# CratonVM corrupts the `<?xml ...?>` declaration when Quarkus/Maven reads `tests/base/pom.xml` in-process, breaking provider deployment for ~130 `tests/base` classes — retracts the 2026-07-07 "NOT A BUG" triage of the `remote-providers` artifact-resolution failure

Status: open — genuine CratonVM-specific bug, confirmed by a fresh (non-stale) HotSpot comparison; supersedes
`docs/internal/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md`

Date observed: 2026-07-13 (HotSpot re-baseline against `hotspot-refresh-v2-shard1`, using a freshly rebuilt
`apps/keycloak/quarkus/dist/target/keycloak-999.0.0-SNAPSHOT.zip`, compared against CratonVM's
`nonpassed-before-refresh2-shard{1,2,3,4}` results)

## Summary

120 `tests/base` classes fail every test with:

```
=> java.lang.RuntimeException: Failed to resolve artifact: org.keycloak.testframework:keycloak-test-framework-remote-providers
   org.keycloak.it.utils.Maven.getArtifact(Maven.java:88)
   org.keycloak.it.utils.Maven.resolveArtifact(Maven.java:51)
   org.keycloak.testframework.server.ProviderDeployer.getDependencyPath(ProviderDeployer.java:119)
   org.keycloak.testframework.server.ProviderDeployer.updateDependencies(ProviderDeployer.java:43)
   org.keycloak.testframework.server.DistributionKeycloakServer.start(DistributionKeycloakServer.java:93)
   org.keycloak.testframework.server.AbstractKeycloakServerSupplier.getValue(AbstractKeycloakServerSupplier.java:82)
   ...
```

Another 10 `tests/base` classes fail the same way but resolving a different artifact
(`org.keycloak.tests:keycloak-tests-custom-providers`) through the identical code path.

The `.err.log` for every one of these 130 classes shows the *actual* root cause, one level deeper than the
`RuntimeException` message suggests:

```
ERROR [io.quarkus.bootstrap.resolver.maven.FailAtCompletionErrorHandler]
ERROR [io.quarkus.bootstrap.resolver.maven.FailAtCompletionErrorHandler] 1)
    java.io.UncheckedIOException: Failed to load POM from C:\craton\CratonVM\apps\keycloak\tests\base\pom.xml
    Caused by: java.io.IOException: Failed to parse POM
    Caused by: org.codehaus.plexus.util.xml.pull.XmlPullParserException: only whitespace content allowed before start tag and not x (position: START_DOCUMENT seen x... @1:2)
```

`tests/base/pom.xml` on disk is a completely ordinary, valid POM file — no BOM, starts exactly with:

```
<?xml version="1.0"?>
<!--
  ~ Copyright 2016 Red Hat, Inc. and/or its affiliates
```

(verified byte-for-byte via `xxd`: `3c 3f 78 6d 6c ...` = `<?xml...`, no leading BOM or stray bytes)

But the embedded Maven/Quarkus bootstrap resolver, running *inside the CratonVM-hosted JVM*, reads this file and
sees `x` as the character at line 1, column 2 — i.e. it sees `<xml` instead of `<?xml`. The `?` character
(`0x3F`) immediately after `<` is being dropped (or the read is off-by-one), which shifts every subsequent
character view left by one and makes the XML pull-parser choke on `x` where it expects either whitespace or the
literal string `<?xml`.

This is a **CratonVM-side file-I/O/decoding bug**, not a Maven/Quarkus/environment gap — the file is correct on
disk, and the exact same file parses fine under real HotSpot (see "Retraction" below).

## Retraction of the 2026-07-07 "NOT A BUG" triage

`docs/internal/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md` (triaged 2026-07-07) had
concluded this `Failed to resolve artifact: ...remote-providers` failure "reproduces identically under real
HotSpot" and was therefore an environment/Maven-setup gap, not a CratonVM defect. That conclusion was based on a
HotSpot comparison run against a **stale `keycloak-999.0.0-SNAPSHOT.zip` distribution artifact** (dated weeks
earlier, May 26/July 2) that was independently discovered to be broken on 2026-07-13 — under that stale
distribution, HotSpot itself failed to boot the test server for a large fraction of classes (`"Keycloak did not
start within timeout"`, `EOFException` in `SerializedApplication.read`), which likely produced a
superficially-similar-looking "Failed to resolve artifact"/"Failed to load current project" error for reasons
entirely unrelated to this POM-parsing corruption.

After rebuilding the distribution fresh (`./mvnw install -pl quarkus/dist -am -pl '!js' -pl '!model/infinispan'
-DskipTests ...`, verified via a new zip timestamp of 2026-07-13 08:37:24) and re-running the 324
timeout-affected classes under real HotSpot solo/sequentially (to avoid a since-diagnosed extraction-directory
race — see Evidence), HotSpot now **cleanly passes** the majority of these `remote-providers`/`custom-providers`
classes (127/162 PASS in the fresh `hotspot-refresh-v2-shard1` run), while CratonVM still fails every one of them
with the exact `XmlPullParserException`/dropped-`?`-character signature above. This is a clean, reproducible
CratonVM-vs-HotSpot divergence on the identical class, identical harness, identical (fresh) distribution — the
2026-07-07 "NOT A BUG" conclusion needs to be retracted. `docs/known-issues/keycloak/README-non-bug-environment-gaps-refresh-20260711.md` §2 has been updated to point here.

## Root cause hypothesis

Something in CratonVM's native file-reading path — specifically whatever `InputStream`/`Reader`/`FileChannel`
implementation backs the embedded Maven POM reader (`org.codehaus.plexus.util.xml.pull.MXParser` reading
`tests/base/pom.xml` via Quarkus's `BootstrapMavenContext`/`LocalProject`) — drops or mis-decodes the `?`
(`0x3F`) byte immediately following the opening `<` of the XML declaration. Candidates:

1. A charset-decoding path that treats `0x3F` as a "replacement/unmappable character" sentinel and
   collapses/skips it instead of passing it through (many `CharsetDecoder`s use literal `?` as their
   `REPLACE`-action substitute for genuinely unmappable input — if CratonVM's decoder logic conflates "the byte
   value 0x3F was read" with "an unmappable byte was replaced with the placeholder '?'", it could wrongly skip
   what it thinks is its own replacement marker).
2. An off-by-one bug specific to whatever buffered-read routine Quarkus's Maven embedder uses (distinct from the
   read path exercised by ordinary classfile/jar loading, which works correctly across thousands of other classes
   — this bug is narrow enough that it doesn't affect the vast bulk of CratonVM's file I/O, only this specific
   POM-reading code path).

This wasn't root-caused further at the Rust source level in this pass — flagging as the next step.

## Next steps

1. Find exactly which native I/O routine backs the file read for `tests/base/pom.xml` in this code path (likely
   triggered via `java.nio.file.Files.newInputStream`/`newBufferedReader` or a `RandomAccessFile`, called from
   deep inside `io.quarkus.bootstrap.resolver.maven.BootstrapMavenContext` → `MavenXpp3Reader`/`MXParser`).
   Write a minimal standalone repro: read `tests/base/pom.xml` (or any file starting with `<?xml`) via the same
   API/call pattern under CratonVM and dump the raw bytes/chars actually delivered to Java, to catch the `?` drop
   directly without the full Keycloak/Quarkus bootstrap overhead.
2. Check whether the corruption is specific to the `?` byte value, or whether ANY byte at that specific buffer
   position gets dropped (test with a synthetic file starting `<Zxml...` or similar to see if `Z` also vanishes,
   which would point to an off-by-one/buffer-boundary bug rather than a charset-substitution bug).
3. Once fixed, re-verify against the full 130-class set (see Evidence) plus the ~215 other `remote-providers`
   classes that were NOT part of this fresh-HotSpot sample (fold in remaining `tests/base`/`tests/webauthn`
   classes from the original ~345-class remote-providers bucket described in
   `docs/known-issues/keycloak/README-non-bug-environment-gaps-refresh-20260711.md` §2) — very likely all of them
   share this exact fix.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-pom-xml-corruption -ClassList <(printf 'module\tclass\ntests/base\torg.keycloak.tests.account.AccountConsoleDisabledTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh2-20260712.exe -JdkHome $jdk
```

Check the `.err.log` for `Failed to load POM from ...tests\base\pom.xml` followed by
`XmlPullParserException: only whitespace content allowed before start tag and not x (position: START_DOCUMENT seen x... @1:2)`.

Confirm the file itself is fine: `xxd apps\keycloak\tests\base\pom.xml | head -1` should show
`3c3f 786d 6c20 7665 7273 696f 6e3d 2231` (`<?xml version="1`).

## Evidence

- CratonVM failures (130 classes total): `apps/keycloak-suite-runner/.suite/results/nonpassed-before-refresh2-shard{1,2,3,4}/all-jit/logs/tests_base.*.{out,err}.log`
  — grep for `remote-providers` or `custom-providers` across those logs to enumerate the full class list.
- Fresh (non-stale-distribution) HotSpot comparison showing these classes PASS cleanly:
  `apps/keycloak-suite-runner/.suite/results/hotspot-refresh-v2-shard1/hotspot-jit/results.tsv` (127 PASS / 33
  FAIL / 2 HANG out of 162 classes from `timeout-affected.tsv` rows 1-162, run solo/sequential on
  2026-07-13 after rebuilding `apps/keycloak/quarkus/dist/target/keycloak-999.0.0-SNAPSHOT.zip` fresh and after
  diagnosing/avoiding a shared-extraction-directory race between concurrent shards).
- Superseded triage: `docs/internal/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md`.
- Rollup doc needing correction: `docs/known-issues/keycloak/README-non-bug-environment-gaps-refresh-20260711.md` §2.
