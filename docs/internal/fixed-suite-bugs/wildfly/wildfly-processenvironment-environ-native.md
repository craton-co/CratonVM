# WildFly Domain Child VM Missing ProcessEnvironment.environ Native

Status: ✅ FIXED — verified 2026-07-06, no longer reproduces on `dev`.

## Original report (2026-07-05)

The Azure WildFly JIT-on non-passed rerun reached Surefire and started the
WildFly domain process through the CratonVM java wrapper, but every child VM
exited before the host controller started because real-JDK mode had no
native for:

```text
java/lang/ProcessEnvironment.environ()[[B
```

```text
Missing native method in real-JDK mode method=java/lang/ProcessEnvironment.environ()[[B
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/UnsatisfiedLinkError: java/lang/ProcessEnvironment.environ()[[B
```

A follow-up layer, reached after that native was added in a minimized probe,
failed next on the private static native class initializer
`java/lang/ProcessImpl.init()V`.

Original repro (Azure host, worktree since pruned):

```bash
cd /data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner
export JAVA_HOME=/home/victor/jdk25 PATH=/home/victor/jdk25/bin:$PATH
export WILDFLY=/data/cratonvm/apps/wildfly
export CRATONVM_BIN=/data/wt/wt-wildfly-nonpassed-20260705-035722/cratonvm-wildfly-nonpassed-20260705-035722
export JDK25=/home/victor/jdk25 JDK25_WIN=/home/victor/jdk25
export MVNW=/home/victor/.m2/wrapper/dists/apache-maven-3.9.11/a2d47e15/bin/mvn
./run-suite.sh run --category failed --start 1 --count 10 --jit on --class-to 300 --tag azure-nonpassed-jiton-004
```

## Root cause and fix

Both natives were already missing from the real-JDK essential path at the
time this doc was written, but were added the same day in `a3728860` ("Fix
WildFly non-passed suite blockers", 2026-07-05 16:07 UTC) — after the repro
binary above was built (~03:45 UTC the same day), so the doc was stale by
the time anyone looked at it again.

`native-builtins/src/lib.rs::register_essential_natives` (the real-JDK-mode
registration path, reachable via `../../../../vm/src/vm/vm_init.rs`'s
`use_synthetic_jdk == false` branch) now registers:

- `java/lang/ProcessEnvironment.environ()[[B` →
  `native_process_environment_environ` (`native-builtins/src/lib.rs:54`):
  builds a `byte[][]` from `std::env::vars_os()`, alternating key/value byte
  arrays, using `ctx.pin_native_root`/`read_native_pin`/`unpin_native_roots`
  to keep the outer reference array reachable across the per-cell
  `ctx.new_array` allocations while it's being populated.
- `java/lang/ProcessImpl.init()V` → `native_noop` — CratonVM already
  overrides `ProcessBuilder.start()` (`../../../../native-io/src/process.rs`,
  `register_process_natives`, called from `register_io_natives` in both
  real and synthetic modes) for actual subprocess creation, so the
  `<clinit>` native just needs to complete without error.

## Verification (2026-07-06)

No Maven/Surefire WildFly harness is provisioned on the current Azure build
host (`/data/data`), so verification used a standalone probe that exercises
the exact real JDK bytecode paths from the original failure, rather than a
hand-rolled stub:

```java
ProcessBuilder pb = new ProcessBuilder("/bin/echo", "hello-from-child");
Map<String, String> env = pb.environment(); // -> ProcessEnvironment.<clinit> -> environ()
Process p = pb.start();                     // -> ProcessImpl <clinit>/init() + real spawn
int code = p.waitFor();
```

Built in an isolated worktree (`verify/wildfly-processenv-20260706`, off
`origin/dev` @ `c2409f69`) with its own binary
(`cratonvm-processenv-verify-20260706`), run as
`./cratonvm-processenv-verify-20260706 --java-home /home/victor/jdk25 -cp . ProcEnvProbe`:

```text
env-size=17
has-PATH=true
exit=0
PROBE-OK
```

Matches real HotSpot (`/home/victor/jdk25/bin/java ProcEnvProbe`) exactly
(`env-size=17`, `exit=0`). No `UnsatisfiedLinkError` for either native.

Moved out of `../../../known-issues` per the "only unfixed bugs" convention.
