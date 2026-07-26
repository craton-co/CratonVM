# CratonVM pom.xml XML-declaration char-corruption — could not reproduce; two real residual bugs found+fixed in the same code path — FIXED

Status: CLOSED. The originally-documented symptom (embedded Maven/Quarkus's `MXParser` seeing `<xml` instead
of `<?xml` when reading `tests/base/pom.xml`, dropping the `?` byte of the XML declaration) does not reproduce
under current `dev`, despite substantial effort to reproduce it faithfully — including building the real
Keycloak 26.6.1 Quarkus distribution from source and running the actual `keycloak-test-framework` bootstrap
end-to-end. While chasing this down, two separate, genuine CratonVM bugs were found and fixed in the exact
code path this doc's repro exercises (`DistributionKeycloakServer.start()`); a further three unrelated
bugs were newly discovered and are documented separately (not fixed here — out of scope, see "New residuals
found" below).

## Original report

See git history for the original report (`../../known-issues/keycloak/pom-xml-declaration-char-corruption-breaks-quarkus-maven-bootstrap.md`,
authored 2026-07-13 12:46, commit 057257d2b): 130 `tests/base` classes failed to resolve
`keycloak-test-framework-remote-providers`/`keycloak-tests-custom-providers` at server-bootstrap time, with the
`.err.log` for every one showing `XmlPullParserException: only whitespace content allowed before start tag and
not x (position: START_DOCUMENT seen x... @1:2)` — i.e. embedded Maven's `MXParser` decoding `tests/base/pom.xml`
saw `x` as the second character instead of `?`, as if the `0x3F` byte immediately after `<` were silently
dropped.

## What this investigation did

1. Traced the exact real-library call chain the failure's stack trace names: `io.quarkus.bootstrap.resolver
   .maven.workspace.ModelUtils.readModel(Path)` → `Files.newInputStream(path)` → `org.apache.maven.model.io
   .xpp3.MavenXpp3Reader.read(InputStream)` → `new org.codehaus.plexus.util.xml.XmlStreamReader(InputStream)`
   (BOM/prolog auto-detection via `BufferedInputStream.mark()`/`reset()` + a 4-byte ASCII pattern check) → `MXParser
   .setInput(Reader)`. Confirmed via `javap` disassembly of the actual `maven-model-builder`/`plexus-utils` jars
   used by this Keycloak/Quarkus version, not assumed from memory.
2. Wrote standalone Java probes reproducing this exact chain byte-for-byte, against the REAL
   `tests/base/pom.xml` (copied from an existing checkout on the Azure build host, byte-verified via `xxd`),
   under both real HotSpot (JDK 25) and CratonVM, for every plausible `plexus-utils` version (3.3.0 through
   3.6.1) and both `FileInputStream`- and NIO2 (`Files.newInputStream`)-backed streams. All combinations decoded
   the file identically and correctly under CratonVM — no dropped byte, no divergence from HotSpot.
3. Bisected: checked out the *exact* commit the doc was authored at (`057257d2b`), built `cratonvm` there, and
   reran the same probes. **The corruption did not reproduce at that commit either.** This means either (a) the
   documented repro (the full `run-keycloak-suite.ps1` + a stale/racing distribution zip, per this doc's own
   now-superseded predecessor) had an environmental confound of its own — the same category of mistake this
   doc's original write-up explicitly flagged and corrected for the 2026-07-07 "NOT A BUG" triage it superseded —
   or (b) the actual trigger needs the full Quarkus-embedded-Maven classloading context and was never reproducible
   via a flat classpath. Given (3) below got well past this exact code path with zero corruption, (a) is the
   more likely explanation.
4. Built the real Keycloak 26.6.1 Quarkus distribution from source (`./mvnw install -pl quarkus/dist -am -pl
   '!js' -pl '!model/infinispan' -DskipTests`, ~4m42s) plus `test-framework/remote-providers` +
   `test-framework/test-providers` (~31s), then ran an actual `tests/base` test class
   (`org.keycloak.tests.welcomepage.WelcomePageTest`) through the real `keycloak-test-framework` bootstrap
   (`DistributionKeycloakServer.start()` → `ProviderDeployer.updateDependencies()` → the exact `Maven
   .resolveArtifact` / pom.xml-reading path this doc is about) under CratonVM. This is the same mechanism
   the original 130-class failure went through, exercised end-to-end rather than in isolation.

## Result

No `XmlPullParserException`, no "Failed to load POM", no "Failed to resolve artifact" at any point — the
provider dependencies resolve and the embedded pom.xml reads cleanly. The original symptom is gone under
current `dev`. It's possible an earlier, unrelated I/O/charset correctness fix that landed later the same day
as the original report (several bulk-read and `Charset.decode(ByteBuffer)` fixes landed between 18:22 and
18:24 UTC, ~6 hours after the doc was authored at 12:46) incidentally covered whatever the real trigger was —
but since the bug didn't reproduce even at the pre-fix commit via direct repro, no single fix commit could be
conclusively identified as *the* fix. Treating this as closed given the exhaustive, byte-exact, full-harness
verification above; re-open with a fresh, precise repro if it resurfaces.

## Two new residual bugs found+fixed (same code path)

Running the real end-to-end harness surfaced two separate, previously-undiscovered CratonVM bugs that were
each blocking `DistributionKeycloakServer.start()` from completing, once the pom.xml-reading step itself
started working:

### 1. `ProcessBuilder.start()` silently produced an empty command for non-ArrayList command lists

`native_process_builder_start` (`native-io/src/process.rs`) only knew how to read the `command` field of a
`ProcessBuilder` as a raw `String[]`, or as an `ArrayList` (via its real `elementData`/`size` fields, falling
back to a synthetic-layout indexed-slot guess). `DistributionKeycloakServer.startKeycloak()` builds its command
with `new LinkedList<>()`, whose real layout has neither an `elementData` field nor a plain backing array at
slot 0 (`first`/`last` `Node` links instead) — so the read silently found nothing, and the native threw
`IllegalStateException: ProcessBuilder: no command specified` even though the command list was genuinely
populated, before ever attempting to spawn the Keycloak server process.

**Fix:** added a generic fallback that invokes the list through its public API (`size()`/`get(int)` via
`ctx.invoke_virtual`) whenever the ArrayList-shaped fast path finds nothing — correct for *any* `List<String>`
implementation regardless of internal layout (mirrors the existing `native_jarfile_stream`-style pattern of
preferring virtual dispatch over guessed field layouts elsewhere in this codebase).

### 2. `Process.descendants()` and `ProcessPipeInputStream.readAllBytes()` were entirely unregistered

- `ProcessUtils.getKeycloakPid()` calls `keycloakProcess.descendants().toList()` to tell the `kc.sh` wrapper
  script's pid apart from the exec'd `java` process's pid. `Process.descendants()` had no native registration
  at all (on either `java/lang/Process` or the VM's synthetic `Process` class), so this threw
  `NoSuchMethodError` immediately after the ProcessBuilder fix above got the server process spawned.
- `DistributionKeycloakServer.getErrorOutput()` calls `keycloakProcess.getErrorStream().readAllBytes()`.
  `readAllBytes()` is registered generically for `java/io/InputStream`, but the VM's synthetic
  `ProcessPipeInputStream` class's chain never reaches `java/io/InputStream` — the same receiver-driven-dispatch
  gap already documented (and worked around) for `java/lang/Process` itself elsewhere in this file — so it also
  threw `NoSuchMethodError`.

**Fix:**
- Implemented `Process.descendants()` for real: walks `/proc/<pid>/task/*/children` breadth-first to enumerate
  every live descendant pid (Linux-only; empty on other targets, matching this module's existing
  Linux-only process-introspection precedent), builds a real `ArrayList<ProcessHandle>`, and returns
  `list.stream()` — reusing the same "build an ArrayList, then call `.stream()`" pattern already established
  by `native_jarfile_stream` (`zip_real_jar.rs`) rather than hand-rolling a `Stream` implementation. Registered
  on both `java/lang/Process` and the synthetic `Process` class, matching every other dual-registered method in
  this file.
- Registered the already-existing generic `native_is_read_all_bytes` (`native-io/src/lib.rs`, made
  `pub(crate)`) directly on the synthetic `ProcessPipeInputStream` class too, rather than duplicating the
  bulk-read loop.

**Verification:** `cargo test -p cratonvm-native-io --lib`: 349 passed, 0 failed (no regressions). End-to-end:
the real Keycloak 26.6.1 server (built from source) now boots successfully under CratonVM via
`DistributionKeycloakServer.start()` — confirmed by running `WelcomePageTest` through the actual
`keycloak-test-framework` JUnit5 extension, past the ProviderDeployer/pom.xml-reading step, past process
spawning, past pid resolution, and past HTTP readiness — the server genuinely started and answered requests
(reaching further real per-test failures unrelated to this doc, see below).

## New residuals found (NOT fixed here — separate, unrelated to pom.xml/process spawning)

Once the server actually started, `WelcomePageTest`'s 6 test methods hit three further, entirely separate gaps
having nothing to do with XML parsing, Maven artifact resolution, or process spawning:

1. **Selenium/HtmlUnit JSON parsing failure**: `org.openqa.selenium.json.JsonException: Unable to parse: {...}`
   building a `RelativeLocator` inside `HtmlUnitDriver`'s constructor. Affects every UI-driven
   (`HtmlUnitWebDriverSupplier`) test method.
2. **Missing `sun.management` native**: `UnsatisfiedLinkError: sun/management/VMManagementImpl.getVersion0()`
   inside `PlatformMBeanProviderImpl.init()`, surfacing as `ServiceConfigurationError` the first time any code
   calls `ManagementFactory.getRuntimeMXBean()` (here: RESTEasy's Apache HttpClient engine construction, itself
   triggered building the Keycloak admin REST client).
3. Likely a downstream *effect* of (2): RESTEasy's client builder appears to catch the JMX failure and fall
   back to a legacy engine class (`org.jboss.resteasy.client.jaxrs.engines.ApacheHttpClient43Engine`) that isn't
   on this test setup's classpath at all (`NoClassDefFoundError`) — may resolve itself once (2) is fixed, or may
   be a genuine separate optional-dependency gap; not investigated further.

These affect any Keycloak integration test that constructs an admin REST client or a Selenium `WebDriver` —
i.e. most of `tests/base` — regardless of whether the pom.xml bug ever existed, so they are out of scope for
this doc. Flagged separately for follow-up.

## Files changed

- `native-io/src/process.rs` — `native_process_builder_start` generic-List fallback;
  `native_process_descendants` (new); `build_process_handle` (extracted from `native_process_to_handle`);
  `direct_child_pids`/`collect_descendant_pids` (new, `/proc`-based); `readAllBytes()` registered on
  `ProcessPipeInputStream`.
- `native-io/src/lib.rs` — `native_is_read_all_bytes` made `pub(crate)` for reuse.
