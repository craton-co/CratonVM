# WildFly boot: `ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.AttributeDefinition` initializing `org.jboss.as.remoting` during parallel-extension-add

Status: OPEN — new, found 2026-07-13 while root-causing
[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] after the
`ProcessBuilder.environment()` fix (`d22a5e73`) landed.
Severity: High — fatal, unrecoverable boot crash (~8% of observed failures, see
[[wildfly-standalone-boot-stw-jit-takeover-hang]]'s Scale section for the sibling breakdown), racy/hard to
reproduce reliably.
First confirmed: 2026-07-13, Azure worktree `test/wildfly-full-suite-20260707`, dev@e8f36c78 (round-6e
binary, `frozen-cratonvm-wildfly-bugbash-v6-20260713`)

## Symptom

During WildFly standalone boot's `parallel-extension-add` step (every extension — remoting, undertow,
elytron, connector, clustering, etc. — is added concurrently), the `org.jboss.as.remoting` module's own
extension-add handler throws:

```text
ERROR [org.jboss.as.controller.management-operation] WFLYCTL0013: Operation ("parallel-extension-add") failed
java.lang.RuntimeException: WFLYCTL0079: Failed initializing module org.jboss.as.remoting
	at org.jboss.as.controller.AbstractControllerService$1.run(AbstractControllerService.java:362)
	...
Caused by: java.util.concurrent.ExecutionException: java.lang.ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.AttributeDefinition
	...
Caused by: java.lang.ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.AttributeDefinition
```

Because `parallel-extension-add` treats all ~30 extensions as one atomic operation, this single failure
rolls back *every* extension (including `org.jboss.as.logging`), and the server then fails fatally:

```text
FATAL [org.jboss.as.server] WFLYSRV0056: Server boot has failed in an unrecoverable manner; exiting.
[cratonvm] System.exit(1) called
```

This is why `target/wildfly/standalone/log/server.log` is always empty for this failure mode too: the
logging extension itself gets rolled back along with `remoting`, so it never installs its file handler
before the process exits.

## Root cause hypothesis — not yet pinpointed to a specific native/method

Some object CratonVM hands back to WildFly's own subsystem-registration code — where WildFly expects a
concrete `org.jboss.as.controller.AttributeDefinition` (or an array/collection of them, built via
reflection or a generic collection accessor during `org.jboss.as.remoting`'s extension registration) — is
instead a plain `java.lang.Object`. This is a classic "wrong runtime type surfaces through a generic/cast
boundary" bug, and given it fires specifically inside `org.jboss.as.remoting`'s own module initialization
(not a shared/common WildFly-core class), the actual defect is most likely in code paths specific to that
module's `Extension`/`SubsystemRegistration` implementation — worth checking with `javap`/decompiled
WildFly-core `org.jboss.as.remoting` sources for reflective attribute-list construction or generic-array
covariance that CratonVM's reflection/generics layer might not model correctly.

## Non-deterministic — reproduces roughly 1 in 5 attempts

Ran the identical isolated launch command (see Repro) five times on an otherwise-idle host: 1/5 hit this
exact `ClassCastException`; the other 4/5 instead hung on a separate, likely-related concurrency bug
([[wildfly-standalone-boot-stw-jit-takeover-hang]]) without ever reaching this code path. Both bugs fire
during the same highly-concurrent `parallel-extension-add` boot step (~30-40 threads all doing
classloading/static-init/service-registration at once), which is consistent with (but does not confirm) a
shared root cause in how CratonVM handles concurrent classloading or object visibility across threads
during that step — flagging the connection for whoever investigates either issue, without claiming they
are proven to be the same bug.

## Repro

```bash
cd apps/wildfly/testsuite/integration/basic
rm -f target/wildfly/standalone/log/server.log
<cratonvm-javahome>/bin/java \
  -Xmx512m -XX:MetaspaceSize=128m \
  -Djboss.home.dir=target/wildfly -Djboss.server.base.dir=target/wildfly/standalone \
  -Djboss.server.log.dir=target/wildfly/standalone/log -Djboss.server.config.dir=target/wildfly/standalone/configuration \
  -Dorg.jboss.boot.log.file=target/wildfly/standalone/log/server.log \
  -Dlogging.configuration=file:target/wildfly/standalone/configuration/logging.properties \
  -jar target/wildfly/jboss-modules.jar \
  -mp <shared-dist>/modules:testsuite/integration/basic/target/modules \
  org.jboss.as.standalone -Dts.wildfly.version=32.0.1.Final -c=standalone.xml
# -> most runs hang (see wildfly-standalone-boot-stw-jit-takeover-hang.md); roughly 1 in 5
#    instead prints the ClassCastException above and exits 1 within ~10s.
```

May need several attempts to reproduce given the non-determinism observed.

## Evidence

```text
/tmp/live_monitor.log on the Azure host — one successful isolated repro showing the full stack trace
/data/data/wt-wildfly-bugbash-20260707-runner/out/round6e-s*of6-jit-real-others-20260713-024309/
  logs/*.log — 39/463 FAIL classes this round show "exited unexpectedly with code [1]" (consistent with
  this crash path; child stdout containing the actual exception isn't forwarded into these Maven logs,
  so the isolated repro above is the direct evidence for the exception itself)
```

## Related

[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] — the original, less-precise
documentation of this whole failure family.
[[wildfly-standalone-boot-stw-jit-takeover-hang]] — the dominant (~87% vs this bug's ~8%) sibling failure
mode hit during the same boot step, a hang rather than a crash; possibly related, not confirmed.
`docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — tracks a large, still-open
"long-tail" family of stale-`ObjectRef`-across-GC bugs in WildFly-boot-adjacent native reflection/IO code
(36+ distinct sites found and fixed across 3 prior sessions via a systematic static-analysis sweep).
This `ClassCastException`'s exact signature (`Object` → `AttributeDefinition`, inside
`org.jboss.as.remoting`'s own extension-add path) is **not named among that doc's already-fixed sites**
as of this writing, and reproduces on a binary built well after that sweep — so this is either a genuinely
new (38th+) site of the same bug class the sweep missed, or a different defect entirely (possibly a
concurrent-collection/HashMap corruption rather than a stale-`ObjectRef`, given the failure is a bad cast
rather than a null/wrong-class-mirror error). Worth checking against that doc's sweep methodology before
assuming either way.
