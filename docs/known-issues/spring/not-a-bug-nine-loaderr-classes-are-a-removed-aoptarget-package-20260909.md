# The 9-class Spring Framework LOADERR cluster is NOT a CratonVM bug — the whole `org.springframework.aop.target` test package no longer exists in this checkout

| | |
|---|---|
| **Status** | CLOSED as not-a-CratonVM-bug, filed 2026-09-09. |
| **Scope** | `run-suite.sh`'s `LOADERR` status, `KRun`'s runner harness. Identical on `craton` and `hotspot`. |
| **Population** | Exactly 9 classes, identical set on every arm and every run checked (2026-09-03 full 3-GC sweep, 2026-09-07 non-passed rerun 3-GC sweep) — `LOADERR=9` on Generational, G1, **and** ZGC, both dates. |

## The classes

```
org.springframework.aop.target.CommonsPool2TargetSourceProxyTests
org.springframework.aop.target.CommonsPool2TargetSourceTests
org.springframework.aop.target.HotSwappableTargetSourceTests
org.springframework.aop.target.LazyCreationTargetSourceTests
org.springframework.aop.target.LazyInitTargetSourceTests
org.springframework.aop.target.PrototypeBasedTargetSourceTests
org.springframework.aop.target.PrototypeTargetSourceTests
org.springframework.aop.target.ThreadLocalTargetSourceTests
org.springframework.aop.target.dynamic.RefreshableTargetSourceTests
```

## What `LOADERR` actually means

`KRun.java` (the suite's per-class JUnit-launcher entry point) wraps the
entire discover-and-execute sequence — starting with `Class.forName(name)` —
in one `catch (Throwable)`. Anything that escapes before a single test result
comes back prints `LOADERR <class> :: <exception>` instead of a normal
`RESULT` line. It says nothing about which VM ran the bytecode; it says the
class object was never obtained.

## The evidence

`failcauses.log` from the 2026-09-07 full 3-GC sweep names the exact
exception for all 9, unconditionally:

```
LOADERR org.springframework.aop.target.CommonsPool2TargetSourceProxyTests :: java.lang.ClassNotFoundException: org.springframework.aop.target.CommonsPool2TargetSourceProxyTests
LOADERR org.springframework.aop.target.HotSwappableTargetSourceTests :: java.lang.ClassNotFoundException: org.springframework.aop.target.HotSwappableTargetSourceTests
... (all 9, same shape)
```

`java.lang.ClassNotFoundException` on the class's own name is
`Class.forName` failing outright — not a test failure, not a VM crash, not a
timeout. The class was never on the classpath handed to the JVM.

**The source doesn't exist either.** This checkout's
`spring-aop/src/test/java/org/springframework/aop/` has exactly six
subpackages:

```
aspectj  config  framework  interceptor  scope  support
```

There is no `target` package — not as source, not as a compiled `.class`
anywhere under `spring-aop/build/classes`. `find` across the entire
`apps/spring-framework` checkout for any of the 9 class names, source or
compiled, returns nothing. The package was removed or relocated upstream at
some point before this checkout's revision, and whatever generated the
suite's class-discovery list did so from an older revision (or a stale
cached list) that still names it.

## Confirmed identical on stock HotSpot

```
$ ./run-suite.sh hotspot --only 'CommonsPool2TargetSourceProxyTests' --tag loaderr-verify
classes: LOADERR=1
test-methods: found=0 passed=0 failed=0
```

Same `LOADERR`, same instant failure (`wall=0s`), same cause — HotSpot cannot
find a class that was never compiled either. This is unconditional: the
`.class` file does not exist for any VM to load, so a per-VM behavioral
difference is not possible here.

## Disposition

Not a CratonVM defect. All 9 are removed from any future "CratonVM
regression" count; they should also be pruned from whatever generates this
suite's class-discovery list so they stop appearing as `LOADERR` noise in
every future sweep. This does not need re-investigating.

## Repro

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> SPRING=<spring-framework checkout> \
  ./run-suite.sh hotspot --only 'CommonsPool2TargetSourceProxyTests' --tag verify
# classes: LOADERR=1 on hotspot too, instantly
```
