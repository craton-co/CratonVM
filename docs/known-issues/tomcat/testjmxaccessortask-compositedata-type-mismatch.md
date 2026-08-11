# `TestJMXAccessorTask` — `CompositeDataSupport` rejects a value against its own declared type

| | |
|---|---|
| **Status** | OPEN |
| **HotSpot** | PASS (`OK (1)`, fresh-verified 2026-08-12, Azure Linux fixture) |
| **CratonVM** | FAIL, reproduces |
| **Discovered** | 2026-08-12, complete Tomcat suite rerun on Azure Linux (4 shards) |

## Symptom

```
1) testCreatePropertyForTabularDataSupport(org.apache.catalina.ant.jmx.TestJMXAccessorTask)
javax.management.openmbean.OpenDataException: Argument value of wrong type for
item details: value javax.management.openmbean.CompositeDataSupport(
  compositeType=javax.management.openmbean.CompositeType(name=details,
    items=((itemName=name,itemType=javax.management.openmbean.SimpleType(name=java.lang.String)))),
  contents={name=alpha}),
type javax.management.openmbean.CompositeType(name=details,
  items=((itemName=name,itemType=javax.management.openmbean.SimpleType(name=java.lang.String))))
```

Read that error closely: the value's own `CompositeType` (as printed) and the
`type` it's being checked against (also as printed) look identical —
`CompositeDataSupport`'s constructor is rejecting a value against a type
description that appears to match, which is exactly the kind of failure you
get when two `CompositeType`/`OpenType` instances describing the *same shape*
fail an `equals()` check they should pass (rather than the value genuinely
having a mismatched shape).

## Reproduction

```bash
source /data/toolchain/env.sh
cd apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
<cratonvm> --java-home /data/toolchain/jdk-25 -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.ant.jmx.TestJMXAccessorTask
```

HotSpot control: `OK (1 test)` in 0.07s.

## Suspected root cause (not yet isolated)

Likely `javax.management.openmbean.CompositeType.equals()` (or `SimpleType`/
`OpenType.equals()` beneath it) not matching two structurally-identical
instances on CratonVM — possibly an identity-based shortcut somewhere in the
comparison, or a field CratonVM's `CompositeType` doesn't populate/compare the
same way HotSpot's does (e.g. an internal description string or item-ordering
array). Not yet checked against source. No existing known-issue doc covers
this signature.
