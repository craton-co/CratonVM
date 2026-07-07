# ElytronRemoteOutboundConnectionTestCase: native SIGSEGV during elytron subsystem / remoting-client test execution

Status: OPEN — new, found 2026-07-07 while verifying the [[wildfly-keyfactory-translatekey-null-spi]] fix
Severity: Unknown/High (hard native crash, not a catchable Java exception; blocks the whole test class)
First confirmed: 2026-07-07, Azure worktree `wt-keyfactory-translatekey` (branch `fix/keyfactory-translatekey-20260707`)

## Symptom

Running `org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase`
(module `testsuite/integration/manualmode`) under CratonVM (`jit-real` mode) via Maven/Surefire now
crashes the forked JVM outright:

```text
[ERROR] Process Exit Code: 139
[ERROR] Crashed tests:
[ERROR] org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase
[ERROR] org.apache.maven.surefire.booter.SurefireBooterForkException: ExecutionException The forked VM
terminated without properly saying goodbye. VM crash or System.exit called?
```

Exit code 139 = SIGSEGV. The trace-level `.dumpstream` log for the forked VM shows it gets well into
actual test execution before dying — past the `@Before` setup, past `Executing operation` (adding an
`elytron` `properties-realm`), through opening a first `management-client` remoting endpoint and all its
connection-provider registrations (`remote`, `remote+tls`, `remoting`, `remote+http`, `remote+https`,
`http-remoting`, `https-remoting`), through a `getAuthenticationConfiguration` call, closing that
endpoint, then opening a SECOND `management-client` endpoint for what looks like the actual per-test
management operation — and segfaults partway through that second endpoint's connection-provider
registration sequence (`remote+http`, tick 5 of the second endpoint), with no further output beyond
`Segmentation fault (core dumped)`. No Rust panic message, no Java exception — a hard native fault.
`ulimit -c` is 0 on this host and no core file was produced, so no backtrace is available yet.

**Reproducible**: confirmed twice in a row (both times crashed at the same point, same wall-clock
~43-45s).

## Why this is newly visible, not a regression

This test class previously failed EARLIER, in its `@Before` setup, with the
`KeyFactory.translateKey`/`getKeySpec` NPE documented in [[wildfly-keyfactory-translatekey-null-spi]]
(now fixed). Nobody had ever exercised this class's actual test-method bodies (remoting connections,
elytron subsystem operations) under CratonVM before, because the class never got that far. Fixing the
KeyFactory NPE let the class progress into new territory, which immediately hit this segfault. This is
very likely a **pre-existing, unrelated bug** in the remoting/xnio/socket or elytron-subsystem-management
native path, now reachable for the first time — not something introduced by the KeyFactory fix (which
only touches `native-builtins/src/jca/key_factory.rs`, nowhere near remoting/networking code).

## Repro

```bash
# On the Azure host (uses the already-built WildFly checkout + Linux Maven driver):
cd /data/data/wt-wildfly-bugbash-20260707-runner   # or any checkout of apps/wildfly-suite-runner's Linux driver
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<any cratonvm release binary with the keyfactory-translatekey fix, or later>
export JDK25_WIN=/home/victor/jdk25
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --only 'ElytronRemoteOutboundConnectionTestCase' --tag repro
# -> classes: CRASH=1, Process Exit Code: 139 (SIGSEGV), test-methods found=0
```

Note this REQUIRES the `KeyFactory.translateKey`/`getKeySpec` fix to be present in the binary under
test — against an unfixed binary the class fails earlier (in `@Before`) and never reaches this crash.

## Evidence

```text
/data/data/wt-wildfly-bugbash-20260707-runner/out/verify-fix-jit-real-all-20260707-053719/logs/00001-org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase.log
/data/data/wt-wildfly-bugbash-20260707-runner/out/verify-fix-rerun-jit-real-all-20260707-053936/logs/00001-org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase.log
/data/data/cratonvm/apps/wildfly/testsuite/integration/manualmode/target/surefire-reports/2026-07-07T05-37-50_272.dumpstream   (trace log ending in "Segmentation fault (core dumped)")
```

## Suggested next steps

Not yet root-caused. To make progress: run the same repro directly under `gdb --args <cratonvm binary>
--java-home ... -jar surefirebooter....jar ...` (or attach to the forked PID) to get a real backtrace —
Maven/Surefire's own fork doesn't preserve one, and this host's `core_pattern` routes to `apport`
which isn't producing a usable core file (`ulimit -c` is 0). Given the crash point (deep in
`org.jboss.remoting3`/xnio connection-provider setup, second `management-client` endpoint), likely
candidates given prior related findings in this codebase: `class_manager`/vtable RwLock contention
(see [[httpclient-hangs-are-classmanager-rwlock-deadlock-and-methodhandle-dispatch]]), xnio native
gaps (see [[wildfly-suite-0618-heapcap-and-xnio-options-bug]]), or a JIT-compiled-frame issue in the
remoting endpoint/connection-provider registration path — but none of these are confirmed; this needs
its own investigation from scratch.

## Related

Found via [[wildfly-keyfactory-translatekey-null-spi]]'s own fix-verification run — not caused by that
fix, just newly reachable because of it.
