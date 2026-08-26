# A refused `SyntheticStub` falls through to an older NATIVE, not to bytecode — and in `--jdk-only` `Handler.setLevel` writes the `LogManager` field

**Status: FIXED 2026-08-24** (the `Handler` pair). The mechanism is
**exhaustively measured at 9 triples**, listed in §3.

## 0. The species, which no gate could see

`registrar_drift.rs` compares a **synthetic-only** pass against a **shipping**
pass. It is structurally blind to two shipping passes registering the same
triple — and that is where this lives.

`register` is last-write-wins, but in `--jdk-only` a registration whose kind is
`SyntheticStub` is **refused** (`allowed_in(JdkOnly)`), so it never enters the
registry at all. When an EARLIER registration of the same triple exists, the
refusal does not hand the method to real JDK bytecode:

> **the earlier native survives as the winner instead.**

The retirement silently does not happen, and the two modes run different code.
Worse for the metric: if the survivor's kind is `Intrinsic`, the shadow census
never counts it — the recorder skips `kind == Intrinsic` at all three sites — so
these rows are invisible retirements *and* invisible to the number that is
supposed to track them.

## 1. Measured, exhaustively

Two `--dump-native-registry` runs on one binary (debug, 2026-08-24 18:59,
`RStrings`), compared on the **winning registration** per triple:

```text
winners: compatible 11623 · --jdk-only 10080
triples owned in BOTH modes: 10080
  ... where the WINNING REGISTRATION DIFFERS:  9
```

**Nine.** Not a sample — every triple in the registry, both modes. The other
1,543 compatible-only winners are triples where *every* registration was refused
in strict, i.e. the retirement working as designed.

## 2. Triage of the nine — 4 intentional, 3 benign, 2 defects

Reading all nine mattered, because the raw count over-states by 4.5x.

**Intentional (4).** `Runtime.{load0,loadLibrary0}`, `System.{load,loadLibrary}`
— `lang_system.rs` branches on `registry.compatibility_mode().is_jdk_only()` and
deliberately registers a different body per mode, with the reason in a comment
(`LoaderScoping::On` vs `Off`; "the cross-loader `loadedLibraryNames` rule stays
strict-only"). Correct as written.

**Benign (3).** `LogRecord.{getLevel,getMessage,getSequenceNumber}` — the
winners differ, the behaviour does not. The `phases_early.rs` bodies go through
`lr_get`, which is layout-aware:

```rust
if crate::log_record_real_layout(ctx, this) {
    Ok(Some(ctx.get_field_by_name(this, name)))
} else {
    Ok(Some(ctx.get_field(this, slot)))
}
```

**Defects (2).** `Handler.getLevel()` / `Handler.setLevel(Level)`. The
`phases_early.rs` bodies that win in `--jdk-only` used a **bare slot 0** with no
such guard, while the `reflect_annotations.rs` bodies they displace read
`get_field_by_name(this, "logLevel")`.

That the sibling `LogRecord` family in the *same function* does it correctly is
the point: the pattern was not unknown, `Handler` just never got it.

## 3. What slot 0 actually is

`javap -p --system`, Temurin 25.0.3+9 — `java.util.logging.Handler`'s six
instance fields, in order (`offValue` is static and takes no slot):

```text
  0 manager(LogManager)   1 filter   2 formatter
  3 logLevel(Level)       4 errorManager        5 encoding
```

So in `--jdk-only`, `getLevel()` answers the **`LogManager`** and `setLevel()`
**overwrites `manager` with a `Level`**.

## 4. Both halves, measured off the real fields

`probes/JulHandlerLevel.java`, via
`--add-opens=java.logging/java.util.logging=ALL-UNNAMED`:

```text
HotSpot 25.0.3+9        PASS (10 checks)
CratonVM compatible     PASS (10 checks)
CratonVM --jdk-only     FAIL (3 of 10)
    getLevel on a fresh Handler: got=java.util.logging.LogManager@587  want=ALL
    setLevel(WARNING) did not reach Handler.logLevel: logLevel=ALL
    setLevel OVERWROTE Handler.manager with a Level: WARNING
```

The probe deliberately uses a bare `Handler` subclass with no-op sinks rather
than `ConsoleHandler`: constructing a `ConsoleHandler` dies in
`Charset.newEncoder()` with `AbstractMethodError: has no Code attribute`, a
SEPARATE defect that would have stopped this probe before it measured anything.

**Corrected 2026-08-25.** The first draft of this paragraph called that "a live
`--jdk-only` blocker on the whole `StreamHandler` family". Both halves of that
were wrong, and measuring took one probe:

* it is **not** `--jdk-only`-specific — `PrintStream.charset()` answers a
  fabricated ABSTRACT `java.nio.charset.Charset` in BOTH modes (6 of 9 checks
  wrong in compatible, 7 of 9 in strict). Compatible only escapes the crash on
  the `OutputStreamWriter` path because a native covers it there;
* it is **not** the whole family — `new StreamHandler(baos, fmt)` is fine, and
  so is every `Charset.forName(...).newEncoder()`. Only a `PrintStream` SOURCE
  triggers it, because JDK 19+ `OutputStreamWriter(OutputStream)` asks a
  `PrintStream` for its `charset()`.

Root cause and fix are in
`bug-printstream-charset-answers-the-abstract-base-20260825.md`.

**Why the round-trip checks pass and hid this.** `setLevel` wrote slot 0 and
`getLevel` read slot 0, so they agreed with each other perfectly. Only a reader
that does NOT go through the pair — a fresh handler's default, or the real field
via reflection — can see it. A self-consistent wrong slot is invisible to any
round-trip test.

## 5. The fix

`handler_real_layout(ctx, h)` = `object_num_fields(h) >= 6`, and both bodies use
`logLevel` by name when it holds.

The count is the discriminator rather than a name probe for a stated reason:
every instance field on `Handler` is a REFERENCE, so `get_field_by_name` cannot
be type-checked the way `log_record_real_layout` checks `longThreadID` for a
`Long`; and it answers `Value::Object(None)` for an unresolvable name, which is
indistinguishable from a genuinely null `logLevel` — "inherit from the parent"
being a legal state. Same shape as `panama_libffi::segment_address`'s
`object_num_fields(seg) >= 6`.

## 6. What is NOT claimed

* The other 7 of the 9 are left alone, with the reasons in §2.
* `Handler.manager` is `null` on CratonVM in compatible mode where HotSpot has a
  real `LogManager`. That is a separate gap, not a type violation, and this
  change does not address it.
* The 9 covers triples owned in BOTH modes. A triple that all registrars fail to
  register in strict is a different (working) case, and a synthetic-only pass
  competing with a shipping one is `registrar_drift.rs`'s job.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only --add-opens=java.logging/java.util.logging=ALL-UNNAMED -cp probes/out JulHandlerLevel
```
