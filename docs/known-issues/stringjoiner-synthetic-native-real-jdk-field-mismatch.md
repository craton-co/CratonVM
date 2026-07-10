# `java.util.StringJoiner`'s `SyntheticStub` native silently produces wrong content in real-JDK mode (pre-existing, not caused by any recent commit)

Status: OPEN — confirmed pre-existing, not fixed (out of scope for the crash-blocking regression it was found alongside)

Found: 2026-07-10, while fixing
`docs/internal/elasticsearch-suite/ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce-FIXED.md`
(a `ClassCastException` that blocked the whole ES suite, caused by dev commit `aa21e334` making
`StringJoiner` yield to real bytecode for the first time). That fix reverts `StringJoiner` to
always use its `SyntheticStub` native (`native-collections/src/lib.rs`,
`register_string_joiner_stub_natives`) — the behavior that existed on **every** dev commit before
`aa21e334` too. While confirming that revert didn't regress anything, this separate,
already-broken bug was found (and confirmed, via a rebuild at the exact pre-`aa21e334` commit, to
have been *already present* — this doc is not a consequence of that fix).

## Symptom

```java
StringJoiner sj = new StringJoiner(" ");
sj.add("public");
System.out.println(sj.toString());  // prints "" (empty), not "public"
```

No exception, no hang — just silently wrong output. `java.lang.reflect.Modifier.toString(int)`
(which builds its result via `new StringJoiner(" ")` + repeated `.add(...)`) is affected the same
way: it always returns `""` in real-JDK mode, regardless of the modifiers passed in.

## Root cause

`native_sj_init_delim`/`native_sj_init_full`/`native_sj_add`/`native_sj_to_string` (and siblings)
in `native-collections/src/lib.rs` implement `StringJoiner` using a legacy 5-field layout
(`delimiter`=0, `prefix`=1, `suffix`=2, an internal `ArrayList` of elements=3, `emptyValue`=4) that
predates the real JDK's actual `StringJoiner` field layout. In real-JDK mode, `alloc_synthetic`
allocates the object using the **real** loaded class's actual field count/layout (7 fields:
`prefix`=0, `delimiter`=1, `suffix`=2, `elts: String[]`=3, `size`=4, `len`=5, `emptyValue`=6) — the
same "real-shaped object, synthetic-index writes" pattern documented for `ThreadPoolExecutor`/
`Thread` in the executors-factory mainlock-NPE fix (same session, different doc). Confirmed via
reflection-based field dumps on a freshly-constructed `new StringJoiner(" ")`:

```text
prefix    = String( )      <- should be delimiter
delimiter = null           <- should be " "
suffix    = null           <- coincidentally right slot, wrong null-vs-"" convention
elts      = ArrayList([])  <- should be String[] or null; native's "elements" landed here
size      = Integer(0)     <- coerced from the native's stray write, coincidentally correct
len       = Integer(0)     <- never touched by the native at all
emptyValue = null          <- never touched by the native at all
```

Additionally, `native_sj_add`'s own bookkeeping breaks silently: it stores elements in an internal
`java.util.ArrayList` helper object (`alloc_synthetic(ctx, "java/util/ArrayList", 2)`), and if
`ArrayList` is ALSO loaded as a real bootstrap class in this mode, that helper object is likely
subject to the identical "real-shaped object with synthetic-index writes" mismatch one level down
— consistent with `add()` silently no-op'ing (the added element never actually appears in the
joiner's output) rather than throwing. Not fully traced to a specific `ArrayList` field collision in
this session.

This is **not** something any recent commit introduced — confirmed identical (byte-for-byte same
reflection dump) on a rebuild at dev commit `4b08ffad`, well before `aa21e334` ever landed. It has
presumably been silently wrong for as long as `StringJoiner`/`ArrayList` have had both a
`SyntheticStub` native AND been loadable as real bootstrap classes side by side. It went unnoticed
because nothing exercises `StringJoiner`'s actual joined *content* in a way that fails a test
assertion — `RandomizedRunner`'s use of `Modifier.toString()` only needs *a* string for sorting a
field list, not the *correct* one.

## Suggested fix direction (not attempted this session)

Same pattern as the `ThreadPoolExecutor`/`Thread`/`ScheduledThreadPoolExecutor` fixes: make
`StringJoiner`'s `<init>` natives drive the real `StringJoiner(CharSequence, CharSequence,
CharSequence)V` constructor via `invoke_special`/`new_object_initialized` instead of writing
legacy-indexed slots, and either drop `add`/`toString`/etc.'s native overrides entirely (letting
real bytecode run against a now-genuinely-real object) or rewrite them to operate on the real
field layout. Given `add()`'s real bytecode is exactly what exposed the **separate** heap-corruption
defect this doc's companion fix worked around (see the companion doc), pursuing "make it real"
here would need that corruption's root cause found and fixed first, or the same class of
regression would resurface immediately.

## Severity

Silent, non-crashing content bug. Low urgency (nothing currently observed to assert on
`StringJoiner`'s actual joined output on CratonVM), but worth fixing eventually since any test or
production code that *does* check `StringJoiner`/`Modifier.toString()`/similar content would fail
silently rather than loudly.
