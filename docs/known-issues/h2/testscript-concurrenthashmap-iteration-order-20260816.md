# `ConcurrentHashMap` iterates in a different order than the JDK's, and `SCRIPT` output inherits it

## Status
**OPEN, root-caused, not fixed (2026-08-16).** 4 of the 15
`org.h2.test.scripts.TestScript` errors on `dev` @ `496bc3c2c`. HotSpot JDK 25
on the same classpath reports 0 errors. Part of the census in
[`testscript-sql-divergences-20260816.md`](testscript-sql-divergences-20260816.md).

Not fixed because the fix is "reimplement the JDK's `ConcurrentHashMap` table
layout", which is a much larger change than the symptom justifies — see
"Fixing it".

## The failure

```
ERROR: org/h2/test/scripts/testScript.sql
line: 6425
exp: > ALTER TABLE "PUBLIC"."A_TEST" ADD CONSTRAINT "PUBLIC"."MIN_LENGTH" CHECK(CHAR_LENGTH("A_VARCHAR") > 1) NOCHECK;
got: > ALTER TABLE "PUBLIC"."B_TEST" ADD CONSTRAINT "PUBLIC"."CONSTRAINT_76" CHECK(CHAR_LENGTH("B_VARCHAR") > 1) NOCHECK;
------------------------------
ERROR: org/h2/test/scripts/testScript.sql
line: 6425
exp: > ALTER TABLE "PUBLIC"."A_TEST" ADD CONSTRAINT "PUBLIC"."DATE_UNIQUE" UNIQUE NULLS DISTINCT ("A_DATE");
got: > ALTER TABLE "PUBLIC"."A_TEST" ADD CONSTRAINT "PUBLIC"."DATE_UNIQUE_2" UNIQUE NULLS DISTINCT ("A_DATE");
------------------------------
```

(and the two mirror-image rows — the pairs are swapped, nothing is missing or
duplicated). The statement is `SCRIPT NOPASSWORDS NOSETTINGS NOVERSION;`, whose
expected output is a `rows (ordered): 14` block, so `TestScript` compares it
line by line.

## What it is

`ScriptCommand` collects every non-`PRIMARY KEY` constraint and sorts:

```java
ArrayList<Constraint> constraints = new ArrayList<>();
for (Schema schema : ...) for (Constraint constraint : schema.getAllConstraints()) { ... }
constraints.sort(null);                                    // ScriptCommand.java:336
```

`Constraint.compareTo` orders **only by constraint type**:

```java
public int compareTo(Constraint other) {
    if (this == other) return 0;
    return Integer.compare(getConstraintType().ordinal(), other.getConstraintType().ordinal());
}
```

so every pair of same-type constraints compares equal, and the emitted order of
`MIN_LENGTH` vs `CONSTRAINT_76` (both `CHECK`) and of `DATE_UNIQUE` vs
`DATE_UNIQUE_2` (both `UNIQUE`) is decided entirely by (a) the stability of the
sort and (b) the order `schema.getAllConstraints()` hands them over.

**(a) is fine.** A direct probe — 6 elements with a type-only comparator, then
64 elements to force a real merge rather than an insertion sort — gives
byte-identical output on both VMs, for `List.sort(null)` and for
`Arrays.sort(Object[])`. CratonVM's object sort is stable, as the JDK requires.

**(b) is the divergence.** `Schema.constraints` is a `ConcurrentHashMap<String,
Constraint>` (`Schema.java:51`) and `getAllConstraints()` returns
`constraints.values()` verbatim. Inserting the eight constraint names from this
test case in script order into a fresh `ConcurrentHashMap` and iterating:

```
HotSpot   [C3, CONSTRAINT_760, MIN_LENGTH, DATE_UNIQUE, CONSTRAINT_76, DATE_UNIQUE_2, B_UNIQUE, CONSTRAINT_7]
CratonVM  [CONSTRAINT_76, DATE_UNIQUE_2, CONSTRAINT_7, C3, CONSTRAINT_760, MIN_LENGTH, DATE_UNIQUE, B_UNIQUE]
```

with **identical `String.hashCode()` values on both VMs** (checked for all
eight). So this is not a hashing difference: it is the map's own bin layout.
CratonVM reimplements `ConcurrentHashMap` natively (`native-collections/src/lib.rs`,
the "segmented CHM path"), and that implementation's bucket assignment, table
sizing and resize policy are its own — not the JDK's `spread()` +
power-of-two-table + bin-order-preserving-transfer scheme that fixes the JDK's
iteration order for a given insertion sequence.

## Is this even a bug?

`ConcurrentHashMap` iteration order is unspecified, and no correct program may
depend on it. H2 does not: `ScriptCommand` sorts. What H2 *does* rely on is that
`SCRIPT`'s output is reproducible for a given database, and the H2 test suite
bakes HotSpot's particular order into an expected-output file.

So: not a JDK spec violation, but a real behavioural divergence with a real
cost. Anything that hashes a set of names and prints them — schema dumps,
generated DDL, `INFORMATION_SCHEMA` walks, serialized `Properties`, JSON object
key order — will differ from HotSpot on CratonVM, and any golden-file test over
such output will fail. This is the first place we have measured it; it will not
be the last.

## Blast radius

Ordering-of-output only. No value is wrong, nothing is lost or duplicated: the
14 rows are all present, two pairs are transposed. Applications that iterate a
`ConcurrentHashMap` for effect rather than for display are unaffected.

## Fixing it

Not attempted. The options, cheapest first:

* **Do nothing, and treat golden-file tests over hash-ordered output as
  expected-divergence.** Costs nothing, keeps the gap.
* **Make the native `ConcurrentHashMap` iterate in JDK bin order.** This is the
  real fix and it is not small: it means matching `spread(h) = (h ^ (h >>> 16))
  & 0x7fffffff`, the power-of-two table with default capacity 16 and load factor
  0.75, bin insertion at the tail, the treeify threshold, and `transfer`'s
  lo/hi split on resize — because the observable order is a function of all of
  them together. Worth scoping only if a second consumer of hash order shows up.
* **Iterate a snapshot sorted by key** inside the native `values()`/`keySet()`.
  Tempting and wrong: it would make CratonVM's order deterministic but still
  not HotSpot's, so it fixes no test, and it costs a sort on every iteration.

The same question applies to `HashMap`; it has not been measured here.

## Repro

The map-order divergence on its own, no H2 needed:

```java
ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
for (String k : new String[]{"CONSTRAINT_7", "CONSTRAINT_760", "MIN_LENGTH", "CONSTRAINT_76",
                             "DATE_UNIQUE", "DATE_UNIQUE_2", "B_UNIQUE", "C3"}) m.put(k, 0);
System.out.println(new ArrayList<>(m.keySet()));
```

```bash
javac -d /tmp/classes ChmOrder.java
java -cp /tmp/classes ChmOrder
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit -c /tmp/classes ChmOrder
```

And in situ, from `apps/h2database/h2` (see the census record for the full
command):

```bash
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```
