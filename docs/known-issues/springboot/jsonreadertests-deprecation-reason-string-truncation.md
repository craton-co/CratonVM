# `JsonReaderTests#deprecatedMetadata`: a parsed JSON string value is truncated by exactly its trailing word

**Status: OPEN — found 2026-07-17**

## Symptom

`org.springframework.boot.configurationmetadata.JsonReaderTests`
(`configuration-metadata/spring-boot-configuration-metadata`) has 8 tests; 1
fails:

```
JUnit Jupiter:JsonReaderTests:deprecatedMetadata()
    => org.opentest4j.AssertionFailedError:
expected: "Server namespace has moved to spring.server"
 but was: "Server namespace has moved to spring."
       org.springframework.boot.configurationmetadata.JsonReaderTests.deprecatedMetadata(JsonReaderTests.java:154)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/configuration-metadata_spring-boot-configuration-metadata.org.springframework.boot.configurati-9cc8b656feb2.out.log`

The source fixture
(`apps/spring-boot/configuration-metadata/spring-boot-configuration-metadata/src/test/resources/metadata/configuration-metadata-deprecated.json`)
contains the literal JSON string value:

```json
"reason": "Server namespace has moved to spring.server",
```

CratonVM's parsed result is missing exactly the trailing word `server` (6
characters) — the actual value ends at the last `.` in the source string,
one character before where the real value continues into `server`. This is
not a wrong-item mix-up (no other property in the same fixture file has a
`reason` field this could have been confused with; only 1 of the file's 5
items even has a `reason` at all) and not a wrong-quote/early-termination
parse (there's no unescaped `"` or other JSON-special character anywhere
near the truncation point) — it reads as a literal drop of the string's
last 6 characters somewhere in the read path.

## Root cause

**Not confirmed — no CratonVM source pinned.** `JsonReader`
(`apps/spring-boot/configuration-metadata/spring-boot-configuration-metadata/src/main/java/org/springframework/boot/configurationmetadata/JsonReader.java`)
parses via a vendored, trimmed `org.springframework.boot.configurationmetadata.json.JSONObject`/`JSONArray`
fork of `org.json` (real, ordinary Java bytecode — not a CratonVM native
override) — its `JSONTokener`/string-literal-reading source was not located
in this worktree to inspect directly (likely compiled into a dependency jar
rather than vendored as source here), so this doc cannot cite an exact
file:line the way the other docs in this rerun do. Two hypotheses, neither
verified:

1. A bug in the vendored JSON tokenizer's string-literal read loop
   (character-buffer bookkeeping, `StringBuilder` capacity/length handling,
   or an early-exit condition) that happens to manifest under CratonVM but
   not HotSpot — possible if the tokenizer's read loop exercises a code
   path (e.g. `Reader.read(char[], int, int)` bulk reads vs. single-`read()`
   calls, or a `StringBuilder`/`String` low-level operation) that CratonVM
   implements with a genuine off-by-N bug for this specific length/shape of
   input. No specific candidate function identified.
2. A CratonVM-level `String`/`StringBuilder`/character-decoding bug
   (`InputStreamReader`, `String.valueOf(char[], int, int)`, or similar)
   independent of the JSON library itself, which the JSON tokenizer merely
   exposes by building its result string through one of those primitives.

Both remain speculative. Confirming would need a standalone repro: parse the
literal fixture JSON (or a minimal reproduction of just the `reason` field)
through the vendored `JsonReader` under CratonVM with tracing/logging
inserted into the tokenizer's string-read loop, to see exactly where the
last 6 characters are dropped — not attempted this session (out of scope:
no code changes or rebuilds permitted for this investigation).

## Affected classes

| Module | Class |
|---|---|
| `configuration-metadata/spring-boot-configuration-metadata` | `org.springframework.boot.configurationmetadata.JsonReaderTests` (1 of 8 tests) |
