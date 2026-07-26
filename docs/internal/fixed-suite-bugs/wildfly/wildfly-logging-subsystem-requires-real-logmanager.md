# WildFly `logging` extension subsystem-add fails: WFLYLOG0078 "requires the log manager to be org.jboss.logmanager.LogManager"

Status: FIXED / CLOSED — fixed 2026-07-08 on branch `codex/wildfly-logmanager-bootstrap-20260708`.
The WildFly 32.0.1.Final direct `standalone.sh` probe under CratonVM no longer prints
`WARNING: Failed to load the specified log manager class org.jboss.logmanager.LogManager`, no longer throws
`WFLYLOG0078`, and gets past the logging subsystem's real-LogManager gate. Later boot failures observed
after this fix (`String.getCanonicalName` and unrelated null-name subsystem failures) are separate,
deeper-in-boot issues.
Severity: Medium — blocks full standalone/managed-container boot for any config that loads the `logging`
extension (essentially all of them), but the server fails fast and cleanly (no crash, no hang) rather than
corrupting state.
First confirmed: 2026-07-07, Azure worktree `wt-wildfly-harness-gcbarrier-0707`, dev @ `bee86ff0`+

## Symptom

Booting real WildFly 32.0.1.Final standalone directly under CratonVM (`bin/standalone.sh`, real-JDK
backend) now completes the boot attempt (no more infinite hang — see the GC-barrier fix) but fails
deterministically during subsystem initialization:

```text
ERROR [org.jboss.as.controller.management-operation] WFLYCTL0013: Operation ("parallel-extension-add") failed
    java.lang.RuntimeException: WFLYCTL0079: Failed initializing module org.jboss.as.logging
    Caused by: java.util.concurrent.ExecutionException: java.lang.IllegalStateException: WFLYLOG0078: The
    logging subsystem requires the log manager to be org.jboss.logmanager.LogManager. The subsystem has not
    be initialized and cannot be used. To use JBoss Log Manager you must add the system property
    "java.util.logging.manager" and set it to "org.jboss.logmanager.LogManager"
FATAL [org.jboss.as.server] WFLYSRV0056: Server boot has failed in an unrecoverable manner; exiting.
```

Earlier in the same boot, the JDK's own bootstrap already warns:

```text
WARNING: Failed to load the specified log manager class org.jboss.logmanager.LogManager
```

Deterministic: reproduced identically on 2/2 consecutive attempts (same message, same point in boot).

## Root cause and fix

This is a known, standing condition in this codebase — `../../../../native-builtins/src/logmanager.rs`'s
`org/jboss/logmanager/Logger.getAttachment`/`attach`/`attachIfAbsent`/`detach` native overrides already
have comments describing it: real `jboss-logmanager` bytecode dereferences `this.loggerNode`, which is
null exactly when `java.util.logging.LogManager.<clinit>`'s `-Djava.util.logging.manager` class-swap
fails and the JDK silently falls back to a plain `java.util.logging.LogManager`. That fallback path is
already tolerated by dedicated native overrides (see `wildfly-jboss-logmanager-geteffectivelevel-null-loggernode.md`,
a related, already-fixed symptom of the SAME underlying fallback).

**This doc is a stricter manifestation of the same root gap.** WildFly's own `logging` extension
subsystem-add operation does not just tolerate a fallback `Logger` the way the general logging natives do —
it explicitly checks (`LoggingExtension`/`LoggingResourceDefinition`, real bytecode) whether the ACTIVE log
manager instance really is `org.jboss.logmanager.LogManager`, and throws `WFLYLOG0078` if not. Since the
CratonVM-hosted JVM never actually installs the real `org.jboss.logmanager.LogManager` (the JDK's own
`-Djava.util.logging.manager` bootstrap class-swap fails, per the WARNING above), this explicit check fails
every time the `logging` extension is added — which happens on every standard `standalone.xml`/
`standalone-full.xml` boot.

The underlying gap was CratonVM not correctly making the active LogManager singleton look like
`org.jboss.logmanager.LogManager` when `java.util.logging.manager=org.jboss.logmanager.LogManager`.
WildFly ships `jboss-logmanager` as a JBoss Modules module
(`modules/system/layers/base/org/jboss/logmanager/main/`), not on the plain system classpath.

The fix makes the `java.util.logging.manager` property path allocate CratonVM's synthetic singleton with
the concrete class `org/jboss/logmanager/LogManager` for the JBoss alias instead of falling back to the
plain `java/util/logging/LogManager` singleton. It also initializes/overrides the immediate
`org.jboss.logmanager.LogContext` close-handler surface used by WildFly's logging configuration so the
server gets past the first real logging subsystem boot operations.

## Why this matters

This was previously **masked entirely** by two earlier-in-the-chain blockers:
1. `container.java.home` unset → the managed/Arquillian-spawned server fell back to real JDK 17, which
   handles the module-classpath bootstrap correctly, so this gap was never exercised under CratonVM at all.
2. Even with `container.java.home` fixed, the GC-barrier boot-hang (see the sibling fixed doc) meant the
   server never got far enough in boot to reach the `logging` extension's subsystem-add operation before
   hanging forever.

With both fixed, this is now the **first real functional blocker** for a full WildFly boot under CratonVM.
Fixing it is likely high-leverage: the `logging` extension is added on essentially every standard
configuration, so this blocks full-boot verification for the whole suite, not just one test.

## Repro

```bash
# On the Azure host, JAVA_HOME must point at a directory with a CratonVM binary at bin/java (see the
# container.java.home fix doc for how to build one), CRATONVM_JAVA_HOME at a real JDK for CratonVM's own
# bootstrap:
cd apps/wildfly/build/target/wildfly-32.0.1.Final
export JAVA_HOME=<cratonvm-javahome-dir>
export CRATONVM_JAVA_HOME=/home/victor/jdk25
timeout 120 ./bin/standalone.sh -b=127.0.0.1 -bmanagement=127.0.0.1
# -> WFLYLOG0078 FATAL within ~15-20s, deterministic
```

## Suggested next steps

Not attempted here (separate subsystem from the GC-barrier/harness fixes this session focused on):
1. Find where CratonVM implements the `-Djava.util.logging.manager` system-property class-swap
   (`java.util.logging.LogManager.<clinit>`'s native override, if any) and check whether it resolves
   classes only via the "current" classloader at JVM-bootstrap time (too early for JBoss Modules to have
   set up the module classloader for `org/jboss/logmanager`) versus a classpath/bootclasspath entry the
   way real HotSpot's `-Xbootclasspath/a:` mechanism does.
2. Alternatively, check whether jboss-modules' own bootstrap (`jboss-modules.jar`'s `Main`) does something
   special (e.g., re-triggering the LogManager swap after its own classloader is live) that CratonVM's
   `java.util.logging` bootstrap ordering doesn't replicate.
3. A synthetic, minimal repro (real `jboss-logmanager` jar + a driver that sets the system property before
   touching `java.util.logging.Logger`, without the full WildFly/JBoss-Modules stack) would isolate whether
   this is a JBoss-Modules-classloader-timing issue specifically, or a more general
   `-Djava.util.logging.manager` gap that would also affect non-WildFly apps relying on a custom LogManager.

## Related

- `wildfly-jboss-logmanager-geteffectivelevel-null-loggernode.md` — the
  already-fixed downstream symptom of the SAME fallback-to-plain-LogManager condition.
- `wildfly-infinispan-remove-listener-segfault.md` and the GC-barrier fix
  doc — the two earlier blockers in the same "WildFly boot under CratonVM" chain, both fixed this session.
