# Hibernate `LocalDateTimeTest` residuals - DST round-trip skew + H2 `<local4>` connect NPE - FIXED

**Status:** FIXED on 2026-07-08 in branch `codex/hib-localdatetime-residuals-20260708-200054`.
**Severity:** Medium for the Hibernate temporal suite slice; previously observed as 1-2 `LocalDateTimeTest` failures.

This note was split from the retired `DdlTypeImpl.getRawTypeName` / empty `IllegalThreadStateException` Hibernate temporal note after `LocalDateTimeTest` still exposed two different residuals:

1. `AssertionFailedError: Writing then reading a value should return the original value ==> expected: <2018-10-28T01:00> but was: <2018-10-28T00:00>`
2. Intermittent H2 connection failure: `Error calling Driver.connect() [General error: "java.lang.NullPointerException: Cannot read the array length because ""<local4>"" is null" [50000-240]]`

## Root Cause

The DST skew had two cooperating causes:

- `TimeZone.getDefault()` / `getDefaultRef()` returned a synthetic UTC object instead of honoring `TimeZone.setDefault(...)`, so Java code using the process default zone never saw the Paris DST transition configured by the test.
- The native `java.util.Date` / `java.sql.Timestamp` deprecated field constructors and `Timestamp.toLocalDateTime()` converted fields as UTC instead of through the current default `TimeZone`.

The H2 `<local4>` NPE was a separate moving-GC root bug in the `Properties.keySet()` path. H2 `ConnectionInfo.readProperties(Properties)` does:

```text
0: aload_1
1: invokevirtual java/util/Properties.keySet:()Ljava/util/Set;
4: invokeinterface java/util/Set.toArray:()[Ljava/lang/Object;
9: astore_2
...
15: aload 4
17: arraylength
```

The failing local 4 was the array returned by `keySet().toArray()`. The stale-receiver warnings for `Object.hasNext()` / `Object.getValue()` came from `native-builtins/src/properties_sidetable.rs::chm_extra_entries`: it walked the real `Properties.map` CHM with unrooted iterator and entry `ObjectRef`s across re-entrant Java calls. A moving GC could stale those raw Rust locals, corrupting the CHM-extra key merge; `Properties.keySet()` could then hand H2 a stale or incomplete view before the immediate `toArray()`.

## Fix

- `../../../../native-builtins/src/lib.rs`: `TimeZone.getDefault()`, `getDefaultRef()`, and `setDefaultZone()` now share the Java static `TimeZone.defaultTimeZone` object instead of synthesizing UTC on every call.
- `../../../../native-builtins/src/deprecated_util.rs`: deprecated `Date` field constructors/getters/setters now convert through the current default time zone, including DST offsets.
- `../../../../native-builtins/src/jdbc.rs`: `Timestamp(int,int,int,int,int,int,int)`, `Timestamp.toString()`, and `Timestamp.toLocalDateTime()` use the default-zone-aware conversion and allocate `LocalDateTime` with the real JDK field layout.
- `../../../../native-builtins/src/properties_sidetable.rs`: `chm_extra_entries()` now roots the CHM iterator, current entry, and returned key/value refs across Java calls; `Properties.keySet()` roots the returned set while CHM-only keys are appended and rebuilds String keys after the CHM walk.

## Verification

Built fixed binary:

```text
/data/data/bin/cratonvm-hib-localdatetime-residuals-20260708-200054-fixed
```

Focused probes:

- `/data/data/probes/TzDstEdgeProbe20260708.java`: Paris 2018-10-28 01:00 round trips through `Timestamp` as `2018-10-28T01:00`.
- `/data/data/probes/H2PropertiesToArrayProbe20260708.java`: string-only and CHM-extra non-string-valued `Properties.keySet().toArray()` loops pass, then H2 `DriverManager.getConnection(...)` succeeds.

Hibernate `LocalDateTimeTest` reruns with the fixed binary:

```text
/data/data/logs/hib-localdatetime-residuals-20260708-200054/localdatetime-fixed-run4.log
/data/data/logs/hib-localdatetime-residuals-20260708-200054/localdatetime-fixed-run5.log
/data/data/logs/hib-localdatetime-residuals-20260708-200054/localdatetime-fixed-run6.log
/data/data/logs/hib-localdatetime-residuals-20260708-200054/localdatetime-fixed-run7.log
```

All four runs reported:

```text
found=162 started=162 ok=90 failed=0 aborted=72 skipped=0
```

Across those runs: 0 DST assertions, 0 H2 `<local4>`, 0 `typeNamePattern`, 0 `IllegalThreadStateException`, 0 `NoSuchMethodError`, and 0 stale-receiver warnings.

## 2026-07-08 Follow-up: direct `CET` alias DST rule

The original `LocalDateTimeTest` residual above was already fixed in `dev` by the default-zone/Timestamp and `Properties.keySet().toArray()` repairs. A later direct check found a narrower `TimeZone.getTimeZone("CET")` mismatch: CratonVM returned only the standard +01:00 offset for `2018-10-28T00:30Z`, while HotSpot still returns +02:00 until the EU DST transition at `2018-10-28T01:00Z`.

Follow-up fix: add `CET` to the EU recurring DST rule arm in `../../../../native-builtins/src/lib.rs` and extend `scratch-min/TzDstProbe.java` to cover the alias.
