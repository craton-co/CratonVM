# `String.chars()` wrong array kind + `Properties` `\f` escape gap — FIXED

Found and fixed 2026-07-24 investigating Spring Boot core39 residual Cluster A
(`docs/known-issues/spring-boot-core39-residual-clusters-20260723.md`).
Worktree `springboot-core39-clusterA-20260723`.

## Bug 1 — `Properties` `.properties`-file loader never decoded `\f`

`native-builtins/src/properties_sidetable.rs`'s `unescape_inner` (the escape
decoder shared by every `.properties`-file/stream load path) handled `\n`,
`\t`, `\r`, `\\`, `\"`, `\'`, `\<space>`, `\:`, `\=`, and `\uXXXX` but had no
`'f'` arm — an unknown escape "degrades to literal characters" per the
function's own doc comment, so `\f` silently became the literal letter `f`
instead of a form-feed (`0x0C`). Confirmed with
`OriginTrackedPropertiesLoaderTests.compareToJavaProperties`, which loads the
same fixture through both real `java.util.Properties.load` (our native
`Properties` bridge) and Spring's own hand-rolled
`OriginTrackedPropertiesLoader` and asserts the two agree — they disagreed on
exactly `test-form-feed-property`: expected `"foo\fbar"`, our
`Properties.load` produced `"foofbar"` (confirmed at the byte level with
`od -c`, ruling out a terminal-rendering artifact).

Fix: added `Some('f') => out.push('\u{000c}')` to the escape match.

## Bug 2 — `String.chars()`/`codePoints()` allocated a reference array, not `int[]`

`native-builtins/src/lang_string.rs`'s `native_string_chars` — the winning
registration for `java/lang/String.chars()Ljava/util/stream/IntStream;` (a
duplicate, correct implementation also exists in `phases_early.rs`; this one
wins registration order and is the one actually invoked) — built its backing
array via:

```rust
let arr = ctx.new_ref_array(ClassId::new(0), char_values.len());
```

`new_ref_array` allocates a *reference*-element array. The loop then stored
`Value::Int` values into that Object-shaped array via `set_array_element`.
Every real consumer of the resulting `IntStream` (`forEach`, `toArray`,
`filter`, `map`, …) reads field 0 back as an `int[]` — an Object-shaped slot
holding a raw `Value::Int` reads back as `0`, so the stream was correctly
*sized* to the string's length but every element was `0`.

Confirmed with a standalone repro (no Spring Boot involved):

```java
"foo-bar".chars().toArray()  // before: [0, 0, 0, 0, 0, 0, 0]
                              // after:  [102, 111, 111, 45, 98, 97, 114]
```

This is a foundational, broadly-used JDK API bug (any character-by-character
`Stream` pipeline over a `String`), not specific to Spring Boot or Cluster A.
It happened to surface in this investigation via Spring Boot's
`LenientObjectToEnumConverterFactory.getCanonicalName()`:

```java
name.chars().filter(Character::isLetterOrDigit).map(Character::toLowerCase)
    .forEach((c) -> canonicalName.append((char) c));
```

With every `c` silently `0`, `canonicalName` was always the empty string for
every input, so `findEnum()`'s `name.equals(candidateName)` always compared
`"" .equals("")` — true for whichever enum constant happened to be checked
*first* in iteration order, regardless of the real input. This surfaced as
`MapBinderTests.bindToMapShouldBeGreedyForScalars` /
`bindToMapWithPlaceholdersShouldBeGreedyForScalars` (both `--nojit`
residuals): every non-exact-match enum value bound to the same wrong
constant (`FOO_BAR`, the first `ExampleEnum` constant) no matter what the
actual source string was.

Fix: replaced the allocation with `ctx.new_array(ArrayElementType::Int,
char_values.len())`, matching the correct sibling implementation in
`phases_early.rs` and every other `IntStream` factory in the codebase.

## Verification

- Standalone repro (`Repro.java`/`Repro2.java`/`Repro3.java`, not checked in
  — scratch files) confirmed both the raw `chars()` corruption and the
  `EnumSet`/canonical-name matching fix.
- `OriginTrackedPropertiesLoaderTests`: FAIL (1/40) → PASS (40/40), JIT and
  `--nojit`.
- `MapBinderTests`: PASS (45/45) JIT (already passing before this fix, via
  the prior session's `Properties.computeIfAbsent` bridge) → PASS (47/47)
  `--nojit` (previously FAIL 2/47 on exactly the two `...ShouldBeGreedyForScalars`
  tests above).
- Green controls (`BeanDefinitionLoaderTests`,
  `ApplicationPidFileWriterTests`,
  `ConfigDataEnvironmentPostProcessorIntegrationTests`,
  `ConfigTreeConfigDataLocationResolverTests`,
  `JakartaApiValidationExceptionFailureAnalyzerTests`,
  `NoSnakeYamlPropertySourceLoaderTests`) all still PASS after both fixes —
  no regression from the `chars()` change despite its broad blast radius.

## Not fixed by this change

`ConfigurationPropertySourcesTests` and
`ConfigurationPropertiesBeanRegistrationAotProcessorTests` were also
originally reported as HANGs in the same cluster. Rebuilding with this fix
and re-running both showed no change in stack-dump location. Follow-up with
much longer timeouts resolved the picture for both:

- `ConfigurationPropertySourcesTests` is CPU-bound, not a deadlock — a
  scaled-down repro proved linear (not quadratic) per-iteration cost, and a
  full standalone rerun completed (PASS) in 2645.0s. Timeout-tuned in
  `run-spring-boot-suite.ps1`.
- `ConfigurationPropertiesBeanRegistrationAotProcessorTests` is a genuine,
  still-open hang — it never completed even at a 7200s (2-hour) ceiling.
  See `docs/known-issues/springboot/configurationpropertiesbeanregistrationaotprocessortests-hang.md`
  for the full diagnostic trail; not fixed by this change or anything else
  found in this investigation.
