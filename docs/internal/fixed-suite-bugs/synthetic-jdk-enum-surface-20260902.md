# The enum surface in `--synthetic-jdk`, and a cluster that wasn't

## The cluster was wrong

`synthetic-jdk-vm-test-triage-20260902.md` grouped four failing tests as an
"enum-constant cluster" and said it was "the one to take first … one defect
wearing four faces". **That was a hypothesis stated with too much confidence,
and measurement refutes it.** All four are stale tests, and none of them was
exercising an enum defect:

| test | what it actually does |
|---|---|
| `countdown_latch_await_timeout_returns_result` | passes a **null** `TimeUnit` and expects `false` |
| `enum_map_put_get_size` | passes a **null** `Class` to `EnumMap(Class)` and expects it to construct |
| `http_version_enums_p60` | addresses an enum CONSTANT as a zero-arg native with a field descriptor |
| `http_redirect_enums_p60` | same |

The first two were verified against HotSpot 25.0.3, which throws the **identical
NPE, message and all**:

```text
Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "unit" is null
Cannot invoke "java.lang.Class.getEnumConstantsShared()" because "klass" is null
```

So CratonVM's current behaviour is correct and the assertions are not. They
passed only while synthetic natives ignored the arguments the real API
dereferences. All four are `#[ignore]`d.

**A cluster is a hypothesis.** Four tests failing with enum-shaped messages is
not four instances of one defect; here it was four instances of "this test
predates a stricter implementation". The grouping cost nothing to make and would
have cost real time to act on.

## So the real question was never asked

None of those tests exercised an enum, so the honest state of enum support in
`--synthetic-jdk` was still unknown. `apps/probes/SyntheticEnumSurface` asks it
directly: 20 rows over a user-defined enum and four JDK enums — `values`,
`valueOf`, `name`/`ordinal`, `compareTo`, `switch`, `getDeclaringClass`,
`isEnum`, `getEnumConstants`, `EnumMap`, `EnumSet`, and JDK constants.

```text
real-JDK           20 / 20 identical to HotSpot 25.0.3
--synthetic-jdk     8 differ   ->  4 after this change
```

## Three general defects, fixed

**1. `Enum.valueOf` was an `UnsatisfiedLinkError` for EVERY enum**, including
user-defined ones, on a three-constant local enum whose `values()`, `name()`,
`ordinal()` and `getEnumConstants()` all worked.

`java/lang/Enum`'s five natives were retired 2026-08-23 (`80d60e911`), and that
retirement was right *as measured*: it ran against the real-JDK regression
suite, where the real bytecode serves every call and a native only shadows it.
`--synthetic-jdk` has no bytecode, and the retirement applied to both modes.

**This is the third time in one session that shape has appeared**, after
`ArrayDeque.iterator()` and `Collections.unmodifiableSortedMap`: *a native
retired because real bytecode covers it is retired in BOTH modes, and only one
of them has the bytecode.* The fix is `#[cfg(feature = "synthetic-jdk")]`, so
real-JDK keeps the retirement exactly as measured. It reuses
`native_class_get_enum_constants`' `$VALUES` path rather than a second constant
source, so the two cannot disagree, and reproduces HotSpot's
`No enum constant <Class>.<NAME>` wording.

**2. `EnumMap.put` was registered under the wrong descriptor.**
`EnumMap<K extends Enum<K>, V>` erases `K` to `java/lang/Enum`, not to
`java/lang/Object`, so `javap -s` gives
`put(Ljava/lang/Enum;Ljava/lang/Object;)Ljava/lang/Object;` and the registered
`(Object, Object)` row never matched a real call site —
`NoSuchMethodError: java.util.EnumMap.put(java.lang.Enum, java.lang.Object)`.
`get`/`remove`/`containsKey` take `Object` in the source, so their erasure is
the one already registered; only `put` diverges, which is why the gap looked
like "EnumMap is broken" rather than "one descriptor is wrong". Both rows are
kept: the `Object` form is what the compiler-generated `Map.put` bridge emits.

**3. `EnumSet.toString` was unregistered**, so `EnumSet.of(ALPHA, GAMMA)`
printed `java.util.EnumSet@9` where HotSpot prints `[ALPHA, GAMMA]`. Real-JDK
runs `AbstractCollection.toString()`; synthetic mode reached `Object.toString`.

## What remains: 4 rows, one cause, and it is not a mechanism

```text
HttpClient.Version.HTTP_1_1   NoSuchFieldError
HttpClient.Redirect.NEVER     NoSuchFieldError
DayOfWeek.MONDAY              NoSuchFieldError
EnumSet.allOf(TimeUnit.class) 0 where HotSpot gives 7
```

A synthetic JDK enum needs an explicit entry in `class_manager`'s field table
declaring its constants as statics, **plus** a native `<clinit>` to populate
them — the shape `PosixFilePermission` and `TimeUnit` already have, which is
why `TimeUnit.MILLISECONDS.name()` and `TimeUnit.values()` are correct while
`DayOfWeek.MONDAY` is not.

There is no general fix available: synthetic mode has no class file to read the
constants from, so every JDK enum it supports is hand-modelled by construction.
Adding these three is mechanical and is whack-a-mole by nature — a table entry
plus a `<clinit>` each — and declaring the fields WITHOUT the `<clinit>` would
be worse than the current error, because `GETSTATIC` would then resolve to a
null constant, which is exactly the shape the two stale tests above hit.

`EnumSet.allOf` on a JDK enum is the same cause one level along: it reads
`$VALUES`, and the hand-written tables declare the constants but no `$VALUES`
array.
