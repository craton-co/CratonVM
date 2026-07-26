# Process.toHandle() missing on CratonVM's synthetic Process implementation

Status: FIXED — 2026-07-07, branch `fix/process-tohandle-missing-native-20260707`
Severity: Low-Medium (broke any code calling `Process.toHandle()`; narrow, single missing native registration)
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`

## Symptom

Any call to `java.lang.Process.toHandle()` on a CratonVM-backed `Process` (returned by
`ProcessBuilder.start()`) threw:

```text
java.lang.NoSuchMethodError: cratonvm/synthetic/Process.toHandle()Ljava/lang/ProcessHandle;
```

This is unambiguous evidence of a CratonVM implementation gap — the receiver class in the error
(`cratonvm/synthetic/Process`) is CratonVM's own internal synthetic wrapper class, not a real JDK class,
so this can't be a WildFly/harness issue; real HotSpot has no such class and a HotSpot A/B is unnecessary
to establish this is CratonVM-specific.

Concretely this broke `org.wildfly.test.scripts.ScriptProcess.start()` (WildFly's test helper for
launching CLI scripts like `jconsole.sh`, used by `org.wildfly.test.scripts.JconsoleScriptTestCase` and
similar), which calls `.toHandle()` on the launched process. Affected 5 classes in the `scripts` module.

## Root cause — corrected from the original hypothesis

The original write-up pointed at `../../../../native-builtins/src/phases_late.rs`'s `register_phase57_process`
(~line 13335), which does register a `cratonvm/synthetic/Process` method surface missing `toHandle()`.
That diagnosis was *plausible but not where the bug actually lived*: `../../../../native-io/src/process.rs`'s
`register_process_natives` registers a **second**, later-registered set of natives for the exact same
two class names (`java/lang/Process` and `cratonvm/synthetic/Process`) with real fd/PID-aware
implementations, and its own comment says outright that it "override[s] the stubs from
`phases_late::register_phase57_process`". Confirmed empirically: patching only `phases_late.rs` and
rebuilding did **not** fix the repro (`pid()` already returned the right value — proof that path was
already live via native-io — but `toHandle()` still threw `NoSuchMethodError`, because native-io's loop
over `["java/lang/Process", SYNTHETIC_PROCESS_CLASS]` never registered `toHandle` either). The real fix
had to go into `../../../../native-io/src/process.rs`.

A second-order wrinkle surfaced while wiring the fix: naively building the returned handle as a bare
1-field synthetic `java/lang/ProcessHandle` (mirroring `ProcessHandle.current()`'s existing pattern in
`phases_late.rs`) produced `AbstractMethodError: method java/lang/ProcessHandle.pid()J has no Code
attribute` instead of `NoSuchMethodError` — because `ensure_class_initialized("java/lang/ProcessHandle")`
actually *succeeds* in this VM (a loadable interface classfile exists even without `--java-home`), so the
object is allocated under the real interface's class id and virtual dispatch runs its (bodyless, abstract)
`pid()` directly instead of falling back to the native registry. The fix constructs a real, concrete
`java.lang.ProcessHandleImpl(pid, startTime)` instead (via `ctx.new_object_initialized`, same idiom already
used elsewhere in this file for `FileDescriptor`/`FileInputStream`), which has actual method bodies.

## Fix

`../../../../native-io/src/process.rs`:
- Registered `toHandle()` → `()Ljava/lang/ProcessHandle;` on both `java/lang/Process` and
  `cratonvm/synthetic/Process` in the same `for proc_cls in [...]` loop as the other Process methods,
  building a real `ProcessHandleImpl(pid, 0)` (falling back to the old 1-field synthetic
  `java/lang/ProcessHandle` layout only if `ProcessHandleImpl` construction itself fails, i.e. no real
  `java.lang.*` classes loadable at all).
- The `pid` passed in is `PROC_FIELD_PID` (index 4 in the synthetic `Process` layout) — the real OS pid
  captured by `spawn_and_wrap` at spawn time — so `toHandle().pid() == process.pid()`, matching the JDK's
  documented contract.

`../../../../native-builtins/src/phases_late.rs` (belt-and-suspenders on the now-shadowed stub, kept for whichever
path is live if registration order or feature flags ever change):
- Added the same `toHandle()` registration to the `synthetic_proc`/`proc` blocks.
- Fixed `pid()` on both blocks to read a newly added `PROC_FIELD_PID` field instead of returning
  `std::process::id()` (the **VM's own** pid, not the launched child's — confirmed as a real, if inert,
  second bug: this stub's `ProcessBuilder.start()` used `Command::output()`, which never exposes the
  child's pid at all). Switched that spawn path to `Command::spawn()` + `Child::id()` +
  `Child::wait_with_output()` so a real child pid exists to capture.

## Known residual gap (not fixed, out of scope for this native-registration bug)

`ProcessHandleImpl.isAlive()` / `.destroy()` / `.waitFor()` on the handle returned by `toHandle()` route
through already-registered natives (`isAlive0`, `destroy0`, `waitForProcessExit0`) that key off the VM's
*internal* subprocess-table handle (an opaque incrementing counter, `PROC_FIELD_HANDLE`), not the real OS
pid stored in the constructed `ProcessHandleImpl`. Since we store the real pid (for `pid()` spec
compliance), those three methods degrade to the same "not found in table → assume still alive" fallback
`ProcessHandle.current()` already exercises, rather than accurately tracking the specific child. Making
them fully accurate would require the process/exit-cache tables to also be queryable by real pid (a
broader rework), so it's tracked here rather than folded into this fix.

## Repro (now passes)

```bash
cd /data/data/wt-wildfly-bugbash-20260707-runner
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<cratonvm release binary with this fix>
export JDK25_WIN=/data/data/jdk25-real
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --only 'org.wildfly.test.scripts' --tag verify
# -> classes: EMPTY=1 (abstract ScriptTestCase base, expected) OK=5; test-methods errors=0
```

Minimal non-WildFly repro (also passes, both `--synthetic-jdk`-style default mode and `--java-home
<real JDK25>` mode):

```java
Process p = new ProcessBuilder("echo", "hi").start();
p.waitFor();
System.out.println(p.pid());                       // real child pid
ProcessHandle h = p.toHandle();
System.out.println(h.pid());                        // same value as p.pid()
System.out.println(h.isAlive());
```

## Evidence

```text
5 affected classes, module testsuite/scripts (all identical signature):
org.wildfly.test.scripts.JconsoleScriptTestCase (and 4 sibling script test classes)
surefire-reports/.../org.wildfly.test.scripts.JconsoleScriptTestCase.txt

Post-fix verification run (2026-07-07):
org.wildfly.test.scripts.AppClientScriptTestCase   OK  found=5 passed=2 skipped=3 errors=0
org.wildfly.test.scripts.JconsoleScriptTestCase    OK  found=5 passed=2 skipped=3 errors=0
org.wildfly.test.scripts.JdrScriptTestCase         OK  found=5 passed=2 skipped=3 errors=0
org.wildfly.test.scripts.ScriptTestCase            EMPTY (abstract base, no @Test methods — expected)
org.wildfly.test.scripts.WsConsumeScriptTestCase   OK  found=5 passed=2 skipped=3 errors=0
org.wildfly.test.scripts.WsProduceScriptTestCase   OK  found=5 passed=2 skipped=3 errors=0
```

## Related / not to be confused with

Unrelated to every other finding from this run — no server/deployment/container involved at all, purely
a missing native method registration hit by test helper code that shells out to a script and wants a
`ProcessHandle` for it.
