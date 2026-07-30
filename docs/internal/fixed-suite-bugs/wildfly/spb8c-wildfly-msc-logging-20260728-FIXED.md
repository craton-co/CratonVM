# SPB.8c (WildFly security manager, MSC, logging) - FIXED 2026-07-29

**Status:** CLOSED. The three blanket JIT bans for `org/wildfly/`,
`org/jboss/msc/`, and `org/jboss/logging/` were removed from
`vm/src/jit/skip_list.rs`.

## Original report

The historical SIGSEGV was attributed to a corrupted reference reaching
`Long.parseLong` after the WildFly security-manager reflective-property path.
Subsequent startup traces associated the same failure archetype with
`ServiceName.equals` and JBoss Logging initialization. Those correlations
created the three package-wide bans but did not establish a reproducible
miscompile in any one of the packages.

## Resolution and regression witness

`vm/tests/wildfly_boot_fixtures/Spb8cWildflyMscLoggingProbe.java` uses the
real WildFly 32.0.1.Final module jars and directly drives every named path:

- `ReadPropertyAction.run` -> `Long.parseLong` and the package-private
  `GetAccessibleDeclaredFieldAction.run` -> `AccessibleObject.setAccessible`
  chain, plus `WildFlySecurityManager` construction;
- `ServiceName.equals`, `hashCode`, and `length`;
- `LoggerProvider.getLogger` and `JDKLogger` construction.

On the Azure Linux fixture, HotSpot and CratonVM JIT (with all three packages
explicitly allowed before removal) and `--nojit` each completed the same
20,000 iterations and 160,000 assertions with zero failures. A JIT compilation
trace additionally confirmed C1/C2 compilation of `ReadPropertyAction.run`,
`ServiceName.equals`, and the JBoss Logging provider/JDKLogger paths.

## Real WildFly check

With all three packages JIT-eligible, WildFly 32.0.1.Final passed the historical
WildFly Security, MSC, and JBoss Logging startup region without the corrupted
reference marker or SIGSEGV. The current fixture's full JIT boot stalls even
with all three bans restored, whereas `--nojit` reaches `WFLYSRV0026` in
42.626 seconds with the known JKS-provider fixture error. That late JIT stall is
therefore a separate baseline issue and is not a justification for retaining
the SPB.8c package bans.

## Evidence

- HotSpot: `SPB8C_PROBE_RESULT classes=3 iterations=2000 assertions=16000 failed=0`.
- CratonVM JIT: `SPB8C_PROBE_RESULT classes=3 iterations=20000 assertions=160000 failed=0`.
- CratonVM no-JIT: `SPB8C_PROBE_RESULT classes=3 iterations=20000 assertions=160000 failed=0`.
- JIT trace: compiled `ReadPropertyAction.run`, `ServiceName.equals`,
  `JDKLoggerProvider.getLogger`, and `JDKLogger.<init>`.
- WildFly no-JIT control: `WFLYSRV0026`, 273 of 522 services started; the five
  expected failures are the independent unavailable-JKS-provider fixture gap.
