# Spring `jmx.access` MXBean proxy: `getThreadInfo(long)` operation-signature mismatch

**Status:** OPEN (narrow residual — the platform-MXBean registration gap and
the `MXBeanMapping.toOpenValue`/`fromOpenValue` `AbstractMethodError` that
this doc originally tracked are both FIXED and merged to dev).

## Symptom

With platform-MXBean registration and JMX OpenType value marshalling both
fixed, `MBeanClientInterceptorTests`/`RemoteMBeanClientInterceptorTests` now
pass 3 of their 4 `mxBean*` test methods. `mxBeanAttributeAccess()` fully
passes. `mxBeanOperationAccess()` still fails:

```
org.springframework.jmx.access.InvocationFailureException: JMX access failed
	at org.springframework.jmx.access.MBeanClientInterceptor.doInvoke(MBeanClientInterceptor.java:453)
	at jdk.proxy1.$Proxy23.getThreadInfo(Unknown Source)
Caused by: javax.management.ReflectionException: Operation getThreadInfo exists but not with this signature: (long)
```

`MBeanServer.invoke(objectName, "getThreadInfo", args, signature=["long"])`
can't find a matching `MBeanOperationInfo` even though `getThreadInfo(long)`
is a real method on the `ThreadMXBean` interface (confirmed already
correctly enumerated by `native_introspector_get_methods` in
`native-builtins/src/jmx_openmbean.rs` — dedups by (name, descriptor) pair,
not just name, so overloads should survive). The mismatch is most likely in
how parameter-type NAMES get reported into the `MBeanOperationInfo` signature
array at invoke-dispatch time (e.g. a `Method` mirror's parameter-type
string, or a JMX operation-descriptor-building path that mishandles a
primitive `long` parameter for an overloaded native method — `ThreadMXBean`
has several `getThreadInfo` overloads: `(long)`, `(long,int)`, `(long[])`,
`(long[],int)`; natives already exist for these on `sun/management/ThreadImpl`
per `native-builtins/src/jmx.rs:1636-1674`, so the issue is in the reflective
operation-signature path, not the natives themselves).

## Scope

2 test methods (`mxBeanOperationAccess()` in `MBeanClientInterceptorTests`
and `RemoteMBeanClientInterceptorTests`). HotSpot baseline on the same host:
321/321 passing (0 failures), confirming this is CratonVM-specific. Current
CratonVM state on this same suite: 319/321.

## Repro

Azure Linux host, JDK 25 real mode (build cratonvm-cli from dev; use a
private Linux-patched copy of `run-suite.sh` for the classpath-separator /
cygpath-shim / `java.exe` workarounds this host needs — see any
`fix/jmx-*` worktree for the pattern):

```
cd /data/cratonvm/apps/spring-suite-runner
export PATH=~/localbin:/usr/bin:/bin:$PATH
CRATONVM_BIN=<built binary> SPRING=/data/cratonvm/apps/spring-framework \
  JDK25=/home/victor/jdk25 JDK25_WIN=/home/victor/jdk25 KRUN_STACK=1 \
  <path-to-linux-runner>/run-suite-linux.sh \
  run --jdk real --jit on --batch 1 --only 'jmx\.access\.MBeanClientInterceptorTests'
```

## Not yet investigated

- Where CratonVM builds the JMX operation signature (parameter type name
  list) used for `MBeanServer.invoke()` dispatch matching, and whether it
  mishandles primitive `long`/overloaded native methods.
