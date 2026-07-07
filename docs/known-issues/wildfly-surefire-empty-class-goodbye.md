# WildFly/Surefire: forked CratonVM does not complete the booter "goodbye" handshake for zero-test classes

Status: OPEN — new, found during 2026-07-07 full WildFly suite bug-bash run
Severity: Medium (misclassifies ~2-6% of a large suite as VM crashes; likely a real, narrow protocol-completeness gap)
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`

## Symptom

When Maven Surefire (2.22.2, JUnit4 provider) forks CratonVM to run a test class that has **zero actually-runnable `@Test` methods** (an abstract base class matched by a `*TestCase.java` discovery glob, or a concrete class whose test methods are all filtered/excluded for that run), the Maven console still reports the correct `Tests run: 0, Failures: 0, Errors: 0, Skipped: 0`, but Surefire's fork handler then reports:

```text
[ERROR] The forked VM terminated without properly saying goodbye. VM crash or System.exit called?
[ERROR] Process Exit Code: 0
```

i.e. the child process exits **cleanly (exit code 0)**, but the Surefire booter's IPC handshake that signals "no more tests, forked VM shutting down normally" never arrives at the parent Maven process before the child exits. Maven then treats the entire class as `BUILD FAILURE` / a forked-VM crash, even though nothing actually crashed and 0/0/0/0 tests were the correct outcome.

## Confirmed CratonVM-specific via HotSpot A/B (same class, same harness, same command)

`org.jboss.as.test.integration.ejb.remote.distinctname.DistinctNameTestCase` (module `testsuite/integration/basic`):

- **Real HotSpot** (`apps/wildfly-suite-runner` `hotspot` mode, same per-class `mvn -Dtest=... test` invocation): `EMPTY` — clean run, `Tests run: 0`, Maven exits normally with `BUILD SUCCESS`. Wall time 19s.
- **CratonVM** (`jit-real` mode, identical Maven invocation, identical `-Dtest=...` filter): `CRASH` — same `Tests run: 0, Failures: 0, Errors: 0, Skipped: 0`, but `BUILD FAILURE` + `forked VM terminated without properly saying goodbye` + `Process Exit Code: 0`. Wall time ~19-20s (not a hang/timeout — it exits, just without the handshake).

Same Maven version, same Surefire version, same class, same command — the only variable is the JVM. This rules out an Arquillian/test-harness explanation; it is CratonVM's own process behaving differently from HotSpot in the zero-test-method case.

## Scale (this run)

In a sample of 50 CRASH/ABEND-classified classes from the 2026-07-07 full-suite run (`apps/wildfly-suite-runner`, jit-real mode, `--class-to 300`), **31/50 (62%)** showed the exact `Tests run: 0, Failures: 0, Errors: 0, Skipped: 0` + "forked VM terminated" signature with exit code 0 or 1 — no SIGSEGV (`Process Exit Code: 139`) was observed in this sample. Affected classes include both obviously-abstract base classes picked up by the `*TestCase.java`/`*Test.java` discovery glob (`AbstractBatchTestCase`, `AbstractMDB2xTestCase`, `AbstractCustomDescriptorTests`, `AbstractSimpleApplicationClientTestCase`, `AbstractTimerManagementTestCase`, `AbstactUnmarshallingFilterTestCase`) and concrete-looking classes that legitimately resolve to 0 runnable methods under both engines (`DistinctNameTestCase`, `EjbNamespaceInvocationTestCase`, `TimerManagementTestCase`, `TxExceptionBaseTestCase`, `JaxrsAtomProviderTestCase`, `OverlayExistingResourceTestCase`, `InjectionSupportTestCase`, `WarEjbNamingContextTestCase`, and more — full list in evidence below).

This means a meaningful fraction of the "CRASH"/"ABEND" classifications produced by any per-class WildFly suite run against CratonVM are **not real VM crashes** — they are this handshake gap. Real crash-hunting in this suite should first filter out `Tests run: 0` classes before trusting a CRASH/ABEND tally.

## Hypothesis (not yet root-caused)

The Surefire booter protocol (pure Java, running inside the forked JVM via `surefirebooterNNNN.jar`) sends a final status message over its IPC channel (stdout-multiplexed protocol in this Surefire version) before the JVM's `main()` returns / the process exits. For classes with 0 matched tests, this appears to be an unusually fast, early-return code path (finishes in a few hundred ms of "test execution" time relative to ~15-30s for classes with real deployments). A plausible mechanism: CratonVM's process-exit / stdout-flush / shutdown-hook ordering does not guarantee the last bytes written just before `System.exit`-equivalent process teardown are flushed/delivered to the parent before the child's exit is observed — and this window is only reliably hit when there's very little work between "provider decides 0 tests match" and "process exits," i.e. the empty-class case. Classes with real deployments run long enough (15s+) that whatever race exists here does not manifest.

This is a hypothesis for whoever picks this up to verify against CratonVM's process-exit / stdout pipe handling (`vm-cli/src/main.rs` and wherever `Runtime.exit`/normal-return process teardown flushes stdio), not a confirmed root cause.

## Repro

```bash
# On the Azure host, from a WildFly checkout with target/wildfly already built:
cd apps/wildfly-suite-runner   # own copy pointed at WILDFLY=<built wildfly checkout>
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<any cratonvm release binary>
export JDK25_WIN=<real JDK 25 home>
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --only 'ejb.remote.distinctname.DistinctNameTestCase' --tag repro
# -> CRASH, Process Exit Code: 0, "Tests run: 0" in surefire-reports txt

./run-suite-linux.sh hotspot --category all \
  --only 'ejb.remote.distinctname.DistinctNameTestCase' --tag repro-hotspot
# -> EMPTY, BUILD SUCCESS, same "Tests run: 0"
```

## Evidence

```text
/data/data/wt-wildfly-bugbash-20260707-runner/out/bugbash-s1of4-jit-real-all-20260707-015522/logs/00072-org.jboss.as.test.integration.ejb.remote.distinctname.DistinctNameTestCase.log
/data/data/wt-wildfly-bugbash-20260707-runner/out/hscheck3-hotspot-all-20260707-023430/  (HotSpot A/B baseline, EMPTY)
```

Full list of 31 classes exhibiting the exact "Tests run: 0 + forked VM terminated, exit 0/1" signature in this sample (module `testsuite/integration/basic` unless noted):

```text
batch.common.AbstractBatchTestCase
beanvalidation.hibernate.validator.BootStrapValidationTestCase
ee.lifecycle.servlet.LifecycleInterceptionTestCase
ee.naming.defaultbindings.datasource.DefaultDataSourceServletTestCase
ejb.mdb.ejb2x.AbstractMDB2xTestCase
ejb.remote.distinctname.DistinctNameTestCase
ejb.remote.ejbnamespace.EjbNamespaceInvocationTestCase
ejb.timerservice.mgmt.TimerManagementTestCase
ejb.transaction.exception.TxExceptionBaseTestCase
jaxrs.atom.JaxrsAtomProviderTestCase
deployment.deploymentoverlay.ear.OverlayExistingResourceTestCase
ee.injection.support.InjectionSupportTestCase
ejb.descriptor.AbstractCustomDescriptorTests
ejb.packaging.war.namingcontext.WarEjbNamingContextTestCase
ejb.remote.client.api.tx.EJBClientUserTransactionTestCase
ejb.stateless.systemexception.SystemExceptionTestCase
ee.injection.support.servlet.HttpUpgradeHandlerInjectionSupportTestCase
ejb.remote.suspend.EjbRemoteSuspendTestCase
ejb.transaction.cmt.mandatory.SFSBMandatoryTransactionTestCase
domain.management.cli.DomainDeployWithRuntimeNameTestCase   (testsuite/domain module)
deployment.dependencies.EjbDependencyRestartTestCase
ee.appclient.basic.AbstractSimpleApplicationClientTestCase
ee.injection.resource.jndi.bad.BadResourceTestCase
ejb.remote.httpobfuscatedroute.HttpObfuscatedRouteTestSuite
ejb.remote.requestdeserialization.AbstactUnmarshallingFilterTestCase
ejb.remote.view.LocalViewRemoteInvocationTestCase
ejb.security.AnnSBTest
ejb.security.runas.mdb.RunAsMDBUnitTestCase
ejb.timerservice.mgmt.AbstractTimerManagementTestCase
ejb.transaction.bmt.BeanManagedTransactionsTestCase
ejb.transaction.cmt.fail.TransactionFirstPhaseErrorTestCase
```

## Related / not to be confused with

This is **separate** from [[wildfly-domain-heap-corrupt-value-timeout]] and [[wildfly-domain-managed-servers-timeout]] (domain-mode heap-corruption / managed-servers-timeout bugs) — those affect `testsuite/domain` classes with actual runnable tests that time out waiting for a domain, a different mechanism.

It is also separate from the much larger (~350+ class) `testsuite/integration/*` failure cluster seen in the same 2026-07-07 run, where classes fail with `NullPointerException: ... InstanceProducer.get() ... is null` or `DeploymentException: Cannot deploy: X` — that cluster was confirmed via the same HotSpot-A/B technique to be a **test-harness/environment gap, not a CratonVM bug**: the per-class `mvn -Dtest=X test` driver used by `apps/wildfly-suite-runner` never starts (and no Maven-plugin-managed) WildFly server before running `integration/*` classes that need one (unlike `testsuite/domain`, which spins up its own domain infra inline per test). The identical class (`EarClassLoadingTestCase`) fails with the identical `Errors: 1` under real HotSpot using the exact same harness invocation (19-24s wall time either way), so this is a suite-runner limitation, not evidence of ~350 distinct CratonVM bugs. A real per-class or per-module bug hunt across `integration/*` requires a harness that starts a managed WildFly server once (or once per module/profile) before running that module's classes — out of scope for this run; noted here so it is not mistaken for a wave of new VM bugs.
