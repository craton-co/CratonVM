# `java.util.StringJoiner`'s `SyntheticStub` native silently produces wrong content in real-JDK mode (pre-existing, not caused by any recent commit)

Status: FIXED. **Retired from `docs/known-issues/` to `docs/internal/fixed-suite-bugs/`**
per this repo's convention that `known-issues/` holds only open items.

Found: 2026-07-10, while fixing
`docs/internal/elasticsearch-suite/ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce-FIXED.md`
(a `ClassCastException` that blocked the whole ES suite, caused by dev commit `aa21e334` making
`StringJoiner` yield to real bytecode for the first time). That fix reverts `StringJoiner` to
always use its `SyntheticStub` native (`native-collections/src/lib.rs`,
`register_string_joiner_stub_natives`) — the behavior that existed on **every** dev commit before
`aa21e334` too. While confirming that revert didn't regress anything, this separate,
already-broken bug was found (and confirmed, via a rebuild at the exact pre-`aa21e334` commit, to
have been *already present* — this doc is not a consequence of that fix).

## Symptom (before fix)

```java
StringJoiner sj = new StringJoiner(" ");
sj.add("public");
System.out.println(sj.toString());  // printed "" (empty), not "public"
```

No exception, no hang — just silently wrong output. `java.lang.reflect.Modifier.toString(int)`
(which builds its result via `new StringJoiner(" ")` + repeated `.add(...)`) was affected the same
way: it always returned `""` in real-JDK mode, regardless of the modifiers passed in.

## Root cause

`native_sj_init_delim`/`native_sj_init_full`/`native_sj_add`/`native_sj_to_string` (and siblings)
in `native-collections/src/lib.rs` implemented `StringJoiner` using a legacy 5-field layout
(`delimiter`=0, `prefix`=1, `suffix`=2, an internal `ArrayList` of elements=3, `emptyValue`=4) that
predates the real JDK's actual `StringJoiner` field layout. In real-JDK mode, `alloc_synthetic`
allocates the object using the **real** loaded class's actual field count/layout (7 fields:
`prefix`=0, `delimiter`=1, `suffix`=2, `elts: String[]`=3, `size`=4, `len`=5, `emptyValue`=6) — the
same "real-shaped object, synthetic-index writes" pattern documented for `ThreadPoolExecutor`/
`Thread` in the executors-factory mainlock-NPE fix. Confirmed via reflection-based field dumps on a
freshly-constructed `new StringJoiner(" ")`:

```text
prefix    = String( )      <- should be delimiter
delimiter = null           <- should be " "
suffix    = null           <- coincidentally right slot, wrong null-vs-"" convention
elts      = ArrayList([])  <- should be String[] or null; native's "elements" landed here
size      = Integer(0)     <- coerced from the native's stray write, coincidentally correct
len       = Integer(0)     <- never touched by the native at all
emptyValue = null          <- never touched by the native at all
```

This is **not** something any recent commit introduced — confirmed identical (byte-for-byte same
reflection dump) on a rebuild at dev commit `4b08ffad`, well before `aa21e334` ever landed. It had
presumably been silently wrong for as long as `StringJoiner`/`ArrayList` had had both a
`SyntheticStub` native AND been loadable as real bootstrap classes side by side. It went unnoticed
because nothing exercised `StringJoiner`'s actual joined *content* in a way that failed a test
assertion — `RandomizedRunner`'s use of `Modifier.toString()` only needed *a* string for sorting a
field list, not the *correct* one.

## Fix (2026-07-11)

`native-collections/src/lib.rs`: added `sj_real_layout(ctx)`, which resolves
`java/util/StringJoiner`'s real field indices by name (`ctx.resolve_field_index("java/util/StringJoiner",
"prefix" | "delimiter" | "suffix" | "elts" | "size" | "len" | "emptyValue")`), returning `None`
when only the synthetic 5-field fallback class is loaded (its fields are unnamed `_f0.._f4`, per
`classloading::class_manager::synthetic_stub_fields`). Every `native_sj_*` entry point now branches
on this: when `Some`, it operates on the real object's actual named fields (with `elts` correctly
typed/grown as a `String[]`, matching real `StringJoiner.add()`'s doubling-capacity growth policy);
when `None`, it falls back to the original legacy 5-field/ArrayList-of-elements code, byte-for-byte
unchanged, so synthetic-JDK-mode behavior is untouched.

This is intentionally still a full from-scratch Rust reimplementation of StringJoiner's behavior
(construction, `add`, `toString`, `length`, `merge`, `setEmptyValue`) — NOT a yield to real
bytecode — even when it targets the real object's fields: running the real `add()` bytecode against
a real `StringJoiner` is exactly what exposes the separate, still-unrelated
heap-reference-integrity defect that the `aa21e334` CCE fix (see above) worked around by excluding
`StringJoiner` from `interpreter.rs`'s `synthetic_stub_should_yield_to_real_bytecode` allowlist.
This native must keep owning execution either way, or that defect resurfaces immediately.

GC safety: every code path re-fetches any array/object reference it needs from a rooted field
(`this`'s own fields) immediately after any allocation, rather than holding a raw `ObjectRef` across
a later `alloc_ref_array`/`create_string` call — `ObjectRef` is a bare pointer (see
`types/src/value.rs`), so a moving GC triggered by a later allocation could otherwise relocate it
out from under a stale local copy.

## Verification

Wrote `SJProbe.java` exercising: basic `add`+`toString`+`length` (3 elements), the 3-arg
`(delimiter, prefix, suffix)` constructor, the no-add empty case, `setEmptyValue` with no adds,
`merge()` (two joiners, confirming the merged joiner's *own* delimiter is used and its prefix/suffix
are dropped), a 20-element growth case (forces the initial capacity-8 array to grow, confirms
count/order survive), and a full reflection dump of `prefix`/`delimiter`/`suffix`/`elts`/`size`/
`len`/`emptyValue` on the basic case. Ran the identical class under real HotSpot (JDK 25,
`--add-opens java.base/java.util=ALL-UNNAMED`) and under the fixed CratonVM binary
(`--java-home /home/victor/jdk25`): every line of output matched byte-for-byte (the only difference
was the default `Object.hashCode()`-derived suffix on a `System$1` identity toString, which is never
expected to match across JVM instances).

Also ran `cargo test --release -p cratonvm-native-collections`: 132 tests across the crate's unit
and mock-integration suites, 0 failures (no regressions from the legacy synthetic-mode fallback
path, which is byte-for-byte unchanged).

Collection/fix context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/wt-stringjoiner-content-20260711`, branch
  `fix/stringjoiner-content-real-layout-20260711`
- Probe: `/tmp/sjprobe/SJProbe.java` on the collection host
