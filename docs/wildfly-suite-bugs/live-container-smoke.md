# Live-container reruns — smoke group

The no-container per-class sweep records almost every Arquillian test as **FAIL**
(`ConfigurationException: javaHome '${container.java.home}' must exist`) because no live
WildFly server is present — identical to HotSpot, so **not** VM defects. This run adds a
**live managed WildFly container** and re-runs the smoke group to get real pass/fail.

## Setup
- Server: existing provisioned `apps/wildfly/build/target/wildfly-41.0.0.Beta1-SNAPSHOT`.
- Driver: Maven (`mvnw -f testsuite/integration/smoke/pom.xml test -Dts.smoke`), which wires
  the `wildfly-arquillian-container-managed` adapter, provisions the per-module server copy,
  sets up users/config, and boots/stops the server.
- `JAVA_HOME` = Temurin **JDK 25** → `container.java.home` defaults to it, so the **server JVM
  is HotSpot**.

## Result — HotSpot server + HotSpot client (baseline)
```
Tests run: 111, Failures: 0, Errors: 0, Skipped: 0   —   BUILD SUCCESS
```
All **111** smoke tests pass with a live container (vs 0 meaningful results no-container).
This is WildFly's own behaviour; it confirms the live-container rerun pipeline works
end-to-end and the server/provisioning are healthy.

## Running the test CLIENT under CratonVM — two gaps found
The goal of running the Arquillian **client** under CratonVM (server stays HotSpot) hit two
distinct CratonVM limitations. Both are **CratonVM findings**, tracked here.

### Gap A — CratonVM as a Surefire-forked test JVM — dup-`-Xmx` **FIXED** ([bug-04](bug-04-surefire-jvm-dup-xmx.md)); booter-init exit-1 remaining
Surefire's `-Djvm=` requires the path to end with `…/bin/java[.exe]`. The build's
`java`-named bin alias (`-F java-bin-alias`) satisfies the *name* check, but it is the **same
clap-based CLI** as `cratonvm.exe` — it does **not** parse real `java` CLI syntax:
```
java.exe -version  ->  error: unexpected argument '-v' found
```
Surefire forks the test JVM with `java -jar/-cp -Dprop -X… org.apache.maven.surefire.booter.ForkedBooter …`;
CratonVM rejects those args and exits, so Surefire reports
`The forked VM terminated without properly saying goodbye. VM crash or System.exit called?`.
**Update:** three layers (see [bug-04](bug-04-surefire-jvm-dup-xmx.md)):
1. **dup-`-Xmx`** — Surefire passes `-Xmx512m` twice; clap aborted (exit 2). **FIXED**
   (`vm-cli/src/main.rs`, `overrides_with`). The fork now parses + starts.
2. CratonVM then **runs the Surefire ForkedBooter → JUnit → Arquillian** fine (the thin
   booter jar's manifest `Class-Path` resolves).
3. **Root blocker (open):** CratonVM presents as Windows but its native `java.nio.file.Path`
   uses Rust **UNIX** separator semantics (`getNameCount("a\b\c")`=1 not 3;
   `toAbsolutePath().normalize()` mangles a mixed-separator path to `/?/C:/…`), so WildFly's
   `validateWildFlyDir` rejects a valid `jboss.dist`. Needs a Windows-path-semantics NIO fix
   (core, shared by the whole suite — deferred, not rushed). Gap C also blocks this path.

### Gap B — CratonVM client: `NullPointerException: hasMoreElements on null` — **FIXED** ([bug-02](bug-02-zipfile-entries-null.md))
Bypassing Surefire with the proven per-class `KRun` harness (`cratonvm.exe -cp … KRun`,
which works) + the **remote** adapter (`wildfly-arquillian-container-remote`) pointed at a
manually-booted server (port-offset 100): the remote container config **is** picked up (the
`ConfigurationException` is gone), but the test then fails under CratonVM with
`java.lang.NullPointerException: Cannot invoke hasMoreElements on null` during Arquillian
client processing — a **null `Enumeration`** where the JDK contract requires an empty one
(e.g. `ClassLoader.getResources(...)` returning `null`). So the in-container deployment never
completes under the CratonVM client. **Fix direction:** audit the resource/enumeration
natives (`getResources`/`findResources`/`getSystemResources`) to return an empty
`Enumeration`, never `null`.

### Gap C — CratonVM client too slow to build the deployment archive ([bug-03](bug-03-regex-perf-deployment-build.md))
With Gap B fixed, the cratonvm client gets **past** ShrinkWrap package scanning but never
finishes. A `--stack-dump-on-timeout` dump shows it **executing** (not blocked) deep in
ShrinkWrap deployment-archive building — `AssetUtil.getFullPathForClassResource` →
`java.util.regex.Matcher.replaceAll`, called per class while packaging the large JUnit-5
container archive. Measured: CratonVM's regex is **~50× slower** (`String.replaceAll`) to
**~600× slower** (precompiled `Matcher`) than HotSpot, so the archive build takes
minutes-to-never. **Performance issue, not a deadlock** — see bug-03 for the fix direction
(JIT coverage of the `Pattern.match` hot loop).

## Status
- Live-container rerun **works** (smoke: 111/111 on HotSpot).
- Running the WildFly Arquillian **client under CratonVM**: Gap A (Surefire java-CLI, open)
  and **Gap B (ZipFile.entries null — FIXED, [bug-02](bug-02-zipfile-entries-null.md))**.
  With Gap B fixed the client reaches deployment but then hits **Gap C** (deploy-phase hang,
  open). Gap C is the remaining prerequisite to collect CratonVM client-side pass/fail
  against a live server.
