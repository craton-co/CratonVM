# W7-64 — the absorbed error had a designated destination, and we dropped it

> **RUN 2026-08-12, IN ALL THREE ARMS INCLUDING `--synthetic-jdk`. This record's
> closing instruction — "the next lane with a build should run
> `probes/CloseFlushSwallowProbe.java` under `--real-jdk` and `--synthetic-jdk`
> and expect `RESULT ok` in both" — is now executed. It is not `ok` in either,
> and two rows this record reports as FIXED are measurably not.**
>
> Binaries: a default-feature release build dated 2026-08-12 15:27 for
> `--real-jdk`/`--jdk-only`, and a `--features synthetic-jdk` release build from
> clean `HEAD 2dbb9d451` (`/c/craton/synjdk-target`, built by the P4-B lane) for
> `--synthetic-jdk`. HotSpot 25.0.3+9 is `RESULT ok`.
>
> | arm | result |
> |---|---|
> | HotSpot 25.0.3+9 | `RESULT ok` |
> | CratonVM `--real-jdk` | `RESULT FAIL count=4` |
> | CratonVM `--jdk-only` | `RESULT FAIL count=3` |
> | CratonVM `--synthetic-jdk` | **the probe dies before its first check** |
>
> ```text
> --real-jdk   FAIL filterOutFlushFailureWins           expected <java.lang.Error: flush-boom> got <none>
> --real-jdk   FAIL streamHandlerHasADefaultErrorManager expected <true> got <false>
> --real-jdk   FAIL printStreamDeleteBlockedWhileOpen   expected <true> got <false>
> --real-jdk   FAIL printStreamCloseIoSinkTrace         expected <flush,close> got <flush,close,flush>
> --jdk-only   the same three, WITHOUT streamHandlerHasADefaultErrorManager
> ```
>
> ### 1. `Handler.errorManager` is STILL null in Compatible mode — the fix is inert
>
> This record states the null `errorManager` was "Fixed by reconstructing the
> third initializer". Measured, reading the field itself through
> `--add-opens java.logging/java.util.logging=ALL-UNNAMED`:
>
> | receiver | HotSpot | CratonVM `--real-jdk` | CratonVM `--jdk-only` |
> |---|---|---|---|
> | a user-defined `extends Handler` | `ErrorManager@…` | **null** | `ErrorManager@10a` |
> | `new StreamHandler()` | `ErrorManager@…` | **null** | `ErrorManager@10b` |
>
> The mode split is the diagnosis and it is decisive. `java/util/logging/Handler.<init>()V`
> is in `RETIRED_SHADOW_TRIPLES` (`native-api/src/retired_shadow.rs:334`), so
> under `--jdk-only` the refusal lets the **real constructor** run and the field
> comes out non-null — this VM demonstrably builds the right state when the
> shadow is out of the way. Under `--real-jdk` the native `<init>` runs and the
> reconstruction block does not fire.
>
> Two things checked so the next lane does not re-check them:
> **only one registrar holds the triple** (`reflect_annotations.rs:300`; grepped
> across all crates, no duplicate, so this is not a shadowed-loser), and
> **`resolve_field_index_by_class_id` does walk the hierarchy**
> (`vm/src/vm/vm_exec.rs:3695`, subclass → super), so the
> `has_error_manager_field` guard is not failing for the obvious reason that the
> receiver is a subclass. `new java.util.logging.ErrorManager()` from bytecode
> works in this arm. The remaining candidates are the guard's second half —
> `matches!(ctx.get_field_by_name(*this, "errorManager"), Value::Object(None))`,
> which is false for any zero-slot representation that is not literally
> `Object(None)` — and `ctx.new_object` returning a shape the `if let` drops.
> **Every one of those failure modes is silent**, which is why a source read
> concluded the fix had landed. NOMINATION 4 makes it not silent.
>
> ### 2. `--synthetic-jdk` runs at last, and cannot reach three of the five findings
>
> The **"Which arm COMPILES this"** section is right that three findings live
> only in the feature build, and its consequence — that no shipping-binary run
> discharges them — is confirmed. What it could not know is that the feature
> build does not discharge them either, because the probe dies first:
>
> ```text
> NoSuchMethodError java/io/PrintWriter.<init>(Ljava/io/Writer;)V
>   [class not found on any classpath entry — synthetic stub, add the missing jar]
>   caller="CloseFlushSwallowProbe.printWriterCloseIsNarrow()V @pc=25"
> ```
>
> That is this record's own observation about `PrintStream` — "`synthetic_stub_ctor_methods`
> mints only the two constructors" — biting on `PrintWriter`. `printWriterCloseIsNarrow`
> is the section carrying `printWriterCheckErrorAfterAbsorb`, i.e. the row the
> whole `PrintWriter.flush`/`close` recording finding rests on. **So finding 5
> (`PrintWriter.flush`/`close` recording) and finding 4 (the `StreamHandler`
> `ErrorManager` reports) are still unadjudicated — not for want of a build, but
> for want of a constructor.** Recorded precisely, because "never run" was the
> old blocker and it is no longer the true one.
>
> **Finding 3 IS adjudicated, and it is CLOSED.** `checkError`/`setError`/`clearError`
> raising `NoSuchMethodError` in synthetic mode is fixed: measured
> `System.out.checkError()` → `false` under `--synthetic-jdk`, matching HotSpot,
> and `System.err.checkError()` returns without throwing. The registrations at
> `native-builtins/src/lib.rs:23007`–`:23009` are live in that arm.
>
> **Finding 4's read side is measurably still missing**, by a probe that does not
> need `PrintWriter`: `new StreamHandler().getErrorManager()` under
> `--synthetic-jdk` raises
> `NoSuchMethodError: java.util.logging.StreamHandler.getErrorManager()Ljava/util/logging/ErrorManager;`.
> So `register_p61_handler_error_manager` registering `Handler.getErrorManager`
> does not serve a `StreamHandler` receiver in that arm. That is a registration-
> shape question (interface/superclass key vs. receiver class), not a missing
> body, and it is the next thing to look at there.
>
> ### 3. The modes fail on DISJOINT sets — do not generalise any row across them
>
> `filterOutFlushFailureWins` is **correct under `--synthetic-jdk`**
> (`java.lang.Error: flush-boom` propagates) and **wrong in both shipping
> modes** (`none`). That is the same disjointness the P4-B lane found campaign-
> wide. Concretely for this record: the "Run it in BOTH modes" table's per-finding
> arm column is necessary but not sufficient — a row can be green in the arm the
> table points at and red in the arm it does not.
>
> ### 4. Two open items confirmed still open, one of them now with a measurement
>
> * **"An `Error` on the write paths is still absorbed."** Confirmed, and it is
>   the *flush* path that the probe catches: `filterOutFlushFailureWins` expects
>   the `Error` and gets `none` in both shipping modes.
> * **`printStreamCloseIoSinkTrace expected <flush,close> got <flush,close,flush>`**
>   — an extra flush after close, in both shipping modes. This is the shape of
>   the last open item ("`checkError()` will flush, absorb, record") landing on
>   the `PrintStream` close path rather than only on `PrintWriter`. W7-70 gave
>   `PrintStream` a `closing` latch; whatever consults it is not consulting it
>   here.
> * `printStreamDeleteBlockedWhileOpen expected <true> got <false>` in both
>   shipping modes — the probe's file is deletable while the `PrintStream` over
>   it is open, where HotSpot on Windows refuses. Adjacent to W7-70's close work
>   and not previously recorded here; the mechanism was not investigated, so it
>   is stated as the observation and nothing more.
>
> Nothing above was rebuilt either; every claim in this block is a run of an
> existing binary or a read of the tree, and the two are kept apart as this
> record's own preamble asks.

**Status: source landed, UNVERIFIED against a VM.** Nothing here has been
built (this lane writes code and docs; the orchestrator builds). What is
stated as measured was measured — on HotSpot 25.0.3.9 (Eclipse Adoptium), by
running Java, or by reading `lib/src.zip` from that same image. What is stated
as read-from-source was read from source. The two are kept apart on purpose.

Closes the two residuals W7-57-close-flush-swallow-sweep.md named by name:
`trouble` / `checkError()`, and `ErrorManager`.

> **ADJUDICATED 2026-08-12 — this record OVERSTATED what is open, and it did not
> say which of its rows a shipping binary contains.** Two of the five items
> under **What is left** are CLOSED IN SOURCE and are struck there; the
> remainder is restated. Separately, a new **Which arm compiles this** section
> below establishes what no earlier pass in this record did: three of its five
> findings are behind `#[cfg(feature = "synthetic-jdk")]`, and that feature is
> **not** in the default feature set (`vm/Cargo.toml`: "intentionally NOT in
> the default feature set"), so **the default `cratonvm-cli` binary — the only
> build that serves `--real-jdk` and `--jdk-only` — never compiled them.** A
> green run of either shipping mode is therefore evidence about two rows out of
> five, not about this record. Everything verified by reading the tree; nothing
> here was built or run either.

## The species

`java.io.PrintStream` and `java.io.PrintWriter` never throw from
`print`/`write`/`flush`/`close`. `java.util.logging.Handler` never throws from
`publish`/`flush`/`close`. Both absorb — and W7-57 was right to keep those
swallows. But neither *discards*. The `catch` clause has a **body**:

| JDK class | the `catch` | the body |
|---|---|---|
| `PrintStream`, `PrintWriter` | `catch (IOException x)` | `trouble = true;` — read back by `checkError()` |
| `PrintStream`, `PrintWriter` (write paths only) | `catch (InterruptedIOException x)` | `Thread.currentThread().interrupt();` — and **not** `trouble` |
| `java.util.logging.Handler` family | `catch (Exception ex)` | `reportError(null, ex, ErrorManager.<CODE>)` |

W7-57 fixed which throwables the `catch` swallows. This lane runs its body. A
native that absorbs without recording has not matched the JDK: it has
converted a reportable failure into complete silence, and `checkError()`
answers `false` forever. That is **strictly worse** than the un-narrowed
swallow W7-57 removed, because an unthrown-but-recorded failure is still
discoverable and an unthrown-and-unrecorded one is not.

The split lives in `native-api/src/print_error_state.rs`, deliberately a
sibling of `delegated_close` rather than an extension of it: that module
decides *which* throwables a JDK `catch` names, this one runs the body of the
same `catch`. Keeping them apart is what makes the `InterruptedIOException`
clause expressible — it is a different `catch` on the same `try`, and it does
**not** set `trouble`.

## The census, and why the handed-over count was a sample

W7-57 handed over "9 sites narrowed, `trouble` not set at any of them". The
population of natives standing in for a JDK **absorb-and-record** method is
larger, because W7-57 counted only `close`/`flush` delegations and the record
is set on the **write** paths too — every `print`, `println`, `printf`,
`format`, `append` and `write` overload on both classes.

Counted by walking every registration of `java/io/PrintStream`,
`java/io/PrintWriter` and the `java.util.logging.Handler` family in
`native-builtins`, `native-io`, `native-api`, `native-collections`,
`native-awt` and `vm`, then reading each distinct native body.

**81 registrations across 5 registrars. 24 distinct native bodies stand in for
a JDK method whose spec is absorb-and-record.** They funnel into **13 distinct
absorb sites**. The split before this lane:

| | absorbs | records | sites |
|---|---|---|---|
| absorbs and now records | yes | **no → yes** | **11** |
| absorbs, correctly records nothing | yes | n/a (HotSpot makes no such call) | 2 |
| **read side did not exist at all** | — | — | **3 methods** |

The 11:

| # | file | native / helper | JDK body it stands in for | was | now |
|---|---|---|---|---|---|
| 1 | lib.rs | `native_printwriter_flush` | `PrintWriter.flush` | `absorb_io_exception` | `absorb_io_exception_recording` |
| 2 | lib.rs | `native_printwriter_close` (sink flush) | ours, `PrintWriter.close` policy | same | same |
| 3 | lib.rs | `native_printwriter_close` (sink close) | `PrintWriter.close` | same | same |
| 4 | logging_shims.rs | `native_printstream_flush` (Java sink) | `PrintStream.flush` | same | same |
| 5 | logging_shims.rs | `native_printstream_flush` (fd) | same | `let _ =` on `fd_table().flush` | `record_host_io_failure` |
| 6 | lib.rs | `route_write_through_out` (byte `write([BII)V`) | `PrintStream.write(byte[],int,int)` and every private `write`/`writeln` | `let _ =` | `record_write_failure` |
| 7 | lib.rs | `write_string_to_writer` | `PrintWriter.write(String,int,int)` | `.is_ok()` | `record_write_failure` |
| 8 | lib.rs | `stream_write` / `stream_writeln_inner` (fd) | the same private `write`/`writeln`, for a console stream | `let _ =` | `record_host_io_failure` |
| 9 | logging_shims.rs | `native_printwriter_write_string` (both branches) | `PrintWriter.write(String)` | `let _ =` | `record_write_failure` |
| 10 | logging_shims.rs | `native_printwriter_write_string_range`, `native_printwriter_write_int` | `PrintWriter.write(String,II)` / `write(int)` | `let _ =` | `record_write_failure` |
| 11 | logging_shims.rs | `native_printwriter_printf` (String branch and byte branch) | `PrintWriter.format` | `.is_ok()` and `let _ =` | `record_write_failure` |

Plus the `Handler` family, whose record is an `ErrorManager` call rather than
a field:

| # | file | native | JDK code | was | now |
|---|---|---|---|---|---|
| 12 | phases_late.rs | `StreamHandler.flush` | `FLUSH_FAILURE` (2) | dropped | `reportError` |
| 13 | phases_late.rs | `StreamHandler.close` | `CLOSE_FAILURE` (3) | dropped | `reportError` |
| 14 | phases_late.rs | `StreamHandler.publish` | `WRITE_FAILURE` (1) | dropped, per byte | `reportError`, once |

The 2 that correctly record nothing are `logmanager.rs`'s
`publish_to_jul_handlers_full` / `publish_existing_record_to_jul_handlers`
handler flushes: HotSpot's `Logger.log` makes no `Handler.flush()` call at
all, so there is no `ErrorManager` report to copy either. `vm_only_best_effort`
stays exactly as W7-57 left it.

**The rest of the print surface already funnels correctly.** `native_printf`,
`native_printf_locale`, `native_printstream_append`,
`native_printstream_write_string`, `native_printstream_write_string_range`,
`native_println_*` and `native_print_*` all reach `stream_write` /
`stream_writeln`, so sites 6–8 cover them. That was established by reading
each body, not by assuming the funnel.

## Does one native back both `PrintStream` and `PrintWriter`? Yes — five of them

This is the shape that was just found wrong across 18 `StrictMath` methods, so
it was checked rather than assumed. Five bodies are registered under **both**
class names:

`native_println_string`, `native_println_void`, `native_println_int`,
`native_println_object`, `native_print_string`.

**Sharing them is correct**, and for a reason worth writing down: they share
only the *text formatting*, and hand off to `stream_write` / `stream_writeln`,
which branch on the receiver — `route_write_through_out` reads `this.out`,
`sink_is_writer` decides char-vs-byte, and `printwriter_autoflush_if_needed`
fires only for a plain `java.io.PrintWriter`. The recording added here is by
field **name** on `this`, and both classes declare `trouble`, so it is correct
on both sides of the shared body with no branch at all.

The one place the two classes genuinely differ is `checkError()`, and the new
`printstream_check_error` is shared **only because it tests both delegation
branches in the JDK's order** — see below. That is stated at the registration
site so the next reader does not have to re-derive it.

## `checkError`'s flush was also wrong — it did not exist

The task asked whether `checkError` only reads a flag. Read from
`lib/src.zip`, JDK 25.0.3.9:

```java
// PrintStream
public boolean checkError() {
    if (out != null) flush();
    if (out instanceof PrintStream ps) return ps.checkError();
    return trouble;
}
// PrintWriter
public boolean checkError() {
    if (out != null) flush();
    if (out instanceof PrintWriter pw) return pw.checkError();
    else if (psOut != null) return psOut.checkError();
    return trouble;
}
```

Measured on HotSpot 25.0.3.9: a `PrintStream` that has **never** failed, over
a sink whose `flush()` throws `IOException`, answers `checkError() == true` on
the first call, and the sink records that the flush was attempted
(`psFlushNotYetAttempted=false`, `psCheckErrorItselfFlushed=true`,
`psFlushAttemptedByCheckError=true`). Same for `PrintWriter`. So an
implementation that reads the stored flag and nothing else is wrong even after
the recording is fixed.

**Per arm:**

* **Compatible mode** — `checkError`, `setError` and `clearError` are not
  registered as natives anywhere, so the real `java.io` bytecode runs, and it
  is correct: it flushes (dispatching to our `flush` native, which now
  records) and returns the real `trouble` field. **Nothing was changed on the
  read side in Compatible mode, deliberately** — shadowing correct real
  bytecode with a native is the contract-1.4 shadow this workspace has been
  retiring all week.
* **Synthetic-jdk mode** — all three raise
  `NoSuchMethodError: java/io/PrintStream.checkError()Z`. Traced: the native
  registry is consulted by `(class, method, descriptor)` **before** any
  bytecode resolution (`invoke.rs`'s `try_stackless_invoke` step 1 →
  `native_override.rs`'s `resolve_step1_native`), no registrar names the
  triple, and `synthetic_stub_ctor_methods("java/io/PrintStream")` mints only
  the two constructors — so the miss falls all the way to
  `vm_exec.rs`'s terminal `LinkageError::NoSuchMethodError`. The read side did
  not answer `false`; it did not exist. All three are now registered from
  `register_synthetic_overrides`, i.e. the synthetic arm alone.

`RJdkHello`'s "PrintStream reported an error" is the reason this was not
obvious from a run: that failure came out of a **real** `PrintStream` in
Compatible mode, where `checkError()` is real bytecode.

## Where the flag lives

`print_error_state` resolves `trouble` **by name**, which lands on the real
slot in Compatible mode and needs a slot to exist in the synthetic one. The
synthetic `java/io/PrintStream` | `java/io/PrintWriter` model grows one:

```rust
"java/io/PrintStream" | "java/io/PrintWriter" => {
    let mut fields = instance_fields(1);
    fields.push(named_field("trouble", "Z"));
    fields
}
```

Three things about that, all deliberate:

* It is spelled with the JDK's own name rather than `_vmN`, because it **is**
  the JDK's field, not a VM-internal slot. `vm_internal_field`'s doc reserves
  `_vmN` for "a slot this VM parks its OWN value in, for which the real JDK
  class declares no field", which is not this.
* Its **index** is not the real image's — the real `java.io.PrintStream`
  declares `trouble` at absolute 4, behind
  `out`/`closed`/`closeLock`/`autoFlush`. `shadow_layout`'s
  `diff_against_model` is right to report that under `CRATONVM_DBG_OVERLAY`,
  and it is harmless because **nothing addresses `trouble` positionally**:
  every reader and writer goes through `print_error_state`, which resolves by
  name. The report is a true statement about a real divergence, so it is left
  to fire rather than suppressed.
* It breaks no test. Checked: no `num_total_fields` assertion in
  `vm/src/vm/tests.rs`, `classloading/`, `native-builtins/tests/` or
  `vm/tests/` names either class; `shadow_layout.rs`'s two exact-name-list
  tests list neither; `SAFE_POSITIONAL_CLAIMS` has no row for either;
  `t9c_synthetic_field_tables_cover_their_factories` asserts
  `declared >= requested` and the only factory requests 1. The real class's
  padding floor is a `max()`, so it is unaffected.

## `ErrorManager` — two defects, one per arm

**Synthetic:** `StreamHandler.flush`/`close`/`publish` absorbed the `Exception`
and dropped it. They now take the absorbed throwable and run the JDK's own
call, `reportError(null, ex, code)`. `Handler.reportError`,
`Handler.getErrorManager`, `Handler.setErrorManager` and the default
`ErrorManager.error` did not exist in that arm at all and are registered from
`register_p61_handler_error_manager` — synthetic-only, because
`register_p61_logging` → `register_phase61_natives` →
`register_synthetic_overrides`. State lives in `logging_shims`' identity-hash
side table (the `jul_logger_handlers_table` pattern), not a field slot: the
synthetic `StreamHandler` is a 2-field object with no room, and a raw slot 2
on a real-JDK `Handler` lands on `formatter`/`logLevel` — the exact trap
`register_p61_file_handler`'s doc comment already records for `FileHandler`.

**Compatible:** a different defect, found while checking whether the arm
needed anything. `java/util/logging/Handler.<init>()V` **is** shadowed by a
native (`reflect_annotations::register_annotation_overrides`, which runs from
`register_essential_natives`, i.e. both modes). A native constructor replaces
the real one wholesale, so **none** of `Handler`'s field initializers run. Two
of the three were reconstructed by hand in that native (`logLevel`, `filter`);
the third,

```java
private volatile ErrorManager errorManager = new ErrorManager();
```

was not — so `errorManager` was **null on every `Handler`** in Compatible
mode. `Handler.reportError` dereferences it unguarded, so every absorbed
`Exception` in the whole `Handler` family NPE'd inside the reporting path and
came out as `reportError`'s own `catch (Exception ex2)` message
("Handler.reportError caught:") instead of the failure. HotSpot's is never
null — its javadoc promises a default is installed, and the probe asserts
`new StreamHandler().getErrorManager() != null`. Fixed by reconstructing the
third initializer, guarded on the field **existing** and being null so neither
a real constructor nor a prior `setErrorManager` is clobbered.

The shape is one the JUL lanes have hit repeatedly this week: a library's own
fallback — here `reportError`'s `catch (Exception ex2)` — turning a null field
into a symptom that looks like something else entirely. It is also the second
time in this record that a native `<init>` reconstructing *some* of a class's
field initializers is the defect, which is worth stating as a rule: a native
constructor owes the class every initializer, not the ones the feature that
prompted it happened to need.

## Measured, on HotSpot 25.0.3.9

Every row below was run before it was written down, and every one is now an
assertion in `probes/CloseFlushSwallowProbe.java`.

| observation | HotSpot |
|---|---|
| `PrintWriter.close()` absorbing an `IOException` → `checkError()` | `true` |
| the same `close()` raising an `Error` → propagates, `checkError()` | `false` |
| `PrintStream.flush()` absorbing an `IOException` → `checkError()` | `true` |
| `print` / `println` / `write(int)` over a failing sink → thrown / `checkError()` | `none` / `true` |
| `checkError()` on a never-failed stream whose flush throws | `true`, and the flush was attempted |
| `PrintWriter` over a failing `PrintStream` → outer `checkError()` | `true` (the `psOut` branch) |
| healthy stream / writer after print+println+flush | `checkError() == false` |
| `PrintWriter` after a CLEAN close over a `StringWriter` | `false` |
| `setError()` / `clearError()` on a subclass | `true` / `false` |
| `StreamHandler.flush` over a failing sink → `ErrorManager` | code `2`, `java.io.IOException: sh-flush-io` |
| `StreamHandler.close` over a failing sink → `ErrorManager` | code `3`, the same `IOException` |
| `StreamHandler.close` raising an `Error` → `ErrorManager` | **never called**; the `Error` propagates |
| a healthy `StreamHandler` after publish+flush+close | `ErrorManager` never called |

## Prove the RED

`probes/CloseFlushSwallowProbe.java`, extended — not replaced. **64 printed
lines, all 64 asserted**, `RESULT ok` on HotSpot 25.0.3.9 today. W7-57's 22
checks are unchanged; `observed.printWriterCheckErrorAfterAbsorb` is now the
assertion `printWriterCheckErrorAfterAbsorb`.

Both of W7-57's properties are kept:

* **the assertion is that the failure is recorded**, never that the call
  returned. A check that passed because nothing threw is the defect's own
  shape, and so is a check that passed because `checkError()` happened to
  return the value the test wanted for an unrelated reason — which is why
  every recording row is paired with a healthy-stream row on the same class.
* **over-correction guards on the same call sites.** A repair can go wrong in
  two directions and only one of them is the one being fixed:

| guard | HotSpot | guards against |
|---|---|---|
| `printWriterCheckErrorAfterPropagatedError` | `false` | setting `trouble` for every absorbed failure, or for a propagated one |
| `printStreamCheckErrorOnHealthyStream` | `false` | setting `trouble` on a clean return — turning `checkError()` into a constant `true` |
| `printWriterCheckErrorOnHealthyWriter` | `false` | same, on the other class |
| `printWriterCheckErrorAfterCleanClose` | `false` | a close that records because it closed |
| `streamHandlerErrorManagerNotCalledForError` | `0` calls | widening `catch (Exception)` to `Throwable` on the reporting path |
| `streamHandlerHealthyReportsNothing` | `0` calls | reporting on every delegation |
| `printStreamHealthyStreamGotItsBytes` / `printWriterHealthyWriterGotItsText` | `true` | a recording path that swallowed the write |

The `ErrorManager` codes are compared against the literals `2` and `3` rather
than `ErrorManager.FLUSH_FAILURE` / `CLOSE_FAILURE`: those are
`public static final int` on a class the synthetic arm fabricates without a
static field table, so reading them would make the probe red for a reason that
is not the one being probed. That gap is named under **What is left**.

**Run it in BOTH modes.** Per finding:

| finding | arm |
|---|---|
| `trouble` not set on flush/close/write | **both** — the `PrintStream.flush` and shared write funnel are registered from `register_essential_natives` |
| `PrintWriter.flush`/`close` not recording | **synthetic only** — those two triples are registered from `register_synthetic_overrides` alone; Compatible runs the real bytecode |
| `checkError`/`setError`/`clearError` raise `NoSuchMethodError` | **synthetic only** |
| `StreamHandler.flush`/`close`/`publish` drop the `ErrorManager` report | **synthetic only** |
| `Handler.errorManager` left null by the native `<init>` | **Compatible only** (in the synthetic arm the field does not exist and the side table serves instead) |

A green Compatible-mode run is therefore not evidence about four of those five.

## Compatible-mode justification, per change class

Compatible mode is contractually frozen except for genuine HotSpot-parity bug
fixes. Two change classes touch it, and both are parity:

1. **Recording an absorbed `IOException` in `trouble`** (sites 4–11 that are
   reachable in Compatible mode: `native_printstream_flush`,
   `route_write_through_out`, `write_string_to_writer`, `stream_write`,
   `stream_writeln_inner`, `native_printstream_write{,_int}`, the three
   `native_printwriter_write_*`, `native_printwriter_printf`). Each stands in
   for a JDK body whose `catch (IOException x)` clause is literally
   `trouble = true;`, quoted per site in the code. HotSpot sets the flag at
   exactly that point; we did not.
2. **`Handler.<init>` reconstructing its third field initializer.** HotSpot's
   `errorManager` is never null. Ours was. The guard means the change is a
   no-op on any handler whose real constructor did run.

Everything else — `checkError`/`setError`/`clearError`, the `ErrorManager`
surface, the synthetic `trouble` slot — is registered from a synthetic-only
registrar or is a synthetic-only layout, and Compatible mode does not reach it.

## Blast radius

Narrower than W7-57's, because **nothing here changes what propagates**. The
`Error`-escapes decisions are exactly as W7-57 left them; this lane only adds
a field write and an `ErrorManager` call on paths that were already absorbing.

**Medium — code that reads `checkError()` and acts on it.**

* Anything that has been getting `false` from `checkError()` on a stream that
  really did fail now gets `true`. That is the fix, and it is also the risk:
  a caller that treats `checkError()` as an abort signal will start aborting
  where it silently continued. **Suites to run: Spring Boot (full), Tomcat**
  — `System.out`/`System.err` are fd-backed there and site 8 makes a failing
  host write set the flag.
* `RJdkHello` asserts on `checkError()` directly and is the one named
  regression risk: a separate lane is repairing `native_osw_init` /
  `native_bw_init` so that a real `PrintStream`'s `charOut`/`textOut` are
  usable. **This lane must not be measured before that one lands** — until it
  does, `RJdkHello`'s `trouble` is set by that defect, not by anything here,
  and nothing in this change makes it better or worse.

**Low — the `ErrorManager` surface.**

* Synthetic-mode `StreamHandler` failures now print
  `java.util.logging.ErrorManager: <code>` and one stack trace to
  `System.err`, once per `ErrorManager` instance. That is HotSpot's own
  output. A test asserting on *empty* stderr around a failing log handler will
  see it; a test asserting on stderr *content* was already seeing the JDK's
  version of it in Compatible mode.
* Compatible-mode `Handler.reportError` stops NPEing and starts delivering.
  **Run the logging suites** (`RJdkLogging`, Tomcat JULI, Spring Boot logging).

**None.**

* The synthetic `trouble` slot (additive, no test reads the field count).
* `checkError`/`setError`/`clearError` in Compatible mode (untouched).
* Every `delegated_close` decision from W7-57 (untouched).

## Which arm COMPILES this (added 2026-08-12)

The "Run it in BOTH modes" table above says which *mode* each finding lives in.
It does not say which *build* contains the code, and for three of the five that
is the load-bearing fact. Read from the tree:

* `register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`
  (`native-builtins/src/lib.rs:21525`–`:21526`), and so is its only caller
  `register_builtins` (`:21514`–`:21515`). `synthetic-jdk` is **not** a default
  feature (`vm/Cargo.toml`, and `vm-cli`'s is a pass-through). So everything
  reachable only from that registrar is **absent from the default binary's
  object code**, not merely unreached at run time.

| finding | registrar | in the DEFAULT build? |
|---|---|---|
| `trouble` not set on flush / the shared write funnel (sites 4–11 that are Compatible-reachable) | `register_printstream_fallback_natives`, called from `register_essential_natives_with_shims` (`lib.rs:7103` → `:17300`) | **yes** — live in `--real-jdk` and `--jdk-only` |
| `Handler.errorManager` left null by the native `<init>` | `reflect_annotations::register_annotation_overrides` ← `register_essential_natives` | **yes** |
| `checkError` / `setError` / `clearError` | `lib.rs:22979`–`:22981`, inside `register_synthetic_overrides` | **no — not compiled** |
| `StreamHandler` `flush`/`close`/`publish` → `ErrorManager`, and the `Handler.reportError` / `get`/`setErrorManager` / `ErrorManager.error` surface | `register_p61_handler_error_manager` (`phases_late.rs:3276`) ← `register_p61_logging` (`:2858`) ← `register_phase61_natives` (`:2836`) ← `lib.rs:24027`, inside `register_synthetic_overrides` | **no — not compiled** |
| `PrintWriter.flush` / `close` recording | `register_synthetic_overrides` alone | **no — not compiled** |

The synthetic `trouble` slot is the one row that does not split this way:
`synthetic_stub_fields` (`classloading/src/class_manager.rs:11615`) carries no
`cfg`, so it compiles into the default build — but it only shapes *fabricated*
classes, which that build never mints for `java/io/PrintStream`, so it is inert
there rather than absent. **Its quoted arm above is now STALE**: W7-70 split
the shared `"java/io/PrintStream" | "java/io/PrintWriter"` arm in two and gave
`PrintStream` a second named field, `closing` (`:12266`–`:12277`).
`PrintWriter` still gets only `trouble`, deliberately — it has no `closing` in
the real image and uses `out == null` as its closed marker, which is the same
fact the last open item below rests on.

**Consequence for the run list.** Two of the five findings can be exercised by
the shipping binary; the other three need a `--features synthetic-jdk` build run
in `--synthetic-jdk` **mode**, which is the configuration README §2.6 records as
never having been run at all. Feature is not mode, and here it is also not
*presence*.

## What is left

* ~~**`native_printstream_close` is a no-op, for every `PrintStream`.**~~
  **CLOSED IN SOURCE 2026-08-12 by W7-70-printstream-close-noop.md**, and
  re-verified from the tree rather than from that record: `native_printstream_close`
  (`native-builtins/src/logging_shims.rs:1245`) now performs the receiver test
  its old comment was a reason for — a `closing` latch read through
  `print_error_state::is_closing`, then the sink resolved **by the JDK's own
  field name** `out`, with the no-op kept only for the console case where `out`
  is not an object. The paragraph below is kept as written and struck by this
  note; the reason it gave for there being no
  `printStreamCheckErrorAfterAbsorbedClose` row in the probe no longer holds
  either.
  *Original text, struck:* ~~Not just
  the console ones — the triple is registered unconditionally in both
  registrars, so `new PrintStream(fileOutputStream).close()` does not close
  the file and does not run HotSpot's `textOut.close(); out.close();`. Its
  comment says "Don't actually close stdout/stderr", which is a correct reason
  for a receiver test it does not perform. That is a bigger and different
  defect than this lane's and it needs its own measurement (the fd-backed
  `System.out`/`System.err` really must survive a `close()`), so it is
  recorded rather than half-fixed. It is also why there is no
  `printStreamCheckErrorAfterAbsorbedClose` row in the probe: the call this
  VM makes there is not the call HotSpot makes.~~
* **An `Error` on the write paths is still absorbed** where HotSpot lets it
  out. `route_write_through_out` returns `bool` into `stream_write` /
  `stream_writeln`, which return `()` across ten call sites; propagating is a
  signature change, not a one-line fix. `record_write_failure`'s doc comment
  carries the same statement at the code. Unchanged from W7-57, which recorded
  it first. **STILL OPEN, verified 2026-08-12, and its SHAPE changed**: the
  helper still returns `bool` (`native-builtins/src/lib.rs:26525`) and
  `stream_write` still returns `()`, so nothing propagates — but the text no
  longer vanishes. W7-81 made an `Error` the `Refused` outcome, which is
  reported as NOT routed, so the caller's fd fast path prints it. Wrong stream,
  not lost data.
* ~~**`route_write_through_out` still reports a FAILED write as "not routed"**,
  which sends the caller to the fd fast path and prints the text to the
  console. HotSpot writes nowhere in that case. Left alone because the console
  fallback is what keeps output flowing when `out` is a `Writer` shape the
  helper cannot address (the picocli / JUnit-console `NoSuchMethodError`
  case), and separating those two reasons needs a measurement this lane did
  not take.~~
  **CLOSED IN SOURCE 2026-08-12 by W7-81-write-route-three-way.md**, and
  re-verified from the tree: `print_error_state::DelegatedWrite`
  (`native-api/src/print_error_state.rs:221`) splits `Delivered` / `Absorbed` /
  `Refused`, and **both** branches of `route_write_through_out` now end in
  `classify_write_failure(ctx, this, written).routed()` — the char branch
  through `write_string_to_writer`, the byte branch inline. An absorbed
  `IOException` is ROUTED, so the console echo HotSpot never makes is gone;
  only a `Refused` call falls back. This item is exactly the "separating those
  two reasons needs a measurement this lane did not take" that W7-81 took.
* **`ErrorManager`'s six `public static final int` codes do not resolve under
  `--synthetic-jdk`.** A synthetic class has no static field table to put them
  in. The codes this VM *passes* are correct; the probe compares literals for
  exactly that reason.
* **`PrintWriter.checkError()` after a close, in synthetic mode, over a
  wrapper sink.** HotSpot reaches `false` by nulling `out` so `checkError()`
  skips its flush; `native_printwriter_close` deliberately does **not** null
  `out` (a null `out` is our "this is a console stream" marker and would
  redirect a closed writer's output to stdout). So over a sink whose
  `flush()`-after-`close()` throws — `BufferedWriter`, `OutputStreamWriter` —
  our `checkError()` will flush, absorb, record, and answer `true` where
  HotSpot answers `false`. The probe's clean-close row uses a `StringWriter`,
  whose `flush()` after `close()` is a no-op, so it is stable in both arms and
  does not paper over this. Closing it needs a closed-marker that is not
  `out == null`. **STILL OPEN, verified 2026-08-12, and the marker now exists
  on the other class only**: W7-70 added a named `closing` field to the
  synthetic `java/io/PrintStream` model and split the arm it used to share with
  `java/io/PrintWriter` (`classloading/src/class_manager.rs:12266`–`:12277`),
  deliberately leaving `PrintWriter` with `trouble` alone because the real
  image declares no `closing` on it. So the closed-marker this item asks for is
  a `PrintWriter`-shaped decision that W7-70 declined to make, not an absent
  mechanism.
* **`InterruptedIOException` is handled in `print_error_state`
  (`absorb_write_exception_recording` / `record_write_failure`) but not
  asserted in the probe.** Producing one from a `Runnable`-shaped check needs
  a thread that is actually blocked in I/O, which is a different fixture than
  everything else here.
* **`addSuppressed`** — still open from W7-57, untouched.
* Nothing in this record has run on a VM. The next lane with a build should
  run `probes/CloseFlushSwallowProbe.java` under `--real-jdk` **and**
  `--synthetic-jdk` and expect `RESULT ok` in both, then run the suites named
  under Blast radius. **Two corrections to that instruction, 2026-08-12.**
  (1) `--synthetic-jdk` is not a mode the default binary has; it needs a
  `cargo build --release -p cratonvm-cli --features synthetic-jdk` binary, and
  per the table above that binary is the *only* one containing three of the
  five findings. (2) `probes/` is never run by `regression-suite/run.sh` at any
  `SUITE=` value — grepped, `run.sh` names no path under `probes/` — so **no
  suite run, however green, discharges this record.** The probe has to be run
  by hand, in both arms, and the two Compatible-reachable rows are the only
  ones a `--real-jdk` transcript can speak to.
