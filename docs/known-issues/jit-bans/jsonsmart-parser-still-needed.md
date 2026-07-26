# JSONSMART-PARSER.1 (`net/minidev/json/parser/`) — CONFIRMED still needed: ~20% corruption rate under JIT

**Status:** Ban confirmed live and necessary. Fourth-for-four real-app-family
test this session in the SPB/CGL/PIC/allocate-then-putfield ban cluster to
find a still-live bug (`org/jboss/as/`, `org/h2/`, `com/unboundid/`, now
`net/minidev/json/parser/`).

## Context

"Medium" priority item from `docs/known-issues/jit-skip-list-open-bans-20260725.md`.
`json-smart-2.6.0.jar` is self-contained (no Spring context needed, unlike
the SPB.1 investigation) — available in `~/.gradle/caches/modules-2/files-2.1/net.minidev/json-smart/2.6.0/...`.
Built a standalone stress repro (`JsonSmartProbe.java`): 10 varied JSON
documents (nested objects/arrays, escaped strings, unicode, numbers,
whitespace-padded, empty containers) parsed with `JSONParser` in a tight
300k-iteration loop, with a round-trip check (parse → serialize → re-parse
→ compare) to catch silent value corruption, not just exceptions.

## Result

| Config | Result (200s timeout, both hit it) |
|---|---|
| Baseline (ban in place) | Never completed the first 30k-iteration checkpoint in 200s (interpreted `net/minidev/json/parser/*` is much slower — expected) but **0 errors** in whatever it did process. |
| `CRATONVM_JIT_ALLOW_PACKAGES=net/minidev/json/parser/` (lifted) | Raced to 1,200,010 parse operations in the same 200s (JIT-compiled, much faster) but **239,605 errors — a ~20% failure rate**, and climbing steadily throughout the whole run (not a one-time startup glitch). |

Corruption starts almost immediately (by iteration 5, well before any real
JIT warm-up threshold in a typical config) and is **unstable** — the exact
same input document produces *different* parse-exception messages across
consecutive iterations:
```
iter=5:  ParseException: Unexpected token "tab" at position 6.
iter=7:  ParseException: Unexpected token "tab":"a\tb at position 12.
iter=16: ParseException: Unexpected character (a) at position 5.
```
Same document, three different corruption shapes within 11 iterations —
this is a live-memory/register-state-dependent miscompile, not a
deterministic logic bug in the parser itself (a real parser bug would fail
the same way every time). Matches the ban's own description almost exactly:
"the default JIT crashes inside emitted code after compiling parser cursor
methods such as `JSONParserString.read()`, `JSONParserString.readS()`, and
`JSONParserBase.skipSpace()`."

## Disposition

**KEEP `net/minidev/json/parser/` banned.** No ambiguity — 20% corruption
rate is severe and immediate, not a rare edge case. This is the cleanest,
highest-magnitude confirmation of this session's four real-app tests in
this ban family.

## Reproduction

Repro source: `docs/known-issues/repros/jsonsmart/JsonSmartProbe.java`.
```bash
JS=<path-to-json-smart-2.6.0.jar>
TMPDIR=/data/tmp CRATONVM_JIT_ALLOW_PACKAGES='net/minidev/json/parser/' \
  <cratonvm-binary> --java-home /home/victor/jdk25 -Xmx1g -c "<classdir>:$JS" \
  JsonSmartProbe
```
Corruption is visible within the first ~20 iterations — no need to run the
full 300k-iteration loop to confirm; useful as a fast (<5s) smoke check.

## Running tally across this session's SPB/CGL/PIC-family real-app tests

| Ban | Test | Result |
|---|---|---|
| `org/jboss/as/` | Real WildFly boot | KEEP — `ModelTypeValidator.validTypes` NPE |
| `org/h2/` | Real 218-class H2 suite | KEEP — `Schema  not found` on reconnect |
| `com/unboundid/` | Real Tomcat `TestJNDIRealmIntegration` | KEEP — SIGSEGV, stale-pointer receiver |
| `net/minidev/json/parser/` | Standalone json-smart stress repro | KEEP — ~20% parse corruption |
| `org/springframework/util/` | Standalone repro (no fixture app) | Inconclusive (see separate doc) |

Four for four confirmed still-needed among the real-app/faithful-repro
tests. Nothing in this large ~25-entry family has been found safe to
remove yet.

## Related

- `docs/internal/jit-ban-sweep-20260725.md` — this session's tracking doc.
- `docs/known-issues/jit-skip-list-open-bans-20260725.md` — shared
  cross-session coordination doc.
