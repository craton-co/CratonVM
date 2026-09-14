# A refused `SyntheticStub` falls through to an older NATIVE, not to bytecode — and in `--jdk-only` `Handler.setLevel` writes the `LogManager` field

**Status: CLOSED 2026-09-01.** The `Handler` pair was fixed 2026-08-24 and is
re-verified in §7.2; §6's `Handler.manager` residual is fixed in §7.3; and the
species itself — which §0 says no gate could see — now has one, at two levels,
in §7.4. The mechanism is **exhaustively measured at 9 triples**, listed in §3,
and the nine reproduce unchanged a week later (§7.1).

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
`docs/internal/fixed-bugs/bug-printstream-charset-answers-the-abstract-base-20260825-FIXED-20260901.md` (retired 2026-09-01, both residuals closed).

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

## 7. CLOSED 2026-09-01 — re-measured, §6's residual fixed, and the species now has a gate

### 7.1 The nine reproduce exactly, one week on

Two `--dump-native-registry` runs on one binary (debug, 2026-09-01, built from
dev's tip), compared on the winning registration per triple — the same
comparison §1 made:

```text
winners: compatible 11718 · --jdk-only 10178
triples owned in BOTH modes: 10178
  ... where the WINNING REGISTRATION DIFFERS:  9
compatible-only winners: 1540
strict-only winners: 0
```

The same nine triples, with the same triage: the four `lang_system.rs` rows are
still the deliberate per-mode branching, and the five logging rows are still the
species. The registry grew by ~95 triples in the week between the two
measurements and the nine did not move — which is what makes the frozen set in
§7.4 a baseline rather than a snapshot.

### 7.2 The `Handler` pair, verified off the real fields

`probes/JulHandlerLevel.java` rebuilt (the file §4 used is not in the tree; the
rewrite reads `manager` and `logLevel` through
`--add-opens=java.logging/java.util.logging=ALL-UNNAMED`, which is the only way
to see a self-consistent wrong slot):

```text
HotSpot 25.0.4+7        PASS (10 checks)
CratonVM compatible     PASS (10 checks)
CratonVM --jdk-only     PASS (10 checks)
```

§4's three failures are gone in the arm that had them. `handler_real_layout` is
in `phases_early.rs` and both bodies go through it.

### 7.3 §6's `Handler.manager` residual — measured, and it was compatible-mode only

§6 left it as "a separate gap". Measured, it is narrower than that reads, and
the strict arm was already right:

```text
Handler.manager on a fresh Handler   HotSpot     java.util.logging.LogManager
                                     --jdk-only  java.util.logging.LogManager
                                     compatible  null
```

The asymmetry names its own cause. `Handler.<init>()V` is registered as a
`SyntheticStub` (`reflect_annotations.rs`), so **strict refuses it and the real
constructor runs** — and the real constructor's first field initializer is
`private final LogManager manager = LogManager.getLogManager();`. In compatible
mode the native `<init>` stands in for the constructor and, as the comment
already beside it says of `errorManager`, *none of the JDK's field initializers
run*.

So this is the same defect as `W7-64`'s, one field over, and it is fixed the
same way: the native `<init>` now writes `manager` from
`logmanager::jul_log_manager_singleton` — the very object
`LogManager.getLogManager()` returns — behind the same two guards (the field
must EXIST, because a synthetic `Handler` has none and `get_field_by_name`
cannot tell "absent" from "null"; and it must still be null, so a constructor
that did run is never clobbered). Both modes now report
`manager=java.util.logging.LogManager`, and the two modes converge rather than
one of them being invented.

### 7.4 The species now has a gate — at two levels, because one level cannot see it

§0's point was that no gate could see this. That was true of both existing
censuses, and each is blind for its own structural reason:

* `registrar_drift.rs` compares a **synthetic-only** pass against a **shipping**
  one. This is two SHIPPING passes, so it is outside the comparison — §0's
  finding, unchanged;
* `duplicate_registration_gate.rs` names it in its own header as **blind spot
  3**: *"a DROPPED registration leaves no row at all"*, because every drop arm
  in `register()` returns before pushing to `registrations`. A shadowed LOSER
  has a census row. A refused one does not.

The fix is to make the registry say so at the moment of refusal.
`JdkOnlyViolation::SyntheticNativeRegistered` gains a `survivor` field —
`"<kind>@<file>:<line>"` of the registration that still owns the slot, `None`
when the refusal genuinely retired the method — filled from
`NativeMethodRegistry::surviving_owner`, which is the ordinary slot lookup run
one line before the refusing `return`. It costs one lookup per refusal, and
refusals are bounded (1,680 on this tree) rather than by the ~12,000
registrations. It is emitted in the `--jdk-only-report` JSON, so a consumer that
tallies by `kind()` alone can now separate the two shapes it was counting as
one.

Two consumers:

* **`native-api/tests/jdk_only_registry.rs ::
  a_refusal_over_an_owned_triple_names_the_survivor`** — a two-registration
  registry: an `Intrinsic`, then a `SyntheticStub` for the same triple under
  `JdkOnly`. It asserts that `find` STILL RESOLVES afterwards (that is the
  defect, stated as an assertion rather than as prose) and that
  `refusals_that_left_a_survivor()` names the triple and leads with the
  survivor's kind;
* **`scripts/jdk-only-refusal-survivors.sh`** — the shipping registry, one
  booted VM under `--jdk-only`, diffed against
  `scripts/baselines/jdk-only-refusal-survivors.tsv`. Wired into the BLOCKING CI
  job beside the strict-corpus ratchet, not the advisory `jdk-only` job, which
  is `continue-on-error` at the job level.

The measured baseline is **exactly the five** this record triaged as the
species, and the fourth column is the sharpest confirmation §0's last paragraph
could have asked for:

```text
java/util/logging/Handler     getLevel            ()Ljava/util/logging/Level;   intrinsic
java/util/logging/Handler     setLevel            (Ljava/util/logging/Level;)V  intrinsic
java/util/logging/LogRecord   getLevel            ()Ljava/util/logging/Level;   intrinsic
java/util/logging/LogRecord   getMessage          ()Ljava/lang/String;          intrinsic
java/util/logging/LogRecord   getSequenceNumber   ()J                           intrinsic
```

**Five of five survivors are `Intrinsic`** — the one kind every §1.4 shadow
recorder skips. The four `lang_system.rs` rows are absent because they are not
refusals at all; the mechanical gate reproduces this record's hand triage
without being told it.

The baseline stores the survivor's KIND and not its `file:line`, on purpose: an
unrelated edit above the registration would move the line and fail a gate that
has nothing to say about it, while the kind is the half that decides whether any
other census can see the row.

### 7.5 What §7 does NOT claim

* **The five are still behaviourally equal**, which is why they are a frozen
  baseline and not a fix list. `handler_real_layout` and `log_record_real_layout`
  make both bodies layout-aware, so the two winners agree on a real JDK image.
  The gate exists for the SIXTH row, not for these.
* **The gate measures one boot of one main class.** A registrar reached only by
  a later, application-triggered path is outside it, exactly as it is outside
  the two censuses above. The script says so at its own top rather than leaving
  a reader to assume totality.
* **`register`'s other drop arms are still rowless.** `CRATONVM_NO_STUBS`,
  `real_net_sockets`, `real_forkjoinpool` and `drop_real_layout_synthetic` all
  still `return` without a census row. Only the `JdkOnly` arm was given a
  survivor, because only that arm is a POLICY whose whole promise is that the
  method falls through to real bytecode.
