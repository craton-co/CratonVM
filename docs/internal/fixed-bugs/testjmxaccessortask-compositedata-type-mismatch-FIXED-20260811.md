# `TestJMXAccessorTask` — `getCompositeType()` was shadowed by a native that returned null

| | |
|---|---|
| **Status** | FIXED 2026-08-11 |
| **HotSpot** | PASS (`OK (1)`) |
| **CratonVM** | PASS (`OK (1)`) |
| **Discovered** | 2026-08-12, complete Tomcat suite rerun on Azure Linux (4 shards) |
| **Fixed in** | `fix/tomcat-resourceset-compositedata-20260811` |

## Symptom

```
javax.management.openmbean.OpenDataException: Argument value of wrong type for
item details: value javax.management.openmbean.CompositeDataSupport(
  compositeType=javax.management.openmbean.CompositeType(name=details, items=(…)),
  contents={name=alpha}),
type javax.management.openmbean.CompositeType(name=details, items=(…))
```

The value's own printed `CompositeType` and the `type` it is checked against
are identical, and in fact are the *same object* — the test builds `details`
from `detailsType` and then puts it in a row whose `details` item is
`detailsType`.

## The suspected cause on the original page was wrong

That page proposed `CompositeType.equals()` (or `SimpleType`/`OpenType.equals()`
beneath it) failing to match two structurally-identical instances, "possibly an
identity-based shortcut somewhere". None of that is what happened. A probe
(`CtProbe`/`CtProbe2`, run against JDK 25 and CratonVM on the same host) put
every step side by side:

| | HotSpot | CratonVM (pre-fix) |
|---|---|---|
| `detailsType.keySet()` | `[name]` | `[name]` |
| `detailsType.getType("name")` | `SimpleType(java.lang.String)` | same |
| `nameToType` TreeMap (reflected) | `{name=SimpleType(...)}` | same |
| `isAssignableFrom(self)` (reflected) | `true` | `true` |
| **field** `compositeType` `== detailsType` | `true` | **`true`** |
| **method** `getCompositeType() == detailsType` | `true` | **`false` — returns null** |

`equals`, `isAssignableFrom`, `TreeMap`, `SimpleType` and the field write were
all correct. The trivial getter was the only thing that lied. Two lessons in
one row: the field and the accessor disagreed, and the diagnostic that named
the failure (`OpenDataException`'s message) printed two identical-looking type
descriptions precisely *because* it never got to compare them.

## Root cause

`native-builtins/src/jmx_openmbean.rs` registers seven natives on the two real
JDK open-data carrier classes:

* `CompositeDataSupport`: `get(String)`, `containsKey(String)`,
  `getCompositeType()`, `getAll(String[])`
* `TabularDataSupport`: `put(CompositeData)`, `size()`, `isEmpty()`

They serve carriers built by `build_composite_data` / `build_tabular_data`,
which keep their state on two CratonVM-private fields, `cratonvm$contents` and
`cratonvm$openType`, and read it back with `get_field_by_name`.

Native registration is per `(class, method, descriptor)` and therefore global,
so those natives also intercepted every `CompositeDataSupport` /
`TabularDataSupport` the *application* constructed through the JDK's own
bytecode. Those instances keep their state in the JDK's `contents` /
`compositeType` (resp. `dataMap` / `tabularType`) and have neither private
field, so the natives answered `null` / `false` / `0` for all of them.
`getCompositeType()` returning null is what made `CompositeType.isValue()`
reject a value against the very type it was built from, inside
`CompositeDataSupport`'s own constructor.

`TabularDataSupport` was worse than a wrong answer: the native `put` wrote into
the carrier map while `values()`, `keySet()` and `entrySet()` — which have no
native — read the JDK's own `dataMap`. Anything an application put into a table
was invisible to anything it read out. On the pre-fix binary a two-row
round-trip does not even get that far:

```
javax.management.openmbean.InvalidOpenTypeException: Argument value's composite
type [null] is not assignable to this TabularData instance's row type […]
	at javax/management/openmbean/TabularDataSupport.put
```

## Fix

Each of the seven natives now decides per *instance* rather than per class.
`is_synthetic_carrier` checks for `cratonvm$contents` / `cratonvm$openType`; an
instance without them goes to `invoke_virtual_bytecode_only`, which reaches the
receiver's real JDK bytecode without re-entering the registration.

This is the same real-vs-synthetic-by-instance problem the
`ThreadPoolExecutor.execute` / `submit` / `shutdown` natives already solved, and
`invoke_virtual_bytecode_only` exists for exactly it — including the note that
routing through `invoke_virtual` instead reintroduces infinite recursion,
because `invoke_on_class_shared_inner` re-finds the same native regardless of
the first override gate.

The delegation is additionally gated on `!is_class_synthetic_stub(class)` so
`synthetic-jdk` mode, where there is no bytecode to fall back to, keeps the
carrier behaviour unchanged.

The registrations are kept, not deleted. Deleting a registration on a
"nothing reaches it" reading is how 65 live natives were lost once already;
gating is the answer to a shadowing native, the same way it is the answer to a
hard-coded compatibility name list.

## Residual noted while fixing (separate bug, not caused by this change)

`build_composite_data` / `build_tabular_data` have **no caller outside this
module's tests**. Under `real-jdk` the new predicate is therefore true for
every instance and the seven natives defer wholesale. The per-instance check is
kept anyway: it is what keeps them correct the moment a caller mints a carrier
again.

That has a visible consequence which is *not* fixed here and predates this
change (confirmed identical on the pre-fix binary):

```
MBeanServer.getAttribute("java.lang:type=Memory", "HeapMemoryUsage")
  HotSpot  -> javax.management.openmbean.CompositeDataSupport
  CratonVM -> java.lang.management.MemoryUsage
```

CratonVM's platform `MBeanServer` hands back the raw MXBean value instead of
converting it to open types. Filed separately as
`docs/known-issues/mxbean-getattribute-returns-raw-value-not-compositedata.md`.

## Verification

```
[rc=0] org.apache.catalina.ant.jmx.TestJMXAccessorTask :: OK (1 test)
```

An application-built `TabularDataSupport` round-trip (`MxProbe`) now matches
HotSpot exactly — `size=2 isEmpty=false valuesCount=2 ids=[x, y]` on both,
where the pre-fix binary threw `InvalidOpenTypeException`.

Reproduction:

```bash
source /data/toolchain/env.sh
cd apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
<cratonvm> --java-home /data/toolchain/jdk-25 -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.ant.jmx.TestJMXAccessorTask
```

Regression coverage:
`only_cratonvm_built_carriers_are_answered_by_the_carrier_natives` in
`native-builtins/src/jmx_openmbean.rs` drives both branches of the
discriminator. Its first version could not: the mock resolves a field name only
when a test declares it via `set_declared_fields`, so the positive branch was
unreachable and the test failed on the assertion it was meant to pin.
