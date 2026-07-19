# `JsonReaderTests#deprecatedMetadata`: BreakIterator split a dotted identifier in a short deprecation reason

**Status: FIXED — 2026-07-18**

## Symptom

`org.springframework.boot.configurationmetadata.JsonReaderTests` failed only
`deprecatedMetadata()` under CratonVM:

```
expected: "Server namespace has moved to spring.server"
 but was: "Server namespace has moved to spring."
```

The failure is for `Deprecation.shortReason`, not for the parsed JSON
`Deprecation.reason` value. The original report incorrectly attributed the
suffix loss to JSON string parsing.

## Root cause

`JsonReader.parseDeprecation()` stores the full `reason` and separately calls
`SentenceExtractor.getFirstSentence(reason)` for `shortReason`. That extractor
uses `BreakIterator.getSentenceInstance(Locale.US)` whenever the text contains
a dot.

CratonVM's bridge `BreakIterator` in
`native-builtins/src/phases_late.rs::bi_find_next` treated every `.`, `!`, or
`?` as a sentence terminator. It did not require whitespace (or end of text)
after the punctuation, so `spring.server` was split after `spring.`.

The JSON read chain is sound: a dedicated `char[] -> StringBuilder -> String ->
substring` probe passes in both execution modes. The repair changes the
sentence rule to terminate only at end of text or before ASCII whitespace.

## Regression coverage

- `break_iterator_sentence_boundary_tests` verifies that `spring.server` stays
  in one sentence and that `"First sentence. Second sentence."` still breaks
  after the whitespace-separated terminator.
- `vm/tests/resources/cratonvm/JsonReaderSubstringProbe.java` exercises the
  complete string-read and sentence-boundary shape; it is registered in the
  JCK-style conformance corpus.

## Verification

On Azure Linux with JDK 25, isolated worktree
`/data/wt-jsonreader-deprecation-string-20260718`, unique release binary
`/data/data/cratonvm-bins/cratonvm-jsonreader-deprecation-20260718`
(`sha256=7874d30542f00cdd5a672d3771ec21c7a867c56b5e3a6553025a8a490eaf15b4`):

- `cargo test -p cratonvm-native-builtins break_iterator_sentence_boundary_tests --lib`: 2 passed.
- `JsonReaderSubstringProbe`: PASS with JIT and `--nojit`.
- Real Spring Boot `JsonReaderTests`: 8/8 passed with JIT and 8/8 passed with
  `--nojit`.

The focused Spring class emitted only pre-existing JUnit layout-guard warnings;
there were no crash, timeout, or assertion residuals.
