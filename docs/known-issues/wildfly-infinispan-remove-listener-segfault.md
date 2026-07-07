# SIGSEGV in infinispan_local::CacheInner::remove_listener during WildFly server shutdown

Status: OPEN — new, found during 2026-07-07 full WildFly suite bug-bash follow-up (harness debugging session)
Severity: **High** — real native SIGSEGV crash, not a misclassification. Likely masked hundreds of the
"no managed container" classifications from the original full-suite run (see below).
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`, dev@37efdc4a

## Symptom

When Maven Surefire forks CratonVM to run a `testsuite/integration/basic` class that actually reaches
the point of starting a real Arquillian-managed WildFly standalone server, the forked CratonVM process
can segfault outright (`Process Exit Code: 139`), rather than failing/passing cleanly:

```text
[ERROR] Error occurred in starting fork, check output in log
[ERROR] Process Exit Code: 139
```

Kernel confirms the crash and its exact location:

```text
kernel: main-vm[<pid>]: segfault at 1d4c0 ip 00005bd2212c2df7 sp 00007a224cda9e50 error 4 in java.exe[...]
```

`error 4` = user-mode read of a non-present page. `ip - image base` symbolizes cleanly (binary was built
`not stripped`) to:

```text
cratonvm_native_builtins::infinispan_local::CacheInner::remove_listener
```

## Root cause hypothesis (source-level, not yet fixed)

`native-builtins/src/infinispan_local.rs`:

```rust
pub fn remove_listener(&self, listener: ObjectRef) -> bool {
    let mut listeners = self.listeners.lock();
    let ptr = listener.as_ptr() as usize;             // <-- crash site
    let before = listeners.len();
    listeners.retain(|h| h.listener_ptr != ptr);
    listeners.len() != before
}
```

called from `native_cache_remove_listener` (`Cache.removeListener(Object)V`), which extracts `listener`
as a bare `ObjectRef` from the native call's `Value::Object` argument via `obj_arg(args, 1)`.

This has the shape of the codebase's known **native stale-local / unrooted-pointer** bug family (see
`docs/known-issues`/`docs/internal` entries on native-stale-local roots and persistent-singleton roots
fixed elsewhere in this codebase): if the `ObjectRef` handed to a native method is not pinned/rooted
across a point where a moving young-gen GC can run, and a GC happens to relocate the underlying listener
object between the JVM handing CratonVM the reference and `remove_listener` calling `.as_ptr()` on it,
the pointer read is stale and can point into freed or relocated memory — producing exactly this kind of
small, near-null-ish faulting address (`0x1d4c0`) rather than a wild/huge one.

`remove_listener` is very plausibly called from WildFly's own shutdown path (deregistering local-cache
listeners — used internally by WildFly for session/EJB-timer/deployment-scanner local caching, not just
clustering) — server shutdown is a natural point for a GC to be running concurrently with native cache
teardown, which matches the observed trigger (this crash surfaced specifically once a real managed
container actually got far enough to attempt a full boot+shutdown cycle, not on every invocation).

## Why this matters beyond this one crash

This crash is a **plausible deeper explanation for a large fraction of the "integration/basic 494-class
no-managed-container cluster"** documented in the original full-suite run
(see the retired/rewritten root-cause note in this session). That analysis found the managed WildFly
server appears to never start; this crash shows that when the harness's stale-port-squatting problem
(a **separate, self-inflicted** issue — see below) is cleared and CratonVM actually attempts a real
server boot, it can segfault outright partway through — which would independently prevent
`InstanceProducer`/`ArchiveDeployer` from ever getting registered, producing the exact same downstream
NPE signature the original analysis attributed purely to a harness/config gap. **The container.java.home
Maven-property gap and this crash are likely compounding, not alternative, explanations** — fixing one
without the other will still show failures.

## Confirmed NOT a stale-server-process artifact

Verified by explicitly killing all leftover WildFly server processes bound to the module's ports (8080/9990)
before each attempt, then re-running `EarClassLoadingTestCase` three times in a row with the fresh
dev-rebuilt binary:
- Attempt 1 (clean port, `container.java.home` unset): `FAIL`, `Errors: 1`, ~23s.
- Attempt 2 (clean port, `container.java.home` unset): **`CRASH`, `Process Exit Code: 139`**, ~23s — the segfault above.
- Attempt 3 (port now squatted again by the orphan the crash itself left behind — see next section): `FAIL`, `Errors: 1`, ~55s (slower, corrupted by the orphan).

So the crash is intermittent/timing-sensitive (consistent with a GC-race hypothesis), not deterministic
on every invocation — but it is real and reproducible within a handful of attempts.

## Compounding harness discovery: crashed/timed-out runs orphan the spawned WildFly server process

Separately from the crash itself: when the forked CratonVM test-runner JVM dies (segfault, or gets
`timeout`-killed for exceeding `--class-to`), the **real WildFly server process it spawned as a child
survives** (it's not in the same process group / doesn't get cleaned up), and stays bound to the
module's default ports (8080/8180/8443/8543/9990/10090) indefinitely. Confirmed directly: found two such
orphans still running and listening on 8080/9990 from test attempts roughly 1-1.5 hours earlier
(`ss -tlnp` showed live listeners; `jboss.home.dir` in their command lines matched this exact module's
`target/wildfly` directory). Interesting detail: these orphaned *servers themselves* run under
**real JDK 17** (`/usr/lib/jvm/java-17-openjdk-amd64/bin/java`), not CratonVM — consistent with
`container.java.home` (referenced by `arquillian.xml` but never defined in the pom hierarchy, see the
separate container.java.home finding) resolving to nothing and Arquillian's `ManagedDeployableContainer`
falling back to the system default `java`. **This means, as currently configured, this module family
never actually tests "does WildFly run correctly hosted by CratonVM" at all** — only "does CratonVM
work as the JUnit/Arquillian client JVM talking to a real-JDK-hosted WildFly instance." Fixing
`container.java.home` to point at a CratonVM-backed JAVA_HOME is necessary to actually exercise this
crash (and the app-under-test) via CratonVM instead of real JDK.

Once one class's run leaves an orphaned server squatting on the shared ports, every subsequent class in
that module for the rest of a run gets corrupted results (LifecycleException "port already in use", or
whatever Arquillian's `allowConnectingToRunningServer=true` fallback does when it "adopts" an unrelated
stale server) until that orphan is manually killed. This is a **harness bug** (a `timeout`/crash should
kill the whole process group, not just the top-level `mvn`/surefire-fork process) independent of the
CratonVM crash itself, but the two compound: the crash creates the orphan, and the orphan then corrupts
every class that runs afterward until someone notices and kills it.

## Suggested fixes

1. **CratonVM (this doc's primary subject):** root-cause the stale pointer in
   `infinispan_local::CacheInner::remove_listener` — likely needs `listener` pinned/rooted for the
   duration of the native call (matching whatever pattern the codebase's other native-stale-local fixes
   already use), so a concurrent GC can't invalidate `listener.as_ptr()` between argument extraction and
   use. A debug build + gdb repro (this doc's evidence was gathered via release-binary `addr2line` symbolization
   of a `dmesg`/`journalctl -k` segfault line, not a live debugger session) would nail the exact line.
2. **Harness (apps/wildfly-suite-runner, separate task):** kill the whole process group on a per-class
   timeout/crash, not just the top-level process, so a crashed/timed-out class can't leave a server
   orphaned to corrupt later classes. Also fix `container.java.home` (separate finding) so the spawned
   server actually runs under CratonVM, which is presumably the point of running this suite at all.

## Repro

```bash
# On the Azure host: clear any stray servers on the module's ports first.
ss -tlnp | grep -E ':8080|:9990'   # kill -TERM any PIDs found bound to these before testing

cd apps/wildfly-suite-runner
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<cratonvm release binary>
export JDK25_WIN=<real JDK 25 home>
# NOTE: do NOT set MAVEN_ARGS=-Dcontainer.java.home=... for this specific repro --
# the crash was found via the *unset* (falls back to real JDK 17 for the server) path.
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 80 \
  --only 'EarClassLoadingTestCase' --tag repro
# Non-deterministic: may need 2-3 attempts (with the port cleared between each) to hit
# Process Exit Code: 139 rather than the ordinary FAIL.
journalctl -k --since '5 min ago' | grep segfault   # confirms + gives the crash address
```

## Evidence

```text
kernel segfault line: main-vm[<pid>]: segfault at 1d4c0 ip 00005bd2212c2df7 sp 00007a224cda9e50 error 4 in java.exe[e5bdf7,5bd2208e5000+11b3000]
addr2line -e <binary> -f -C 0x9dddf7  ->  cratonvm_native_builtins::infinispan_local::CacheInner::remove_listener
/data/data/wt-wildfly-bugbash-20260707-runner/out/cleanslate2-jit-real-all-20260707-071253/crashes.log
```

## Related

Same underlying "unrooted native pointer across a GC point" mechanism as the codebase's existing
native-stale-local-family fixes elsewhere (see memory/internal docs on that family) — this looks like an
occurrence that wasn't caught by those earlier sweeps, in Infinispan's local-cache listener path
specifically. Separate from, but compounds with, the `container.java.home` harness gap and the
process-group-orphaning harness bug documented above.
