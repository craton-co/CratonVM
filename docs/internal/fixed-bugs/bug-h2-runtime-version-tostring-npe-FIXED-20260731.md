# `Runtime.version()`'s lightweight `Runtime$Version` never populated its fields — FIXED 2026-07-31

## Status
**FIXED** — `fix/runtime-version-lightweight-20260731`, merged to `dev`.
Retired here from `docs/known-issues/h2/`. The original report is reproduced
verbatim at the bottom.

The report's root cause was correct as far as it went, and two further
defects of the same family turned up while verifying it. All three are fixed.

## What was wrong

### 1. `Runtime.version()` returned an object with all four fields `null`
`native_runtime_version` allocated a bare real-layout `Runtime$Version` and
returned it without writing `version` / `pre` / `build` / `optional`. Every
real-bytecode accessor then NPE'd or returned `null`:

| accessor | before | HotSpot |
|---|---|---|
| `toString()` | NPE `"this.version" is null` | `25.0.3+9-LTS` |
| `version()` | `null` | `[25, 0, 3]` |
| `interim()` / `update()` / `patch()` | NPE | `0` / `3` / `0` |
| `major()` / `minor()` / `security()` | NPE | `25` / `0` / `3` |
| `pre()` / `optional()` | `null` | `Optional.empty` |
| `equals` / `hashCode` / `compareTo` | NPE | work |

### 2. `Runtime.Version.build()` corrupted *correctly parsed* versions too
The `build()` native override is forced for **every** `Runtime$Version`
receiver, including the fully populated objects `Runtime.Version.parse(String)`
produces — and it returned a blanket `Optional.empty()`. So
`Runtime.Version.parse("25.0.1+9").build()` was `Optional.empty`, not
`Optional[9]`.

### 3. …which silently broke version comparison (worst of the three)
`Runtime.Version.compareTo` / `equalsIgnoreOptional` read the `build` **field**
on the receiver but the `build()` **accessor** on the argument. With (2) in
place the two disagreed, so a parsed version did not even compare equal to
itself, and two *different* versions compared *equal*:

```
                                      before      HotSpot
v.compareTo(v)  where v = parse("25.0.1+9")   1          0
parse("25.0.1+9").equals(parse("25.0.1+10")) true      false
parse("17.0.2-ea+7-abc").compareToSelf        1          0
```
Any version gate written as `a.compareTo(b)` or `a.equals(b)` got a wrong
answer, with no exception to notice.

## Fix
`native-builtins/src/lang_system.rs`

- `parse_runtime_version_str` hand-parses the JDK's
  `$VNUM(-$PRE)?(\+$BUILD)?(-$OPT)?` grammar without the regex engine —
  running `Runtime.Version.parse` is exactly what the native override exists to
  avoid (interpreted-mode regex blocks signed-jar opening before loader work
  begins), and the grammar is small enough to split by hand. Unit-tested
  against real-JDK-25 expectations in `runtime_version_parse_tests`.
- `native_runtime_version` populates all four fields from the VM's reported
  version string (`java.runtime.version`, falling back through
  `java.vm.version` / `java.version` / `java.specification.version`).
  Population is best-effort: during very early bootstrap `List`/`Optional` may
  not be usable, and a bare object — the historical behaviour — beats a failing
  `Runtime.version()`.
- The result is memoised for the process, as HotSpot's own `Runtime.version()`
  memoises into `private static Runtime.version`. Restores the
  `Runtime.version() == Runtime.version()` identity, and makes the call
  *cheaper* than before (249 ms vs 277 ms for 200 000 calls) rather than 14 µs
  each. The memo lives in the JNI global-ref table, **not** in the JDK static —
  `ctx.set_static_field` into a real JDK class silently does not stick.
- `native_runtime_version_build` reads the real `build` field and only falls
  back to `Optional.empty()` when the receiver genuinely has none
  (synthetic-JDK mode). Fixes (2) and therefore (3).
- `runtime_feature_version` factors out `feature()`'s system-property fallback.

`native-builtins/src/phases_late/jar_manifest.rs`

- `p59_jar_lookup_versioned_entry` asked for the feature version by building a
  whole `Runtime$Version` and calling `feature()` on it — per jar-entry lookup.
  It now reads the system property directly. Multi-release entry selection is
  byte-identical before and after (verified below).

## Verification
Host: Azure Linux box, `--java-home /home/victor/jdk25` (Temurin 25.0.3), `--nojit`.

- **Differential probe** (`RuntimeVersionProbe`, 60 assertions across
  `Runtime.version()` and three `parse()` shapes): every structural difference
  vs real HotSpot is gone. The only remaining differences are the version
  numbers themselves — CratonVM reports `25.0.1+8`, Temurin `25.0.3+9-LTS` —
  which is correct behaviour.
- **Comparison/equality probe** (`P3`): all five `equals`/`compareTo`/
  `hashCode`/`equalsIgnoreOptional` cases now match HotSpot exactly.
- **`major()`/`minor()`/`security()`**: NPE → `25`/`0`/`1`.
- **Baseline repeated twice**, both times failing identically, before
  attributing anything.
- **H2**: `org.h2.test.db.TestFullText` and `org.h2.test.unit.TestRecovery` no
  longer throw `ExceptionInInitializerError`. Lucene's `Constants.<clinit>`
  completes (it now logs its own `"You are running with Java 22 or later"`
  message, i.e. it successfully read the version), the FULLTEXT_LUCENE trigger
  is created, and the tests run ~60s of real work past the old failure point.
  **They do not pass yet** — see "Remaining blocker".
- **No regression in jar handling**: a 7-jar / 12 642-entry scan of
  `getName`/`getRealName`/`getSize` hashes identically before and after, and a
  purpose-built multi-release jar still selects `../../../apps/META-INF/versions/17/`.
- `cargo test -p cratonvm-native-builtins`: 3144 passed, 0 failed.

## Remaining blocker (a different bug, now filed separately)
With this fixed, the two H2 classes get far enough to hit
`docs/known-issues/h2/bug-filechannel-map-anon-memory-growth.md`:
`FileChannel.map` retains its whole mapping, anonymous RSS grows past `-Xmx`,
and the kernel SIGKILLs the process. That defect reproduces on unmodified
`dev` with a 14-line probe that never touches `Runtime.Version`, and is
unaffected by this fix (identical peak RSS with and without it) — it is the
next bug in the queue for these two tests, not a residual of this one.

---

# Original report

<details>
<summary>`docs/known-issues/h2/bug-h2-runtime-version-tostring-npe-lightweight-version-object.md` as filed 2026-07-30/31</summary>

## Severity
**LOW-MEDIUM** — narrow trigger (any code that calls `.toString()` — or any
other real-bytecode method that reads `this.version` directly — on the
`Runtime.Version` object returned by `Runtime.version()`), but the trigger
is common: Lucene's own `org.apache.lucene.util.Constants` static
initializer does exactly this, so anything that touches Lucene (H2's
`FULLTEXT`/`FULLTEXT_LUCENE` functions) hits it deterministically the first
time that class is touched in a process.

## Affected test classes (H2 suite)
- `org.h2.test.db.TestFullText` (`testCreateDropLucene`)
- `org.h2.test.unit.TestRecovery` (`testRecoverFulltext`)

Both fail identically: creating the `FULLTEXT_LUCENE` trigger throws
`ExceptionInInitializerError` while Lucene's `Constants` class initializes.
Real HotSpot JDK25 passes both (the same code touches `Runtime.version()`
there and gets a fully-populated, real `Runtime.Version` instance).

## Symptom
```
org.h2.jdbc.JdbcSQLSyntaxErrorException: Error creating or initializing
  trigger "FTL_TEST" object, class "org.h2.fulltext.FullTextLucene$FullTextTrigger",
  cause: "java.lang.ExceptionInInitializerError"; see root cause for details
	at org/h2/fulltext/FullTextLucene.createTrigger(FullTextLucene.java:286)
	...
Caused by: java/lang/NullPointerException: Cannot invoke "java.util.List.stream()"
  because "this.version" is null
	at java/lang/Runtime$Version.toString(Runtime.java:1383)
	at org/apache/lucene/util/Constants.<clinit>(Constants.java:34)
	at org/apache/lucene/store/FSDirectory.open(FSDirectory.java:156)
```

## Root cause
`native-builtins/src/lang_system.rs`'s `native_runtime_version` (the native
override for `Runtime.version()`) deliberately does **not** call the real
JDK's `Runtime.Version` parser (its own doc comment explains why: routing
every call through the regex-based parser is prohibitively slow in
interpreted real-JDK mode and blocks signed-jar opening before loader work
even begins). Instead it allocates a bare, lightweight object of the real
class shape via `alloc_concurrent_synthetic(ctx, "java/lang/Runtime$Version", 4)`
and returns it immediately — **the real `version` field (a
`List<Integer>`, the parsed dot-separated version-number sequence) is never
set; it stays `null`.**

Two companion natives already know about and work around this gap:
- `native_runtime_version_feature` (`Runtime.Version.feature()`) explicitly
  checks `if let Value::Object(Some(parts)) = ctx.get_field_by_name(this, "version")`
  and falls back to a `java.specification.version`-derived value when it's
  absent.
- `native_runtime_version_build` (`Runtime.Version.build()`) is entirely
  overridden to return `Optional.empty()` specifically because "CratonVM's
  lightweight `Runtime.version()` object does not populate the real JDK
  `build` field."

But `Runtime.Version.toString()` (and, by the same mechanism, `compareTo()`,
`equals()`, `hashCode()`, `major()`, `minor()`, `security()`, etc.) are
**not** natively intercepted — they run the real JDK bytecode, which
unconditionally reads `this.version` and calls `.stream()`/`.get(int)` on
it. Since CratonVM's lightweight object never set that field, any of these
real methods NPEs the moment they're invoked on a `Runtime.version()`
result.

## Suggested fix
Populate `this.version` in `native_runtime_version` at construction time
with a genuine, minimal `List<Integer>` (e.g. `List.of(<feature-version>)`,
matching what `native_runtime_version_feature`'s own fallback already
computes from `java.specification.version`) via `set_field_by_name`, the
same accessor the two companion natives already use for this exact field.

## Related
- Neither `feature()` nor `build()`'s existing defensive handling is itself
  broken — they're proof the gap was known for *some* accessors on this
  object; `toString()` (and the other real-bytecode-only accessors) simply
  never got the same treatment.

</details>

*(The report's "Related" note turned out to be half right: `feature()`'s
handling was fine, but `build()`'s was itself broken — see defects 2 and 3
above.)*
