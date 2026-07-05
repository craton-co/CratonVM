# WildFly Domain Child VM Missing ProcessEnvironment.environ Native

Status: open

Date observed: 2026-07-05

## Summary

The Azure WildFly JIT-on non-passed rerun now reaches Surefire and starts the
WildFly domain process through the CratonVM java wrapper, but every child VM exits
before the host controller starts because real-JDK mode has no native for:

```text
java/lang/ProcessEnvironment.environ()[[B
```

The parent test reports only a container startup timeout, but the copied Surefire
system output shows the child process root cause:

```text
Missing native method in real-JDK mode method=java/lang/ProcessEnvironment.environ()[[B
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/UnsatisfiedLinkError: java/lang/ProcessEnvironment.environ()[[B
```

## Repro

Azure host worktree:

```bash
cd /data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner
export JAVA_HOME=/home/victor/jdk25 PATH=/home/victor/jdk25/bin:$PATH
export WILDFLY=/data/cratonvm/apps/wildfly
export CRATONVM_BIN=/data/wt/wt-wildfly-nonpassed-20260705-035722/cratonvm-wildfly-nonpassed-20260705-035722
export JDK25=/home/victor/jdk25 JDK25_WIN=/home/victor/jdk25
export MVNW=/home/victor/.m2/wrapper/dists/apache-maven-3.9.11/a2d47e15/bin/mvn
./run-suite.sh run --category failed --start 1 --count 10 --jit on --class-to 300 --tag azure-nonpassed-jiton-004
```

Primary result directory:

```text
apps/wildfly-suite-runner/out/azure-nonpassed-jiton-004-jit-real-failed-20260705-043353
```

## Notes

JDK 25 `java.lang.ProcessEnvironment` expects `environ()` to return a `byte[][]`
with alternating key and value byte arrays. The native must use the host process
environment and pin the outer reference array while constructing nested byte
arrays so a moving GC cannot relocate an unrooted array during allocation.


## Follow-up Layer

After adding `ProcessEnvironment.environ()[[B`, the minimized environment probe
advanced to the next real-JDK process class initializer and failed on:

```text
java/lang/ProcessImpl.init()V
```

JDK 25 declares this as a private static native class initializer in
`java.lang.ProcessImpl`. For CratonVM's current process-launch path it can be a
no-op bridge: CratonVM already overrides `ProcessBuilder.start()` for actual
subprocess creation, while this native only needs to let `ProcessImpl.<clinit>`
complete when `ProcessBuilder.environment()` initializes process support.
