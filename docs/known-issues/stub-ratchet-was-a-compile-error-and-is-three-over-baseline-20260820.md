# The stub ratchet has not been running, and it is three over its baseline

**Status: OPEN.** Found 2026-08-20 while merging current dev into an unrelated
TLS branch. Two facts, and the first explains the second.

## 1. `native-builtins/tests/stub_ratchet.rs` does not compile on dev

```
error: unknown start of token: \
    --> native-builtins/tests/stub_ratchet.rs:1169:78
error: this file contains an unclosed delimiter
    --> native-builtins/tests/stub_ratchet.rs:1735:3
```

`synthetic_stub_count_does_not_regress`'s message was rewritten and the OLD
message body was left behind, unquoted, immediately after the new one's closing
`",` — sixteen lines of prose sitting in expression position. It arrived with
`26e4b5db4 Merge branch 'claude/jdk-only-mode-completion-1351c0' into dev`
(2026-08-20).

The blast radius is the whole crate's integration suite: `cargo test -p
cratonvm-native-builtins` cannot build the test target, so it fails before any
test in `stub_ratchet.rs` runs — and a reader sees a compile error, not a
ratchet verdict.

**Repaired in the branch that found it**, minimally: the orphaned block is
deleted and the one sentence from it that the call still passes an argument for
(`WHERE THEY ARE (top 8 files): {}`, consuming `breakdown`) is folded into the
surviving message. Nothing else about the message is invented — the new text was
already complete on its own; it was two format arguments and one `{}`.

## 2. With it compiling, the ratchet is RED by three

```
STUB-RATCHET in the no-management configuration: 1614 SyntheticStub natives
now registered, exceeding the frozen baseline of 1611.
```

`BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT = 1611` was set by
`083998c7b fix(jdk-only): 227 §1.4 shadows retired` (2026-08-19), which is an
ancestor of dev. So three SyntheticStub registrations have been added since,
under cover of a test target that could not be built.

**It is not the TLS branch that found it.** Measured, by putting dev's
`native-builtins/src` and `native-io/src` back under the repaired test:

```
dev sources + repaired test        1614  (baseline 1611)   FAILED
branch sources + repaired test     1614  (baseline 1611)   FAILED
```

Same number both ways: the branch contributes zero of the three. (Its own new
`SSLContext.getProvider` registration in `tls.rs::register_ssl_context` is
invisible to this census, which walks `register_essential_natives`, and that
registrar is only reached from the `synthetic-jdk`-gated
`register_synthetic_overrides`.)

## What it would take to close

The test says it itself, and the instruction is worth following rather than
short-cutting:

> FIRST, find out WHICH rows, because this number cannot tell you why it moved.
> Run `dump_synthetic_stubs` here and at the commit that last set
> `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT`, and diff the sorted `@@STUB` lines.

So: build `083998c7b`, run `dump_synthetic_stubs` there and on dev, diff. Then
classify each of the three — a NEW fake (implement it properly, do not raise the
baseline) or a `Bridge`→`SyntheticStub` RETAG of something already fake (an
improvement, re-freeze with the list). The `stub-ratchet` record notes that on
2026-08-19, 30 of 33 added rows were the second kind, so the prior is retag —
but a prior is not a measurement, and **the baseline must not be re-frozen
without the row list.**

Deliberately NOT done here: re-freezing 1611 → 1614 would turn a gate that has
been blind for a day into a gate that has been blind for a day and then
rubber-stamped.

## Three more gates were red on dev in the same pass

Found by running the two crates' suites either side of a
`git checkout origin/dev -- <src>` control, which is the only way to tell
"my change broke it" from "it was already broken". None of these is the TLS
branch's; each was measured with dev's own sources under the same test binary.

| crate | test | control (dev sources) |
|---|---|---|
| `cratonvm-native-builtins` | `lang_class::tests::null_receiver_on_an_instance_field_outranks_the_access_refusal` | FAILED |
| `cratonvm-native-io` | `io_tests::fis_close_marks_closed_and_is_idempotent` | FAILED |
| `cratonvm-native-io` | `io_tests::fis_read_bytes_zero_length_answers_zero_on_a_closed_stream` | FAILED |
| `cratonvm-native-io` | `io_tests::fis_skip_consults_the_descriptor_before_the_count` | FAILED |

A fourth, `lang_math::tests::canonical_wrapper_if_cached_follows_the_configured_integer_bound`,
was fixed rather than filed — it was a one-line `i32` overflow in
`canonical_wrapper_if_cached`'s index computation
(`i32::MAX - INTEGER_CACHE_LOW`), where the contract is a cache MISS. Release
builds wrap and miss anyway, so only the debug gate was red.

The three `fis_*` rows share a prefix and are almost certainly one cause; they
are listed separately because that has not been checked.

**The pattern, not the rows.** Five distinct unit-test gates were red on dev at
once, and one of them was a compile error that had been masking a sixth. This
is the same shape as the 2026-08-18 finding that four native-builtins gates were
red on dev simultaneously. A crate suite that nobody runs to green stops being
a gate; the cheapest fix is to run `cargo test -p <crate>` on the crates a
change touches, and to control any failure against `origin/dev` sources before
believing it is yours.

## Repro

```bash
cargo test -p cratonvm-native-builtins --test stub_ratchet          # on dev: compile error
cargo test -p cratonvm-native-builtins --test stub_ratchet -- dump_synthetic_stubs --nocapture
```
