# Process.toHandle() missing on CratonVM's synthetic Process implementation

Status: OPEN — new, found during 2026-07-07 full WildFly suite bug-bash run
Severity: Low-Medium (breaks any code calling `Process.toHandle()`; narrow, single missing native registration)
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`

## Symptom

Any call to `java.lang.Process.toHandle()` on a CratonVM-backed `Process` (returned by
`ProcessBuilder.start()`) throws:

```text
java.lang.NoSuchMethodError: cratonvm/synthetic/Process.toHandle()Ljava/lang/ProcessHandle;
```

This is unambiguous evidence of a CratonVM implementation gap — the receiver class in the error
(`cratonvm/synthetic/Process`) is CratonVM's own internal synthetic wrapper class, not a real JDK class,
so this can't be a WildFly/harness issue; real HotSpot has no such class and a HotSpot A/B is unnecessary
to establish this is CratonVM-specific.

Concretely this breaks `org.wildfly.test.scripts.ScriptProcess.start()` (WildFly's test helper for
launching CLI scripts like `jconsole.sh`, used by `org.wildfly.test.scripts.JconsoleScriptTestCase` and
similar), which calls `.toHandle()` on the launched process. Affected 5 classes in the `scripts` module in
this run, all failing identically.

## Root cause (found via source read, not yet fixed)

`native-builtins/src/phases_late.rs` (around line 13335) registers the native method surface for
`cratonvm/synthetic/Process` — the runtime class CratonVM's `Process`-returning natives (e.g.
`ProcessBuilder.start()`) actually produce. It registers `waitFor()`, `waitFor(long, TimeUnit)`,
`exitValue()`, `isAlive()`, `destroy()`, `destroyForcibly()`, `pid()`, `getInputStream()`,
`getErrorStream()` — but **not `toHandle()`** (added to `java.lang.Process` in JDK 9, returns a
`ProcessHandle` for the process). Since virtual dispatch on a `cratonvm/synthetic/Process` receiver
probes the native registry keyed on that synthetic class name (per the comment already in the file just
above this registration block) rather than falling back to a real `java.lang.Process` implementation,
an unregistered method throws `NoSuchMethodError` instead of falling through to any default behavior.

## Suggested fix

Register a native override for `toHandle()` alongside the other `synthetic_proc` methods in
`native-builtins/src/phases_late.rs`, returning a `ProcessHandle` backed by the same PID/exit-status
bookkeeping the existing `pid()`/`exitValue()`/`isAlive()` natives already use (note: the existing `pid()`
native currently returns `std::process::id()` — the *current* process's PID, not necessarily the actual
launched subprocess's PID; worth double-checking that's correct/intentional while in the area, since a
`ProcessHandle` built from the wrong PID would be a second, subtler bug).

## Repro

```bash
cd apps/wildfly-suite-runner   # own copy pointed at WILDFLY=<built wildfly checkout>
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<any cratonvm release binary>
export JDK25_WIN=<real JDK 25 home>
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --only 'JconsoleScriptTestCase' --tag repro
# -> FAIL, Errors: 2, NoSuchMethodError: cratonvm/synthetic/Process.toHandle()
```

Minimal non-WildFly repro should also work directly against `cratonvm`:

```java
Process p = new ProcessBuilder("echo", "hi").start();
p.waitFor();
p.toHandle();  // -> NoSuchMethodError: cratonvm/synthetic/Process.toHandle()
```

## Evidence

```text
5 affected classes, module testsuite/scripts (all identical signature):
org.wildfly.test.scripts.JconsoleScriptTestCase (and 4 sibling script test classes)
surefire-reports/.../org.wildfly.test.scripts.JconsoleScriptTestCase.txt
```

## Related / not to be confused with

Unrelated to every other finding from this run — no server/deployment/container involved at all, purely
a missing native method registration hit by test helper code that shells out to a script and wants a
`ProcessHandle` for it.
