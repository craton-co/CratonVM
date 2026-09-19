# W7-81 — a delegated write has three outcomes, and one `bool` was answering for all of them

**Status: source landed, UNVERIFIED against a VM.** Nothing here has been built
(this lane writes code, probes and docs; the orchestrator builds). What is
stated as measured was measured — on HotSpot 25.0.3.9 (Eclipse Adoptium), by
running `probes/CloseFlushSwallowProbe.java`. What is stated as read-from-source
was read from source. The two are kept apart on purpose.

> **Re-verified 2026-08-12 (lane A12), source-level, nothing run.** The landed
> source is present as described: `DelegatedWrite` with `classify` / `routed` /
> `delivered` and `classify_write_failure` in
> `native-api/src/print_error_state.rs`, re-exported from `native-api/src/lib.rs`;
> `routed()` is read at exactly **three** call sites in
> `native-builtins/src/logging_shims.rs`, matching the three bodies this record
> claims it changed (`native_printwriter_write_string`, `…_range`, and
> `native_printwriter_printf`'s retry gate). **Still UNVERIFIED against a VM** —
> `probes/CloseFlushSwallowProbe.java` has a measured HotSpot column and no
> CratonVM column, and the two console-echo tokens
> (`W781-IO-MUST-NOT-ECHO`, `W781-ERR-MUST-ECHO`) are the only observable of the
> behaviour change; nobody has read them off a CratonVM run in either mode.
> Disposition: **still open on verification, not on source.**

Ships the design W7-70-printstream-close-noop.md wrote down under
**`route_write_through_out`'s call sites — decided, and NOT threaded** and
deliberately did not ship, for a reason it stated plainly: shipping it blind is
how a fallback nobody remembered gets deleted. Builds on
W7-64-printstream-trouble-and-errormanager.md's `trouble` recording and on
W7-57-close-flush-swallow-sweep.md's absorb/propagate split; nothing from
either is reinvented.


> **VERIFIED AGAINST A BINARY 2026-09-03.** This record's disposition was
> "still open on verification, not on source", and named exactly one observable:
> the two console-echo tokens, "the only observable of the behaviour change",
> which "nobody has read off a CratonVM run in either mode". They have now been
> read, in all three modes, by counting occurrences in the process's stdout —
> one occurrence is the probe's own declaration line, two means the echo fired.
>
> ```text
>                              HotSpot   compatible   --jdk-only   --synthetic-jdk
> W781-IO-MUST-NOT-ECHO           1          1            1              1
> W781-ERR-MUST-ECHO              1          2            2              2
> ```
>
> Against this record's own before/after table: `W781-IO-MUST-NOT-ECHO` is
> **absent** (after: absent; HotSpot: absent) and `W781-ERR-MUST-ECHO` is
> **echoed** (after: echoed; HotSpot: absent, "the one outcome this VM cannot
> offer"). Both read the "after" column. The probe collects these and says it
> "cannot judge" them; the counts judge them.
>
> **The defect this section exists for is closed.** The probe's explicit
> equality — `routeBranchesAgreeOnError`, written because "one branch was fixed"
> and "both branches were fixed" look identical row by row — **PASSES** on
> CratonVM (`0/true` == `0/true`) in all three modes.
>
> The two `observed.psRouteErrorWriteOutcome` / `pwRouteErrorWriteOutcome`
> lines still differ from HotSpot (`none` vs the `Error`). Those are **PRINTED,
> NOT ASSERTED** by the probe, which names them a KEPT divergence and gives the
> reason at `CloseFlushSwallowProbe.java:1191-1199`: asserting HotSpot's value
> "would demand the change this lane declined to make". They are not counted as
> residuals here.
>
> **What this does NOT verify.** `psRouteErrorWriteDidNotRecordTrouble` and
> `pwRouteErrorWriteDidNotRecordTrouble` are `true` where HotSpot is `false`,
> in all three modes. HotSpot reaches neither, because it throws first — so its
> `false` is the absence of a decision, not a decision. Whether our `true` is
> wrong or is simply downstream of the declined change is **not settled here**;
> it is a hypothesis, and it is the residual this record leaves.
>
> Binary built from this tree at the merge of `origin/dev` on 2026-09-03;
> oracle Temurin 25.0.3+9-LTS on the same host. Reproduce:
> `javac -d /tmp/p probes/CloseFlushSwallowProbe.java && cd /tmp/p &&`
> `cratonvm --java-home $JAVA_HOME -cp . CloseFlushSwallowProbe | grep -c W781-ERR-MUST-ECHO`
## The defect

`route_write_through_out` returned one `bool`, and its two branches did not
agree about what that `bool` meant.

```rust
// char sink
if let Some(true) = sink_is_writer(ctx, out) {
    let text = String::from_utf8_lossy(bytes);
    return write_string_to_writer(ctx, this, out, &text);  // == record_write_failure(...)
}
// byte sink
let written = ctx.invoke_virtual(out, "write", "([BII)V", …);
record_write_failure(ctx, this, written);
true                                                        // unconditional
```

`record_write_failure` answers `true` only for a clean return. So:

| sink raised | char branch | byte branch |
|---|---|---|
| nothing | routed | routed |
| `IOException` | **not routed** — text echoed to the console fd | routed |
| `Error` / `RuntimeException` / `InternalError` | not routed — text echoed | **routed** — text vanishes |

Both rows in bold are wrong, and they are wrong in **opposite directions**:

* The char branch treated a failure HotSpot **handles** as a write that did not
  happen. HotSpot's `catch (IOException x) { trouble = true; }` discards the
  bytes — they are on no stream anywhere — so echoing them to the console is a
  **second write, to a stream HotSpot never touched**. Not a rescue: a
  divergence in the loudest possible place.
* The byte branch treated a failure HotSpot **propagates** as a write that
  succeeded. A `NoSuchMethodError` out of our own dispatch made the text vanish
  with no fallback at all — and "the console fallback is the picocli /
  JUnit-console survival path" was the stated reason the whole repair was
  deferred. That fallback did not exist on the byte branch.

## The three-way answer

`native-api/src/print_error_state.rs`:

```rust
pub enum DelegatedWrite { Delivered, Absorbed, Refused }

impl DelegatedWrite {
    pub fn classify(internal_error: bool, io: bool) -> DelegatedWrite {
        if io && !internal_error { DelegatedWrite::Absorbed } else { DelegatedWrite::Refused }
    }
    pub fn routed(self) -> bool {
        match self {
            DelegatedWrite::Delivered | DelegatedWrite::Absorbed => true,
            DelegatedWrite::Refused => false,
        }
    }
    pub fn delivered(self) -> bool { matches!(self, DelegatedWrite::Delivered) }
}
```

| delegated call returned | HotSpot | `DelegatedWrite` | routed? |
|---|---|---|---|
| cleanly | bytes delivered | `Delivered` | yes |
| `IOException` | `catch` runs, `trouble = true`, **bytes written nowhere** | `Absorbed` | yes — do not echo |
| `InterruptedIOException` | `catch` runs, interrupt re-asserted, bytes written nowhere | `Absorbed` | yes — do not echo |
| `Error` / `RuntimeException` | propagates out of `println` | `Refused` | **no — keep the console fallback** |
| `MethodCallFailed::InternalError` | not a Java throwable at all | `Refused` | **no** |

`classify_write_failure` runs the JDK's `catch` bodies exactly as W7-64 left
them (`InterruptedIOException` re-asserts the interrupt and does **not** set
`trouble`; any other `IOException` sets it; nothing else touches it) and returns
which of the three happened. `record_write_failure` is now
`classify_write_failure(…).delivered()` — byte-identical behaviour wherever it
is still used.

### A second site had the same conflation, found by grepping the predicate

Every call site of `record_write_failure` was read, not just the routing helper.
Five of the six discard the `bool` outright (`native_printwriter_write_string`,
`…_range`, `…_write_int` and the byte retry, all in
`native-builtins/src/logging_shims.rs`) — nothing to get wrong there.

**One gated on it**, in `native_printwriter_printf`:

```rust
let wrote_string = record_write_failure(ctx, this, wrote);
if !wrote_string && !backing_is_writer { /* retry via write([BII)V) */ }
```

The comment above it says the retry is for the case where `write(String)`
"isn't registered" — which is `Refused`, a `NoSuchMethodError` out of our own
dispatch. But `delivered()` is also `false` for an absorbed `IOException`, so a
`format` whose sink raised one **re-sent the same characters through the byte
overload on the same backing**: a double write on a sink the JDK had already
given up on, and the second attempt sets `trouble` a second time. Now gated on
`routed()`, so the retry fires only on the case its own comment names.

With that changed, `record_write_failure`'s `bool` is **read at no call site in
the workspace**. That is the cleanest statement that the conflation is gone: the
predicate that could not tell "HotSpot wrote it nowhere" from "the sink could
not take the call" no longer decides anything. It is kept rather than deleted
because it is a `native-api` public re-export and the recording it performs is
still wanted.

### Three more natives resolve `out` themselves and never reach the helper

A registration census over every triple that reaches `route_write_through_out`
turned up the thing that makes a fix in this family look landed and be inert:
`route_write_through_out` is **not** the only place that reads a
`PrintStream`/`PrintWriter` `out` and invokes `write` on it. Three natives in
`native-builtins/src/logging_shims.rs` resolve the backing writer themselves
(via `printwriter_get_backing_writer`, which is `route_write_through_out`'s
sink decision expressed a second time) and return before the shared path is
reached:

| native | had a fallthrough | now |
|---|---|---|
| `native_printwriter_write_string` | yes (`native_printstream_write_string`) | gated on `routed()` — a refusal falls through to the console fallback |
| `native_printwriter_write_string_range` | yes (`…_write_string_range`) | same |
| `native_printwriter_write_int` | **no** | unchanged — see below |

All three used to `return Ok(None)` unconditionally, which is the **byte
branch's** old bug on a different receiver shape: a `NoSuchMethodError` out of
our own dispatch made the text vanish with no fallback at all. The two with a
fallthrough now hand a `Refused` call to it; an `Absorbed` `IOException` still
returns, because HotSpot wrote the characters nowhere.

`native_printwriter_write_int` is **deliberately left**. It is the end of its
own path — there is nothing to fall through to — so giving it the fallback
means inventing a `stream_write` call for a single character, which is a
different change and needs its own justification. A `NoSuchMethodError` from
`out.write(int)` still loses that character silently. Named here rather than
guessed at.

The picocli / JUnit-console shape is unaffected by all of this and still runs
through `route_write_through_out`: `printwriter_get_backing_writer` excludes
`java/io/BufferedWriter`, and a `new PrintWriter(System.out)` backing *is* a
`BufferedWriter`, so it takes the fallthrough on the first line rather than the
failure path.

One further site mirrors the routing *predicate* without doing the routing:
`native-builtins/src/lang_misc.rs` (`printStackTrace`) repeats the null-`out`
test with a comment saying so. This branch does not touch that predicate — only
the classification of a failure after it — so that site still agrees.

The `InternalError` arm keeps W7-57's rule intact and states it in the code:
**`MethodCallFailed::InternalError` is not a Java throwable and can never be the
`IOException` a JDK `catch` names**, so absorbing it is never JDK parity —
whatever the `io` argument reads.

### Which branch is authoritative, and why it is not a single answer

Neither branch was right on both rows, so neither was adopted wholesale. **The
authority is per-outcome:**

* **On an absorbed `IOException`, the BYTE branch is authoritative** — it
  already declined to echo. The reason is HotSpot's, not a preference: the JDK
  catches the exception, sets `trouble`, and the bytes are gone. There is no
  second destination in HotSpot's version of this code, so producing one is a
  double write. This is the half that changes `System.out.println`'s behaviour.
* **On a refusal, the CHAR branch is authoritative** — it already fell back.
  Here HotSpot's answer is unavailable: it propagates, and these natives return
  `void` through helpers that cannot. Of the two answers that *are* available —
  drop the text silently, or print it to the console — **silence is the only one
  HotSpot never produces**. The fallback is louder than HotSpot; silence is
  wrong in a way that cannot be recovered from downstream.

Stated as one rule: **`routed` means "this VM must not write the bytes
anywhere else", and that is true both when the sink took them and when HotSpot
would have thrown them away.** It is false only when the sink could not take
the call at all.

### No signature moved, and the 23 call sites are a red herring

The handed-over framing was that this needed `?` threaded through
`stream_write`'s and `stream_writeln`'s call sites. **It needed none of it.**

The count is confirmed and is **23**: `stream_write(ctx, …)` has **12** (one in
`native-builtins/src/lib.rs`, eleven in `native-builtins/src/logging_shims.rs`)
and `stream_writeln(ctx, …)` has **11** (nine in `lib.rs`, two in
`logging_shims.rs`). "Ten" was a sample, as W7-70 said. All 23 sit in bodies
that already return `MethodCallResult`, so `?` would indeed drop straight in —
and **`?` is the wrong repair**, exactly as W7-70 argued: propagating the
`Error` deletes the fallback on the one case the fallback exists for.

The split needed instead is a **decision**, not an arity change: three inputs
mapped onto two outputs *differently*, inside one helper. Not one caller's
signature changed. `route_write_through_out` still returns `bool`; the `bool`
now means one thing in both branches.

The one place where the arity genuinely *is* the blocker is
`printwriter_autoflush_if_needed`, and it is deliberately **not** touched: it is
not a write, so it has no fallback destination, so "refused" and "absorbed" have
the same consequence there (nothing) and the only open question is propagation.
Its comment now says that instead of pointing at this one.

### The console fallback survives for `Error` — this is the load-bearing claim

Yes. `Refused` maps to `routed() == false`, which is the same `false` the char
branch has always returned, and all four callers
(`stream_write`, `stream_writeln_inner`, `native_printstream_write`,
`native_printstream_write_int`) still spell it
`if route_write_through_out(…) { return; } … stream_fd(…)`. The fallback path
is untouched.

**What it would break if removed:** the picocli / JUnit-console help-text path.
Those write through a `PrintWriter` whose sink is a char `Writer`; when a method
on that sink is missing, our dispatch raises `NoSuchMethodError`, and the
fallback is what still gets the help text to the console instead of producing a
silently empty `--help`. Removing it converts a visible registration gap into
invisible truncated output — the same "lost data reported as success" shape
W7-57 was chartered on, one layer up.

The change makes that fallback **wider**, not narrower: the byte branch now has
it too, where it previously reported every failure as routed.

## Prove the RED

`probes/CloseFlushSwallowProbe.java`, extended — not replaced. **119 printed
lines, 112 asserted**, `RESULT ok` on HotSpot 25.0.3.9 today (measured, this
lane). W7-57's 22, W7-64's 42 and W7-70's 29 are unchanged; **23 are new** — 19
asserted, 4 printed and not asserted.

The probe's three standing properties are kept.

**The effect must ARRIVE, never "the call returned".** Every row asserts where
the characters ended up (`TraceOut.sink`, `TraceWriter.received`) and what the
receiver recorded (`checkError()`), on both sinks, for all three outcomes:

| row (both `ps…` and `pw…`) | HotSpot | discriminates |
|---|---|---|
| `RouteCleanWriteReachedSink` = `hello` | delivered | a fix that stopped delivering |
| `RouteCleanWriteNoTrouble` = `false` | `false` | a fix that records success as failure |
| `RouteIoWriteReachedSinkBytes/Chars` = `0` | nothing written | the absorbed case delivering anyway |
| `RouteIoWriteRecordedTrouble` = `true` | `trouble` set | absorbing without recording |
| `RouteIoWriteThrewNothing` = `none` | absorbed | a blanket `?` that propagates the `IOException` |
| `RouteErrorWriteReachedSinkBytes/Chars` = `0` | nothing written | — |
| **`RouteErrorWriteDidNotRecordTrouble` = `false`** | `catch` never sees it | **the fallback being deleted** |

**Every recording row is paired with an over-correction guard on the same call
site.** `…IoWriteRecordedTrouble` (`true`) is paired with
`…ErrorWriteDidNotRecordTrouble` (`false`) on the identical construction, and
`…IoWriteThrewNothing` (`none`) guards the opposite over-correction on the same
call: a repair that satisfies one and not the other has gone wrong in one of the
two available directions.

`RouteErrorWriteDidNotRecordTrouble` is the row that matters most and it is
worth saying why it is not decorative. **The tempting way to collapse three
answers back into two is to widen `Absorbed` to cover an `Error`** — which is
the same edit as deleting the fallback, because `Absorbed` means `routed` means
"do not fall back". `classify_write_failure` sets `trouble` for everything it
calls `Absorbed`, so that widening sets `trouble` for a failure HotSpot's
`catch` never sees, and this row goes red. It is **the only in-process
observable that moves with the fallback.**

Three explicit agreement rows sit beside them, because *"one branch was fixed"
and "both branches were fixed" look identical row by row*:

* `routeBranchesAgreeOnCleanDelivery` = `hello`
* `routeBranchesAgreeOnIoException` = `0/true`
* `routeBranchesAgreeOnError` = `0/false`

**Rows that are mode- or platform-dependent are printed and NOT asserted, with
the reason named.** Four are new:

* `observed.psRouteErrorWriteOutcome`, `observed.pwRouteErrorWriteOutcome` —
  HotSpot `java.lang.Error: …`. This VM answers `none`, and that is a **kept**
  divergence: making the write natives propagate is the change W7-70 showed
  deletes the fallback. Asserting the HotSpot value would demand precisely what
  this lane declined to do.
* `observed.echoTokenThatMustNotAppear`, `observed.echoTokenThatMustAppearOnCratonVM`
  — see below.

### The honest limit: the console echo cannot be asserted from inside the JVM

**This is the finding that explains why W7-70 "could not measure" it, and it
should be read before anyone reports the probe as insufficient.**

The whole behaviour delta of this change lands on one thing: whether the caller
falls back to `stream_fd` and writes to the console file descriptor. That
fallback is `ctx.fd_table().write_string(fd, text)` — a **raw host write to fd 1
or 2**. It does not go through the `System.out` object, so `System.setOut` with
a capturing `PrintStream` does not see it; no Java code running in the process
can observe it.

Worse, the fallback is not even *reached* for an ordinary test sink:
`stream_fd` decides "is this a console stream" by walking field 0 up to four
hops looking for pointer identity with `System.out` / `System.err`, and a
`new PrintStream(new BoomOut(…))` chain leads nowhere. So for most
constructible receivers, "routed" and "not routed" have **identical** observable
consequences: nothing.

That is why the Java-visible rows above are green both before and after the fix,
and why the decision table is pinned in Rust instead — `native-api/src/print_error_state.rs`
grows four unit tests over `DelegatedWrite::classify` / `routed` / `delivered`,
naming what each answer is for:

* `an_absorbed_ioexception_is_routed_so_it_is_not_echoed`
* `an_error_is_refused_so_the_console_fallback_survives`
* `an_internal_error_is_refused_and_never_absorbed`
* `a_clean_write_is_the_only_delivered_answer` — which asserts
  `routed() != delivered()` for `Absorbed`, pinning apart the two meanings the
  single `bool` used to conflate.

`consoleEchoIsOutOfBand()` in the probe states the limit rather than papering
over it. It performs the two writes whose echo the change moves, through sinks
that chain `System.out` through slot 0 **on purpose** so the fallback has an fd
to find, and prints the two tokens:

| token | before | after | HotSpot |
|---|---|---|---|
| `W781-IO-MUST-NOT-ECHO` | echoed (char branch called the absorbed `IOException` "not routed") | absent | absent |
| `W781-ERR-MUST-ECHO` | absent (byte branch called the `Error` "routed" and the text vanished) | echoed | absent — HotSpot throws instead, the one outcome this VM cannot offer |

Whoever runs the probe reads the process's stdout for those tokens. It is
evidence the probe collects and cannot judge, and it is labelled as such: the
slot-0 chaining depends on this VM's field layout rather than on any Java
contract, which is a second reason nothing there is asserted. On HotSpot the two
sink classes are inert scaffolding and neither token is echoed — measured.

**Run it in BOTH modes.** `--real-jdk` and `--synthetic-jdk`, expecting
`RESULT ok` in both. A green Compatible-mode run is not evidence about the
synthetic natives and vice versa; W7-70's per-finding table still applies to its
own rows.

## Which registrar wins

**No registration was added, moved or removed by this branch.** The change is
entirely inside `route_write_through_out`, `write_string_to_writer`, four
`native-builtins/src/logging_shims.rs` bodies and `native-api`'s
`print_error_state`. No registration site was edited, so no `NativeKind` block
boundary moved and no `retired_shadow.rs` row is affected.

The census was run anyway, because a body-only edit still has to reach the body
that *wins*. Two registrars own every triple that reaches this code, and every
one of them was read:

| | `register_printstream_fallback_natives` | `register_synthetic_overrides` |
|---|---|---|
| file | `native-builtins/src/logging_shims.rs` (115–420) | `native-builtins/src/lib.rs` (21412–24192) |
| cfg gate | **none** — called unconditionally from `register_essential_natives_with_shims` | `#[cfg(feature = "synthetic-jdk")]`, also runtime-gated on `config.use_synthetic_jdk` |
| Compatible (`--real-jdk`) | **wins** | not called |
| synthetic | runs first | **wins** (runs after) |
| ambient `NativeKind` | `Bridge`, one block, no nested `set_category` | `Intrinsic`, one block, no nested `set_category` |

25 triples are registered by **both**, and in every case the two registrations
**name the same function**, so the ambient-kind split is a census tag and not a
behavioural one. Seven more are `register_printstream_fallback_natives`-only
(winner in both modes) and one — `java/io/PrintWriter.print(Ljava/lang/String;)V`
— is `register_synthetic_overrides`-only, so Compatible mode runs real bytecode
for it. Nothing else in the workspace registers any of them; the other hits on
those class names are `find()` lookups and constant-pool entries.

Because the edit is to shared **bodies** rather than to any registration, it
covers every winner in both modes by construction. The one thing that would have
made it inert is a second implementation of the same routing — and there was
one, which is why the three `native_printwriter_write*` natives above are part
of this branch rather than a footnote.

`route_write_through_out` itself is reached from exactly four native bodies —
`stream_write` and `stream_writeln_inner` in `native-builtins/src/lib.rs`,
`native_printstream_write` and `native_printstream_write_int` in
`native-builtins/src/logging_shims.rs` — plus, through `stream_writeln`, the
`emit_framework_log` helper, which is not itself a registered native but is
called from roughly twenty JUL / log4j / jboss-logmanager sites in
`native-builtins/src/logmanager.rs` and `phases_late.rs`. Those inherit the
change too.

## Compatible-mode justification

Compatible mode (`--real-jdk`) is contractually frozen except for genuine
HotSpot-parity fixes. **This is the widest-reach change in this family: it
changes what `System.out.println` does on every failure path.** Two change
classes touch Compatible mode; both are parity, and both name the HotSpot
behaviour they converge on.

1. **An absorbed `IOException` no longer echoes to the console.** HotSpot's
   `PrintStream.write(String)` / `write(byte[],int,int)` and their `PrintWriter`
   twins end `catch (IOException x) { trouble = true; }`, with no second write
   anywhere in the body. Writing the text to a console HotSpot never touched was
   the divergence; not writing it is parity. `trouble` is still set, so
   `checkError()` still reports it — the failure stays *observable*, it just
   stops being *duplicated*.
2. **A refused call now falls back instead of being reported as written.**
   HotSpot propagates an `Error` out of `println`. This VM cannot, and W7-70
   established that making it able to would delete the picocli fallback. Between
   the two reachable answers, the console fallback is the one that does not
   silently lose the text. This half only ever *adds* output where there was
   none.

Everything else is comment-only or Rust-internal (`DelegatedWrite`, the four
unit tests, the probe).

## Tests

No test was weakened.

* The four tests W7-57 tightened — `gzip_output_stream_p70`,
  `stream_handler_lifecycle_p61`, `object_output_stream_p70`,
  `pushback_reader_basics_p66` — assert that their sink slot is null. None is a
  `PrintStream` write path and none is touched.
* No test in `vm/src/vm/tests.rs`, `types/`, or `native-builtins/tests/` names
  `record_write_failure`, `classify_write_failure` or `DelegatedWrite`
  (grepped). `record_write_failure`'s behaviour is unchanged regardless.
* Four unit tests are **added**, in `native-api/src/print_error_state.rs`.
  `cargo check --all-targets` runs no tests, and `vm/src/vm/tests.rs` is
  synthetic-jdk-only, so these are placed where a plain `cargo test -p
  cratonvm-native-api` reaches them.
* No `CRATONVM_*` environment variable was added, so the four-part contract
  `cargo test -p cratonvm-types` enforces is not engaged.

## Blast radius

**Widest in this family.** Every `System.out.println` in every workload goes
through `stream_write` / `stream_writeln`, and both call
`route_write_through_out`. The behaviour only differs on a **failing** sink, but
"failing sink" includes every `NoSuchMethodError` our own dispatch raises inside
a redirected stream — which is not rare in this VM, and is the point.

**Highest — anything that redirects `System.out` / `System.err` to a Java
sink** (`System.setOut`, `TeePrintStream`, Surefire's `ForwardingPrintStream`,
Spring Boot's `OutputCaptureExtension`, DaCapo's `stdout.log` digest):

* A sink whose `write` raises an `IOException` **stops being echoed to the
  console.** Console output that was appearing twice — once nowhere, once on the
  terminal — now appears where HotSpot puts it, which is nowhere. Any log
  scraper or test assertion that has been reading the echo will stop seeing it.
  `checkError()` is unchanged and still reports the failure.
* A sink whose `write` raises an `Error` or `RuntimeException` **starts being
  echoed to the console**, where the text previously vanished. Expect *new*
  console output on byte sinks, each occurrence a dispatch gap this makes
  visible rather than a regression. Interleaving with other output changes.
* A `MethodCallFailed::InternalError` on a byte sink now also falls back rather
  than reporting success — same direction, same reasoning.

**Medium — anything that counts console lines or diffs stdout.** Both halves
above move bytes across the console boundary. Digest-checking workloads
(DaCapo's `stdout.log`) are sensitive to this in both directions.

**Medium — `PrintWriter.write(String)` / `write(String,int,int)` over a
non-`BufferedWriter` `Writer` backing** (`ModelNode.toString()` over a
`StringWriter`, Spring's response writers). A refused call now falls through to
the shared write path instead of returning silently, which means the text may
reach the console where it previously vanished — and `record_printed_line` fires
on that path, so `OutputCapture`-style collectors see it too. Failure path only;
a working sink is untouched.

**Medium — `PrintWriter.printf` / `format` over a byte backing.** The retry
through `write([BII)V` no longer fires on an absorbed `IOException`, only on a
refusal. Output that was being written twice to such a sink is written once;
output that was reaching it only via the retry is unaffected, because a
`NoSuchMethodError` is still a refusal.

**Low — `PrintWriter` char sinks.** Only the `IOException` half applies (the
char branch already fell back on an `Error`), and only where the receiver's
slot-0 chain reaches `System.out` / `System.err`; elsewhere the fallback had no
fd to find and did nothing either way.

**None.**

* The console streams themselves. `System.out` / `System.err` have a null `out`
  and return `false` from `route_write_through_out` before any of this is
  reached.
* `record_write_failure`'s six other call sites (unchanged semantics).
* `printwriter_autoflush_if_needed`, `native_printstream_flush`,
  `native_printstream_close`, and every W7-57 / W7-64 / W7-70 decision.
* The `trouble` recording rules, which are W7-64's unchanged.

### Suites the orchestrator must run

The family's prior lane named these and they are the right set, for the reason
above — all four are console-redirection-heavy:

* **Spring Boot (full)** — `OutputCaptureExtension` observes the `PrintStream`
  installed by `System.setOut`; both halves of this change land on it.
* **Tomcat** — `System.setOut` redirection into `catalina.out`.
* **Surefire / JUnit-console** — `ForwardingPrintStream`, and the picocli
  help-text path whose fallback this preserves. **Check `--help` output is not
  empty**; that is the fallback's own regression test.
* **DaCapo** — the `stdout.log` digest, which is byte-exact and is the workload
  the non-console arm of these natives was built for.

Run `probes/CloseFlushSwallowProbe.java` first, under both `--real-jdk` and
`--synthetic-jdk`, and grep the output for `W781-IO-MUST-NOT-ECHO` (must be
absent outside its `observed.` line) and `W781-ERR-MUST-ECHO` (must be present
outside its `observed.` line).

## What is left

* **The write natives still cannot propagate.** An `Error` out of a sink's
  `write` is absorbed here where HotSpot lets it out; the fallback makes it
  visible rather than fatal. Closing that needs the fallback replaced by
  something else first, and there is no candidate yet.
  `observed.psRouteErrorWriteOutcome` is the gap.
* **`printwriter_autoflush_if_needed` still swallows an `Error`**, and there it
  genuinely is only the 23-call-site arity.
* **`native_printwriter_write_int` still loses a refused character silently**,
  because it has no fallthrough to hand the refusal to. Named above.
* **`printwriter_get_backing_writer` is a second copy of
  `route_write_through_out`'s sink decision**, and `lang_misc.rs`'s
  `printStackTrace` is a third copy of its null-`out` predicate. All three agree
  today. Converting the idiom instead of the sites is the standing repair and
  is not attempted here.
* **The console echo has no in-process observable.** Any future change to this
  helper is measurable only by reading the process's stdout, which is why the
  decision table is pinned by Rust unit tests. If someone wants a real assertion,
  the thing to build is a `FdTable` that can be redirected to a buffer a probe
  can read back — that would make this whole family testable and does not exist
  today.
* Everything W7-70 left under **What is left** that this branch did not touch:
  the post-close write, Compatible-mode `checkError()`'s extra flush, the six
  unregistered `PrintStream` methods, `native_fos_close`, `addSuppressed`, and
  `java.io.PrintWriter` having no closed marker.
* Nothing in this record has run on a VM.
