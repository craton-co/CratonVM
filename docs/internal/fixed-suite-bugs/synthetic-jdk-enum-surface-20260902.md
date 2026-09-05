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

## The last 4 rows: closed, and the cause was not what the enum names said

The four were `DayOfWeek.MONDAY`, `HttpClient$Version.HTTP_1_1`,
`HttpClient$Redirect.NEVER` and `EnumSet.allOf(TimeUnit.class)`. The obvious
reading — "three JDK enums are not modelled, and that is inherent to a mode with
no class files" — was right about the first three and **wrong about why the
fourth failed**, and the fourth is the interesting one.

### Three enums, modelled

`class_manager.rs` now declares their constants (through a shared
`enum_constant_fields` builder, in `javap` declaration order because THAT IS THE
ORDINAL), `native-builtins` publishes them through a single generic
`publish_synthetic_enum_constants`, and each gets a `java/lang/Enum` superclass
row.

That superclass row is not paperwork. Without it the constants publish fine and
`HttpClient$Version.HTTP_1_1.name()` is a `NoSuchMethodError`, because
`Enum.name()`/`ordinal()`/`compareTo()` are registered on `java/lang/Enum` —
which is exactly what `posix_publish_constants`' doc predicted: *"one missing
superclass row … and every constant is NAMELESS"*.

One generic publisher rather than three copies, for the reason that same doc
gives: it is itself the survivor of a copy whose `name`/`ordinal` fallbacks came
out INVERTED, latent only because a superclass row happened to exist.

### `EnumSet.allOf` was never about those enums

`TimeUnit` was already fully modelled — table, `<clinit>`, working `name()`,
`ordinal()`, `toNanos()`, `values()` — and `EnumSet.allOf(TimeUnit.class)` still
answered 0. Two causes stacked, and neither is visible from an enum's own
behaviour:

1. **No synthetic JDK enum has ever carried `ACC_ENUM`.**
   `class_is_declared_enum` requires the flag AND a `java/lang/Enum` parent, and
   it gates `Class.getEnumConstants`, which `EnumSet.allOf` reads through. So:

   ```text
   TimeUnit             isEnum=false getEnumConstants=null allOf=0
   DayOfWeek            isEnum=false getEnumConstants=null allOf=0
   PosixFilePermission  isEnum=false getEnumConstants=null allOf=0   <- the MODEL
   a user-defined enum  isEnum=true  getEnumConstants=2    allOf=2
   ```

   `PosixFilePermission` has been the template every other synthetic enum was
   copied from, and it was in this state the whole time. `create_synthetic_stub`
   now derives `ACC_ENUM` from the superclass row — one fact, one place, no
   second list of enum names — and it has to be tested BEFORE the `$`/`able`
   heuristics, or `HttpClient$Version` is fabricated as an INTERFACE.

2. **`$VALUES` was declared for no synthetic enum at all.**
   `set_static_field_by_name` resolves a DECLARED static and is a silent no-op
   otherwise, so `<clinit>`s that published `$VALUES` were writing nowhere.
   `posix_publish_constants` says so about itself — *"the declaration is
   nominated; the publish is written now so it starts working the moment that
   lands"* — and this lands it, for `PosixFilePermission` and `TimeUnit` as well
   as the three new enums.

Neither was findable from the four failing rows. **`ACC_ENUM` was invisible
because nothing asked**: an enum answers `name()`, `ordinal()`, `values()`,
`valueOf` and `switch` correctly without it, and only `isEnum()` /
`getEnumConstants()` / `EnumSet` ever look. Finding it took a diagnostic that
printed `isEnum` and `getEnumConstants` beside the answer, rather than the
answer alone — and the tell was the CONTROL row: a user-defined enum passing
every column that the JDK enums failed.

## Result

```text
real-JDK           20 / 20 identical to HotSpot 25.0.3   (unchanged)
--synthetic-jdk    20 / 20 identical                     (was 8 differing)
```

`PosixFilePermission` goes from `getEnumConstants=null` to 9 as a side effect,
and `apps/probes/CollectionViewTypes`' `EnumSet` row stops being a
`NoSuchFieldError`.

Two guesses were made and measured wrong on the way here, both corrected by
instrumenting instead of reasoning: `ACC_ENUM` was first added to
`synthetic_stub_access_flags`, which is never called for these classes (a
temporary `eprintln` proved it — the branch never fired), and `TimeUnit`'s
missing superclass row was assumed absent-but-harmless until `isEnum` was
printed.
