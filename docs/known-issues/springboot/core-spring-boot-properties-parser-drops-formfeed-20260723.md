# OriginTrackedPropertiesLoader silently drops a literal form-feed byte from a property value

**Status: OPEN — found 2026-07-23**

## Symptom

```
JUnit Jupiter:OriginTrackedPropertiesLoaderTests:compareToJavaProperties()
    => org.opentest4j.AssertionFailedError:
expected: {..., "test-form-feed-property"="foo<FF>bar", ...}
 but was: {..., "test-form-feed-property"="foobar", ...}
```

(The literal 0x0C form-feed byte between "foo" and "bar" doesn't render in
the log, but is present byte-for-byte in the raw log file — confirmed with
`diff` against the "but was" line, which is otherwise character-identical
except this one value.) Every other value in the ~29-entry comparison
map — including ones with other odd characters (`\t`, `\r`-derived
newlines, ISO-8859-1 accented chars, embedded `=`) — round-trips correctly.
Only `test-form-feed-property` differs, and only by the missing form-feed
byte.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard2/logs/core_spring-boot.org.springframework.boot.env.OriginTrackedPropertiesLoaderTests.out.log`

## Root cause

Not pinned to an exact file:line. `OriginTrackedPropertiesLoader`
(`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/env/OriginTrackedPropertiesLoader.java`)
is Spring's own, from-scratch `.properties` character reader (it does not
delegate to `java.util.Properties`) and has no special-case handling for
form-feed (`\f`, 0x0C) anywhere in its source — a literal FF byte embedded
in a `.properties` value (not one of the defined escape sequences like
`\n`/`\t`/`\r`/`\\`/`\uXXXX`) is meant to be copied through verbatim by
this loader, same as any other non-escape character.

Since this is real, unmodified Spring bytecode reading char-by-char, the
byte loss must be happening in something CratonVM provides underneath it —
most likely a whitespace-classification helper. Java's own
`Character.isWhitespace(0x0C)` returns `true` (form feed is Unicode
whitespace), and `.properties` parsing genuinely needs to skip *leading*
whitespace on continuation lines; if CratonVM's loader (or a shared
low-level char-scanning primitive it was written to rely on) applies that
same "skip whitespace" check to a form-feed appearing **mid-value** — not
just at a line's leading edge — it would silently drop exactly this one
byte and nothing else, matching what's observed. This is a hypothesis, not
confirmed — no CratonVM native override specific to this loader or to
`Character.isWhitespace` was located this session.

**What would confirm/refute:** a minimal repro loading a one-line
`.properties` file `k=foo\x0Cbar` (raw FF byte, not the `\f` escape, which
`.properties` doesn't define) via `OriginTrackedPropertiesLoader` in
isolation, checking whether the FF survives into the parsed value.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.env.OriginTrackedPropertiesLoaderTests |
