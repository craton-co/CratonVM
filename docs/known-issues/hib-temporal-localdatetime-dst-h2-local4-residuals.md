# Hibernate `LocalDateTimeTest` residuals - DST round-trip skew + H2 `<local4>` connect NPE

**Status:** OPEN - separate from the retired `DdlTypeImpl.getRawTypeName` / empty `IllegalThreadStateException` note.
**Severity:** Medium for the temporal suite slice; currently observed as 1-2 failures in `LocalDateTimeTest`.

After the 2026-07-08 Hibernate JIT guard retired `hib-temporal-residuals-typename-npe-illegalthreadstate.md`, `LocalDateTimeTest` still exposes different failures:

1. `AssertionFailedError: Writing then reading a value should return the original value ==> expected: <2018-10-28T01:00> but was: <2018-10-28T00:00>`
2. Intermittent H2 connection failure: `Error calling Driver.connect() [General error: "java.lang.NullPointerException: Cannot read the array length because ""<local4>"" is null" [50000-240]]`

These are not the fixed signatures: the final 2026-07-08 `LocalDateTimeTest` probe had 0 `typeNamePattern`, 0 `IllegalThreadStateException`, and 0 `NoSuchMethodError`.

## Repro

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
echo org.hibernate.orm.test.type.temporal.LocalDateTimeTest > /data/data/logs/hib-temporal-residuals-20260708-170734/localdatetime-fixed-final.list
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  /data/data/bin/cratonvm-hib-temporal-residuals-20260708-170734-fixed \
  --java-home /home/victor/jdk25 --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner \
  /data/data/logs/hib-temporal-residuals-20260708-170734/localdatetime-fixed-final.list 0
```

Observed on 2026-07-08: `found=162 started=162 ok=88 failed=2 aborted=72 skipped=0`.

## Notes

The DST skew resembles the previously fixed one-hour temporal bug family but reappears for `LocalDateTimeTest` on the current harness. The H2 `<local4>` connect NPE is intermittent and also appeared while narrowing Hibernate JIT containment; treat it as a separate residual until a trace identifies whether it is H2, connection setup, or a remaining VM/JIT interaction.
