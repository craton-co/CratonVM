# The stub ratchet had not been running, and it was three over its baseline

**Status: FIXED 2026-08-21.** Found 2026-08-20 while merging current dev into an
unrelated TLS branch. Two facts, and the first explains the second.

The compile break was repaired the day it was found; the three rows were
identified and the baselines re-frozen on 2026-08-21, with the account in the
constant's own doc comment. Both configurations are green:

```
cargo test -p cratonvm-native-builtins --test stub_ratchet                        11 passed
cargo test -p cratonvm-native-builtins --features management --test stub_ratchet  11 passed
```

## 1. `native-builtins/tests/stub_ratchet.rs` did not compile on dev

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

## 2. With it compiling, the ratchet was RED by three

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

## The three rows, and why the gate's own dichotomy did not fit them

`dump_synthetic_stubs` did not exist at the freeze commit `083998c7b` — it was
added later — so the page's original recipe was not directly runnable. The
substitute is honest and cheap: graft the CURRENT test onto that commit's tree.
`census_rows` is byte-identical between the two, so the graft measures
`083998c7b`'s registry and nothing else.

```text
cratonvm/internal/ss/JavaUtilJarAccess$1.entryFor(Ljava/util/jar/JarFile;Ljava/lang/String;)Ljava/util/jar/JarEntry;
cratonvm/internal/ss/JavaUtilJarAccess$1.getTrustedAttributes(Ljava/util/jar/Manifest;Ljava/lang/String;)Ljava/util/jar/Attributes;
cratonvm/internal/ss/JavaUtilJarAccess$1.isInitializing()Z
```

Three added, **zero removed**, from `392e6990a` (2026-08-19), which completed
`jdk.internal.access.JavaUtilJarAccess`: the carrier had two of its five methods
and the other three were abstract on a synthetic class, so a caller reaching one
got an `AbstractMethodError`.

**The two-column rule did not adjudicate this one, and that is the finding.**
The file's own classifier is:

> * total UNCHANGED, stubs up → existing fakes were relabelled. Welcome.
> * total UP by about the stub delta → new fakes were registered. Do not re-freeze.

Measured: totals moved 12792 → **12885** (no-management) and 13160 → **13253**
(management), i.e. **+93 rows against +3 stubs**. Neither case. It is 90 new
`Bridge` rows and 3 new stubs, and the 3 are genuinely new registrations.

By the letter, that is case (a) — "do NOT just raise the baseline". Both of
case (a)'s remedies are unavailable here, and not by accident:

* **"make the new native a real `Bridge`/`Intrinsic`" would be wrong.**
  `cratonvm/internal/ss/JavaUtilJarAccess$1` is on
  `no_image_receiver::VM_MINTED_STAND_IN_RECEIVERS`, and `F33-1` decided
  deliberately that a factory and the carrier it hands out share one kind — so
  the whole thing is refused under `--jdk-only`. Tagging these three `Bridge`
  would keep a fake carrier alive in strict mode, which is the exact defect that
  page exists to record.
* **"fix the underlying VM gap so real bytecode runs" has nothing to run.** The
  receiver's class exists only inside CratonVM and has no bytecode at all. There
  is no JDK body behind `entryFor` to yield to.

So the alternative to these three stubs is not real bytecode — it is the
`AbstractMethodError` they were added to stop, and `--jdk-only` behaviour is
identical either way because the carrier is dropped whole in both.

**The third case, now written into the constant's doc comment:** a stub added to
a receiver that strict mode already refuses IN ITS ENTIRETY costs strict mode
nothing. Ask what mints the receiver before applying the dichotomy; if the owner
is a listed stand-in, the row count is the only thing that moved, and completing
its interface is a fix rather than a regression.

All four constants re-frozen together — both stub baselines AND both
`MEASURED_TOTAL_REGISTRATIONS`, because a re-freeze that leaves the totals stale
disarms the classifier for the next reader.

## Three more gates were red on dev in the same pass, and are still open


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
