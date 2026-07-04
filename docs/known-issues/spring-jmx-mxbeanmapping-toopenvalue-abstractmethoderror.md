# Spring `jmx.access` MXBean proxy: `MXBeanMapping.toOpenValue` AbstractMethodError + InvocationFailureException

**Status:** OPEN (residual, uncovered after fixing the `FileDispatcherImpl.init0`
native gap — see `docs/internal/` fix commit for that cluster).

## Symptom

With the `FileDispatcherImpl.init0()V` `UnsatisfiedLinkError` fixed (which
previously aborted these tests before their bodies ran at all — see the
related fix commit), `MBeanClientInterceptorTests` and
`RemoteMBeanClientInterceptorTests` now execute their `mxBean*` test methods
but two of the four fail with distinct errors:

- `mxBeanAttributeAccess()`:
  ```
  java.lang.AbstractMethodError: method com/sun/jmx/mbeanserver/MXBeanMapping.toOpenValue(Ljava/lang/Object;)Ljava/lang/Object; has no Code attribute
      at com.sun.jmx.mbeanserver.ConvertingMethod.invokeWithOpenReturn(ConvertingMethod.java:195)
      at com.sun.jmx.mbeanserver.MXBeanIntrospector.invokeM2(MXBeanIntrospector.java:115)
      ...
      at com.sun.jmx.mbeanserver.MXBeanProxy$GetHandler.invoke(MXBeanProxy.java:122)
      at javax.management.MBeanServerInvocationHandler.invoke(MBeanServerInvocationHandler.java:258)
      at org.springframework.jmx.access.MBeanClientInterceptor.doInvoke(MBeanClientInterceptor.java:405)
  ```
  This says the *concrete* runtime subclass CratonVM resolves for
  `com.sun.jmx.mbeanserver.MXBeanMapping` (an abstract JDK-internal class
  with several private concrete converter subclasses, e.g. `IdentityMapping`,
  chosen by `MXBeanMappingFactory` based on the attribute's Java type) has no
  bytecode body for `toOpenValue` — i.e. CratonVM appears to be resolving/
  loading the abstract declaration itself (or a synthetic stand-in) rather
  than the real concrete subclass the JDK would pick.

- `mxBeanOperationAccess()`:
  ```
  org.springframework.jmx.access.InvocationFailureException: JMX access failed
      at org.springframework.jmx.access.MBeanClientInterceptor.doInvoke(MBeanClientInterceptor.java:453)
      at jdk.proxy1.$Proxy23.getThreadInfo(Unknown Source)
  ```
  Thrown from the operation-invocation path (`getThreadInfo()` on the
  `ThreadMXBean` proxy) rather than the attribute-access path — plausibly the
  same underlying `MXBeanMapping` resolution gap, hit while converting the
  method's return value instead of an attribute value (Spring's own exception
  wrapping hides the original cause here; needs a raw stack trace with
  cause chain to confirm it's the same root issue).

## Scope

4 test methods across 2 classes (`MBeanClientInterceptorTests`,
`RemoteMBeanClientInterceptorTests`), both `mxBeanAttributeAccess()` and
`mxBeanOperationAccess()`. HotSpot baseline on the same host: 321/321 passing
(0 failures), confirming this is CratonVM-specific.

## Repro

Azure Linux host, `/data/wt/wt-jmx-rmi-cluster` (branch `fix/jmx-rmi-cluster`),
JDK 25 real mode:

```
cd /data/cratonvm/apps/spring-suite-runner
export PATH=~/localbin:/usr/bin:/bin:$PATH   # needed for the cygpath shim
CRATONVM_BIN=<built binary> SPRING=/data/cratonvm/apps/spring-framework \
  JDK25=/home/victor/jdk25 JDK25_WIN=/home/victor/jdk25 KRUN_STACK=1 \
  /data/wt/wt-jmx-rmi-cluster/runner-linux/run-suite-linux.sh \
  run --jdk real --jit on --batch 8 --only 'jmx\.'
```

## WIP branch (unmerged, incomplete)

`fix/jmx-platform-mxbean-registration` (pushed to origin, not merged) has a
partial attempt at this: registers `com/sun/jmx/mbeanserver/MXBeanMapping.
toOpenValue`/`fromOpenValue` as identity-passthrough natives (targeting the
AbstractMethodError directly), plus makes `MemoryMXBean`'s heap `max` report
the real configured `-Xmx` instead of the `-1` sentinel. As of the last run
before that worktree was shut down, the targeted 2-class probe
(`MBeanClientInterceptorTests` + `RemoteMBeanClientInterceptorTests`, 28
test methods) was at 13/28 passing (up from 11/28 baseline) -- progress, but
not resolved. Whoever picks this up next should start from that branch
rather than re-deriving the `toOpenValue` identity-passthrough approach from
scratch, but should NOT merge it into `dev` without finishing verification
against the full jmx suite (`--only 'jmx\\.'`) and the HotSpot 321/321
baseline.

## Not yet investigated

- Where CratonVM resolves `com.sun.jmx.mbeanserver.MXBeanMapping` subclasses
  (native registration vs real bytecode) — check whether this class or its
  concrete converter subclasses are getting a synthetic/stub treatment
  somewhere in `native-builtins/src/jmx.rs` or a generic "abstract JDK-internal
  class treated as instantiable" path, similar to prior `AbstractMethodError`
  root causes seen elsewhere in this codebase (redefine/structural-check
  order sensitivity, synthetic stub shadowing real bytecode).
- Whether `mxBeanOperationAccess()`'s `InvocationFailureException` shares the
  exact same root cause as the attribute-access `AbstractMethodError` (needs
  the wrapped cause, not just Spring's summary message).
