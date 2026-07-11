# WildFly standalone managed server fails to boot when launched from within a CratonVM Surefire fork — 96% of `testsuite/integration/basic`+`domain` failures in round 6

Status: OPEN — new, found 2026-07-11 during round-6 rerun (post `\p{java*}` regex fix, post
`build/target/wildfly-32.0.1.Final` distribution restore, post per-module flock fix)
Severity: **Critical** — this is now the single dominant blocker for the entire WildFly suite under
CratonVM, exceeding even the scale of the earlier `\p{java*}` regex bug.
First confirmed: 2026-07-11, Azure worktree `test/wildfly-full-suite-20260707`, dev@adfdd0a1 (round-6
binary, `frozen-cratonvm-wildfly-bugbash-v5-20260711`)

## Symptom

Almost every Arquillian `@RunWith(Arquillian.class)` test class in `testsuite/integration/basic` (and a
smaller, differently-shaped set in `testsuite/domain`) fails before any test method executes:

```text
org.jboss.arquillian.container.spi.client.container.LifecycleException: The java process starting the
managed server exited unexpectedly with code [1]
	at org.jboss.as.arquillian.container.CommonManagedDeployableContainer.startInternal(...)
	at org.jboss.as.arquillian.container.CommonDeployableContainer.start(...)
	at org.jboss.arquillian.container.impl.ContainerImpl.start(...)
	...
```

`target/wildfly/standalone/log/server.log` is never created (confirmed by deleting it before each
attempt and checking afterward) — the managed server process dies before `jboss-modules` writes its very
first log line. Elapsed time to failure is 5-25s (variable, not a fixed pattern), never a full timeout.

## Confirmed CratonVM-specific via clean HotSpot A/B

`org.jboss.as.test.integration.beanvalidation.BeanValidationTestCase`, identical `target/wildfly`
directory, identical harness invocation, only the JVM differs:

- **CratonVM** (jit-real mode): `LifecycleException`, exit code 1, no server.log, `Tests run: 1, Errors: 1`.
- **Real HotSpot** (same class, same command via `run-suite-linux.sh hotspot`): boots and passes cleanly —
  `OK`, 3/3 test methods passed, 17s wall time.

## Scale (round 6, this session)

Of 628 classes completed before the run was stopped for investigation:

- 605 `FAIL`, 18 `EMPTY`, 5 `TIMEOUT`, **0 `OK`**
- 583 of the 605 `FAIL` are in `testsuite/integration/basic`; 22 are in `testsuite/domain`
- Of the 583 `basic`-module failures: 301 are `LifecycleException: ... exited unexpectedly with code [1]`,
  281 are `LifecycleException: Could not start container` (a related/overlapping signature — same
  underlying managed-server-boot failure, different Arquillian wrapper message depending on timing), 1 is
  unrelated (`Cannot generate arquillian service`)
- Exit codes seen: 297× code `1`, 4× code `139` (SIGSEGV — worth separate follow-up, not covered here)

This is **96% of all non-EMPTY/non-TIMEOUT failures** in the round — the dominant signal by a wide margin.

## Investigation — narrowed but not fully root-caused

The actual managed-server launch command was captured live via `ps aux -ww` during a real class run:

```text
.../cratonvm-javahome/bin/java -D[Standalone] --add-exports=... --add-opens=... \
  -Dorg.jboss.ejb.client.wildfly-testsuite-hack=true -Xmx512m -XX:MetaspaceSize=128m \
  -Djboss.dist=.../build/target/wildfly-32.0.1.Final -Dmaven.repo.local=... \
  ... -ea -Djboss.home.dir=.../testsuite/integration/basic/target/wildfly \
  -Djboss.server.base.dir=.../target/wildfly/standalone \
  ... -jar .../target/wildfly/jboss-modules.jar \
  -mp /data/data/cratonvm/apps/wildfly/build/target/wildfly-32.0.1.Final/modules:/data/data/cratonvm/apps/wildfly/testsuite/integration/basic/target/modules \
  org.jboss.as.standalone -Dts.wildfly.version=32.0.1.Final -c=standalone.xml
```

Note the `-mp` module path is colon-separated: the shared distribution's `modules/` (which does contain
`org.jboss.as.standalone` and `org.jboss.as.cli` — verified directly) plus a per-module
`target/modules` path that does **not** exist on disk (confirmed absent). `target/wildfly/modules` itself
also does not exist (`maven-resources-plugin`'s `ts.copy-wildfly` execution deliberately excludes
`modules/` from its per-module copy — see [[wildfly-regex-java-predefined-classes-unsupported]]'s sibling
investigation of the same build for context on this shared-checkout layout).

**Attempts to reproduce this exact failure in isolation all succeeded (did not crash):**

1. Running the captured command directly from an interactive shell under CratonVM — boots through
   `WFLYSRV0049: WildFly Full 32.0.1.Final ... starting` and many MSC service-thread startups, no crash,
   runs until manually killed.
2. Same, but with the `-Xmx512m -XX:MetaspaceSize=128m` flags added back (in case CratonVM's per-object
   overhead makes this tight heap OOM silently) — got substantially further into boot (past `infinispan`
   subsystem parsing) before the test's own timeout killed it; still no crash.
3. A minimal `ProcessBuilder`-based Java repro, itself launched under CratonVM (to replicate "CratonVM
   parent spawns CratonVM child via `ProcessBuilder`", matching how Arquillian's
   `org.wildfly.core.launcher` + `CommonManagedDeployableContainer.startInternal()` actually launches the
   managed server — via Java's process API from inside the running JVM, not from a shell) — also booted
   successfully, no crash.

None of these isolated repros reproduce the failure, despite closely matching the captured real command.
This means the trigger is specific to running **inside the actual Surefire-forked JVM**, with its full
Arquillian/WildFly-testsuite classpath (hundreds of JARs) and whatever additional environment/JVM state
that fork carries — not simply "CratonVM can't launch this managed-server command." Plausible unexplored
directions for whoever picks this up:

- A CratonVM-specific `getResources(META-INF/MANIFEST.MF)` classpath-scale workaround is visible in the
  boot log even in the successful isolated runs: `capping 519 flat-classpath matches to 128 (CratonVM
  module-jar flood; see classloader.rs WF32-fix)`. Worth checking whether the **parent** (Surefire-forked)
  JVM's much larger classpath interacts with this cap or a related mechanism in a way that corrupts state
  inherited by the child process (environment, working directory, or `java.class.path` propagation via
  `ProcessBuilder`).
- Diff the actual environment variables / JVM system properties visible to the child process under the
  real harness vs. the isolated repro (e.g. via a wrapper script that dumps `System.getenv()` /
  `System.getProperties()` before doing anything else, since the crash happens before any WildFly logging
  — a trivial diagnostic `java` binary that just prints its environment and args, substituted in place of
  the real managed-server jar, would surface exactly what's inherited differently).
- Confirm whether `ProcessBuilder.redirectErrorStream`/stdout-draining behavior differs between
  CratonVM-launched-from-shell vs. CratonVM-launched-from-CratonVM-fork in a way that could cause the
  child to block on a full pipe early enough to look like an instant exit (though exit code 1, not a hang,
  argues against a pure pipe-deadlock explanation).
- 4 of the 605 failures show exit code 139 (SIGSEGV) rather than 1 — worth checking whether these are the
  same root cause hit at a slightly different point, or a distinct crash.

## Repro

```bash
cd apps/wildfly-suite-runner   # own copy pointed at WILDFLY=<built wildfly checkout>
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<any post-2026-07-11 cratonvm release binary>
export JDK25_WIN=<real JDK 25 home>
export MAVEN_ARGS='-Dcontainer.java.home=<cratonvm-javahome wrapper dir>'
./run-suite-linux.sh run --category others --jit on --jdk real --class-to 90 \
  --only 'integration.beanvalidation.BeanValidationTestCase' --tag repro
# -> FAIL, LifecycleException: exited unexpectedly with code [1], no server.log written

./run-suite-linux.sh hotspot --category others --class-to 90 \
  --only 'integration.beanvalidation.BeanValidationTestCase' --tag repro-hotspot
# -> OK, 3/3 tests passed, ~17s
```

## Evidence

```text
/data/data/wt-wildfly-bugbash-20260707-runner/out/round6b-s1of6-jit-real-others-20260711-023656/
  (and s2..s6) logs/*.log — 628 classes total, 605 FAIL, 0 OK, this signature in 583/605
/data/data/wt-wildfly-bugbash-20260707-runner/out/hstest-modules-hotspot-others-20260711-094721/
  (HotSpot A/B baseline for BeanValidationTestCase — OK)
/tmp/manual_repro.log, /tmp/manual_repro2.log, /tmp/ProcLaunchTest.java on the Azure host — isolated
  repro attempts, all succeeded (did not reproduce the crash)
```
