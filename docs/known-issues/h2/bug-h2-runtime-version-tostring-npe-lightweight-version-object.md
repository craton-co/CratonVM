# `Runtime.version().toString()` NPEs — CratonVM's lightweight `Runtime$Version` never populates the real `version` field

## Status
**OPEN** — root-caused via direct code inspection (native-builtins source +
observed failures), not yet fixed. Found while investigating H2 suite
regressions in the twelfth-pass `TestUpgrade` session's follow-up full-suite
run (2026-07-30/31).

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

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestFullText
```
Minimal, H2-independent repro (not yet written/verified this session, but
should reproduce trivially): `System.out.println(Runtime.version());` in any
class run under CratonVM real-JDK mode.

## Suggested fix
Populate `this.version` in `native_runtime_version` at construction time
with a genuine, minimal `List<Integer>` (e.g. `List.of(<feature-version>)`,
matching what `native_runtime_version_feature`'s own fallback already
computes from `java.specification.version`) via `set_field_by_name`, the
same accessor the two companion natives already use for this exact field.
That alone would make `toString()`/`compareTo()`/etc. behave sanely without
needing to intercept each of them individually (they'd read a real,
single-element list instead of `null`). Alternatively (more surgical, less
central): add a native override for `toString()` mirroring the existing
`feature()`/`build()` pattern.

## Related
- Neither `feature()` nor `build()`'s existing defensive handling is itself
  broken — they're proof the gap was known for *some* accessors on this
  object; `toString()` (and the other real-bytecode-only accessors) simply
  never got the same treatment.
