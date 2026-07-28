# `OriginTrackedPropertiesLoaderTests` form-feed mismatch — FIXED

Closed 2026-07-28. The original 2026-07-23 known-issue report incorrectly
described a literal form-feed byte being dropped by Spring Boot's parser. The
fixture actually contains the Java `.properties` escape `\f`; Spring Boot's
hand-written `OriginTrackedPropertiesLoader` correctly decodes it, while
CratonVM's native `java.util.Properties.load` path did not.

## Root cause and repair

`native-builtins/src/properties_sidetable.rs::unescape_inner`, the shared
decoder for `.properties` file/stream loads, had arms for `\n`, `\t`, and
`\r`, but no `f` arm. Consequently `\f` degraded to the literal `f`, yielding
`"foofbar"` instead of `"foo\fbar"`; the Spring comparison test reported the
only mismatch as `test-form-feed-property`.

Commit `6ea3b7f30` repaired the decoder with
`Some('f') => out.push('\u{000c}')`. This closure adds a direct native unit
assertion for that escape, preventing the exact omission from recurring.

The same earlier investigation also found an independent `String.chars()`
primitive-array bug. Its fix and broader context remain recorded in
`string-chars-wrong-array-kind-and-properties-formfeed-escape-FIXED.md`.

## Fresh closure verification

On current `dev` ancestry (`075fdfc54`), built an isolated release binary
`cratonvm-springboot-properties-formfeed-20260728.exe` and ran the exact real
Spring Boot fixture through `apps/spring-boot-suite-runner` with JDK 25:

| Mode | Class | Result |
|---|---|---|
| JIT | `org.springframework.boot.env.OriginTrackedPropertiesLoaderTests` | PASS — 40 started, 40 successful, 0 failed (9.304s) |
| `--nojit` | `org.springframework.boot.env.OriginTrackedPropertiesLoaderTests` | PASS — 40 started, 40 successful, 0 failed (4.036s) |

Both logs contain `SBRUNNER_RESULT tests=40 failed=0 aborted=0 skipped=0
containersFailed=0`. There are no residual failures attributable to this
issue, so this document is retired from `docs/known-issues`.
