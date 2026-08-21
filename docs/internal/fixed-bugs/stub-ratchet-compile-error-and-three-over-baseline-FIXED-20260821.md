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

## Four more gates were red on dev in the same pass — two fixed, two still open (four tests)


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
| `cratonvm-native-builtins` | `proxy_selector::tests::env_proxy_lookup_respects_case_insensitive_windows_storage` | FAILED (**Windows only**; arrived 2026-08-21 with a later dev merge) |

Two were fixed rather than filed, both one-liners with no production behaviour
change:

* `lang_math::…_follows_the_configured_integer_bound` — an `i32` overflow in
  `canonical_wrapper_if_cached`'s index computation
  (`i32::MAX - INTEGER_CACHE_LOW`) where the contract is a cache MISS. Release
  builds wrap and miss anyway, so only the debug gate was red.
* `lang_class::null_receiver_on_an_instance_field_outranks_the_access_refusal` —
  **the TEST was wrong and the production message was right all along.** It
  asserted `format!("{err:?}").contains("because \"o\" is null")`, and `Debug`
  for a `String` ESCAPES its quotes, so the rendering carries
  `because \"o\" is null` while the needle was unescaped. It could never have
  passed. Now compared against the `ENSURE_OBJ_NPE` constant the same call
  passes in — strictly stronger than the substring, because it pins the
  `Object.getClass()` half that names WHICH dereference HotSpot reports.

## The three `fis_*` rows are one cause, and it is not the test

Diagnosed 2026-08-21, **not fixed here** — see below for why.

All three assert that a positively-marked closed `FileInputStream` refuses
`read()`, `read(byte[])` and `skip()`. Each of those bodies is shaped:

```rust
let fd = match fis_get_fd(ctx, this) {
    Some(fd) => fd,
    None if fis_is_closed(ctx, this) => return Err(io_stream_closed()),
    None => return Ok(...),          // the benign answer
};
```

and each carries a comment saying "Only a positively-marked close is refused".
**That is not what the code does.** `fis_is_closed` is consulted only when the
DESCRIPTOR lookup already failed, so a stream whose close wrote the closed
marker while `FileDescriptor.fd` still reads as a number takes the `Some(fd)`
arm and performs the I/O. `io_stream_is_closed` has two independent grounds —
`(fd < 0 && handle < 0)`, or a negative marker in slot 0 — and only the first
can ever be reached from these bodies.

That is exactly the failure family the comments cite
(`G4-1-the-io-and-nio-fabricated-success-sweep`): a closed stream answering EOF
instead of throwing makes `while ((n = in.read()) != -1)` exit cleanly and the
copy come out silently truncated.

**The mock is faithful, which is the part worth stating.** `native-io`'s
`MockNativeContext` keeps `set_field_by_name` in a side map keyed by
`(ptr, name)`, disjoint from the indexed `get_field(this, 0)` slots — so a test
that sets `fd` by name and a marker written by index do not alias. That is what
production looks like when the two writes land in different places, and it is
why these tests see the hole.

**Scope, and why it is filed rather than fixed here.** The same shape is at TEN
sites — five `fis_*` and five `fos_*` (`native-io/src/lib.rs`, the
`None if f{i,o}s_is_closed` arms). Fixing three would leave the family
inconsistent, which is worse than either extreme, and fixing ten is a behaviour
change across every `java.io` consumer in the suites — it wants its own branch
and its own regression run, not a ride inside a netty TLS change.

The fix, when someone takes it: hoist the closed check ABOVE the descriptor
lookup at all ten, in one shared helper so they cannot drift. One carve-out must
survive — `native_fis_read_bytes`'s `is_empty_transfer(len)` early return has to
stay in front of the closed check, because HotSpot measurably answers
`read(b, 0, 0)` on a closed stream with `0` and no throw.
**The pattern, not the rows.** Seven distinct unit-test gates were red on dev
across two days, and one of them was a compile error that had been masking an
eighth. This is the same shape as the 2026-08-18 finding that four
native-builtins gates were red on dev simultaneously. A crate suite that nobody
runs to green stops being a gate; the cheapest fix is to run
`cargo test -p <crate>` on the crates a change touches, and to control any
failure against `origin/dev` sources before believing it is yours.

## The proxy row is a test DOUBLE that stopped modelling Windows

Separate from the `fis_*` family, and also filed rather than fixed, because
unlike the other two test-side reds this one has no obviously-correct one-liner.

`env_proxy_lookup_respects_case_insensitive_windows_storage` sets both
`all_proxy` and `ALL_PROXY` and asserts the UPPERCASE value wins, on the stated
grounds that "Windows stores environment keys case-insensitively, so the final
assignment is the single value visible through either spelling". It now reads
back the lowercase one.

Production is not implicated. On real Windows `std::env::var` IS
case-insensitive, so `read_settings` behaves as the test describes. What changed
is the DOUBLE: `with_proxy_env` was rewritten to stop mutating `environ` at all
("Nothing is written to `environ` now, so there is nothing to restore and
nothing to race" — a good change, it removed a process-wide data race against
every parallel test) and the thread-override map that replaced it is keyed
case-SENSITIVELY. So the two spellings are two entries where Windows has one.

`#[cfg(windows)]`, so the Azure Linux host never runs it; its
`not(windows)` twin — `env_proxy_lookup_prefers_lowercase_when_both_are_set` —
passes, because lowercase-wins is exactly what a case-sensitive map gives you.

The fix is a decision, not a repair: either `with_thread_overrides` folds case
on Windows so the double models `environ` (correct, but it is a shared helper
with many callers), or this test stops claiming to exercise case-insensitive
storage it can no longer reach. Whoever takes it should say which, rather than
making the assertion agree with the double.

## Repro

```bash
cargo test -p cratonvm-native-builtins --test stub_ratchet          # on dev: compile error
cargo test -p cratonvm-native-builtins --test stub_ratchet -- dump_synthetic_stubs --nocapture
```
