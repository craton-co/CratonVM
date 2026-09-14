# W7-70 — `PrintStream.close()` was a no-op for every stream, not for the one it named

**Status: source landed, UNVERIFIED against a VM.** Nothing here has been
built (this lane writes code and docs; the orchestrator builds). What is
stated as measured was measured — on HotSpot 25.0.3.9 (Eclipse Adoptium), by
running Java, or by reading `lib/src.zip` from that same image. What is stated
as read-from-source was read from source. The two are kept apart on purpose.

Branched from `dev@7273656c1` and merged
`fix/printstream-trouble-and-errormanager-20260812` on top, because the
`trouble` machinery this lane builds on had not reached `dev`. Everything
W7-64-printstream-trouble-and-errormanager.md landed is used, not reinvented.

Closes the residual W7-64 named first under **What is left**, and the one
W7-57-close-flush-swallow-sweep.md left before it.


> **VERIFIED AGAINST A BINARY 2026-09-03. The fix works on both shipping arms
> and this record's headline defect is STILL LIVE under `--synthetic-jdk`.**
>
> Run via `probes/CloseFlushSwallowProbe.java`, the handle this record shares
> with W7-57 and W7-81. This record's own opening example was
> `new PrintStream(fileOutputStream).close()` delivering "no bytes on disk".
> The probe asserts exactly that:
>
> ```text
>                                    HotSpot        compatible  --jdk-only  --synthetic-jdk
> printStreamFileNonEmptyAfterClose   true            PASS        PASS         false
> printStreamFileContentAfterClose    data-on-disk    PASS        PASS         "" (empty)
> printStreamCloseIoSinkTrace         flush,close     flush,close,flush  (same)  (same)
> ```
>
> So the receiver test this record shipped — "a null `out` IS the console" —
> is doing its job in Compatible and `--jdk-only`, and the exact byte loss the
> record was written about reproduces verbatim under `--synthetic-jdk`.
>
> **It is not a stale second registration.** That was the first thing checked,
> because this record's own diagnosis was that `close` is registered
> unconditionally in both registrars. The registry dump under `--synthetic-jdk`
> says both doors point at the fixed native and names which one runs:
>
> ```text
> close ()V  by=native-builtins/src/logging_shims.rs:363  owns_slot=False  inv=0
> close ()V  by=native-builtins/src/lib.rs:24078          owns_slot=True   inv=6   overwrote=bridge
> ```
>
> The owning slot is the repaired `native_printstream_close` and it ran six
> times. The loss is downstream of the close, not in front of it.
>
> **The residual on the shipping arms** is one extra `flush`:
> `flush,close,flush` against HotSpot's `flush,close`. The probe already
> isolates its cause in a printed observation —
> `observed.printStreamCheckErrorOnClosedStreamReflushed=1` against HotSpot's
> `0` — so `checkError()` on an already-closed `PrintStream` re-flushes it.
> That is a new, narrow defect, not a return of the no-op.
>
> **What this does NOT verify, and what is NOT diagnosed.** The
> `--synthetic-jdk` byte loss is measured, not explained. A GC guard fired in
> that same run (`zgc::get_field: plain-object field access on an ARRAY
> receiver`, `element_type=Char`) and it is **co-located, not shown to be the
> cause** — it is recorded so the next reader has the thread, not as a finding.
> Nothing here re-verifies §§ that this record measured on HotSpot or read from
> source; this note covers the CratonVM column the record never had.
## The defect

```rust
pub(crate) fn native_printstream_close(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Don't actually close stdout/stderr
    Ok(None)
}
```

The comment is a correct reason for a receiver test the body never performed.
`java/io/PrintStream.close()V` is registered **unconditionally in both
registrars** — `register_printstream_fallback_natives` (Compatible and
synthetic) and `register_synthetic_overrides` (synthetic) — so the no-op
applied to every `PrintStream` in the VM:

```java
PrintStream ps = new PrintStream(new BufferedOutputStream(new FileOutputStream(f)));
ps.println("data");
ps.close();     // no bytes on disk, file handle still open, no exception
```

A `try`-with-resources over that block exits cleanly. This is not a lost
exception; it is **lost data reported as success**, which is the exact fault
shape W7-57 exists for, one level up from the `let _ = ctx.invoke_virtual(…)`
sites it swept.

Third defect in this family in one day, and the two before it set the reading
order: `Formatter`'s only guard was the inverse of its own comment, and
`Handler.<init>`'s native left `errorManager` null on every handler. So no
claim below rests on a comment; each rests on `lib/src.zip` or on a
measurement.

## What HotSpot's `close()` actually does — and why reading it is not enough

`lib/src.zip`, JDK 25.0.3.9, `java/io/PrintStream.java:423`:

```java
private boolean closing = false; /* To avoid recursive closing */

public void close() {
    synchronized (this) {
        if (!closing) {
            closing = true;
            try {
                textOut.close();
                out.close();
            }
            catch (IOException x) {
                trouble = true;
            }
            textOut = null;
            charOut = null;
            out = null;
        }
    }
}
```

Read literally, that does **not** flush. It does. `charOut` is
`new OutputStreamWriter(this, charset)` — the character layer writes back into
the `PrintStream` — so `textOut.close()` bottoms out in
`StreamEncoder.implClose`, whose `out` is `this`: it calls `this.flush()`,
which is `out.flush()` on the real sink, and then `this.close()`, which the
`closing` latch turns into a no-op.

So the contract was **measured on the sink** rather than inferred, with an
`OutputStream` that records the order it is driven in:

| case | sink ops | `close()` throws | `checkError()` |
|---|---|---|---|
| clean | `flush, close` | none | `false` |
| sink `flush` throws `IOException` | `flush, close` | none | `true` |
| sink `flush` throws `Error` | `flush` — **close skipped** | the `Error` | — |
| sink `close` throws `IOException` | `flush, close` | none | `true` |
| sink `close` throws `Error` | `flush, close` | the `Error` | `false` |
| second `close()` | nothing further | none | unchanged |
| write after `close()` | nothing | none | `true` |

Two rows do real work that a "flush then close" one-liner gets wrong:

* **A flush `Error` skips the close.** The two statements are straight-line
  inside one `try`, so the repair has to be `absorb(flush)?;` *then*
  `absorb(close)?;` — the `?` between them is the behaviour.
* **The second `close()` does not reach the sink at all.** `closing` is set
  before the delegation and never cleared, including on the path where the
  delegated close throws: after a propagated `Error`, a retry is still a
  no-op (measured).

## The fix

`native_printstream_close`, in `native-builtins/src/logging_shims.rs` — one
body, both registrations, so one edit covers both arms.

1. `if (!closing)` — return early, via `print_error_state::is_closing`.
2. Read `this.out` **by name**. A non-object there (including the `Value::Int`
   fd tag `ensure_system_streams` parks in the legacy synthetic layout's slot
   0) means "no Java sink", i.e. the console: flush the fd, do not latch
   `closing`, return.
3. Otherwise latch `closing`, then
   `absorb_io_exception_recording(flush)?; absorb_io_exception_recording(close)?;`

`absorb_io_exception_recording` is W7-64's, unchanged: it runs the JDK's
`catch (IOException x) { trouble = true; }` body and propagates everything
that `catch` does not name — every `Error`, every `RuntimeException`, and
every `MethodCallFailed::InternalError`, which is not a Java throwable and can
never be the `IOException` a JDK `catch` names.

### The console is still safe, and now for a reason the code states

Three independent guards, none of them a name test:

* `System.out` / `System.err` have a **null `out`**. `ensure_system_streams`
  allocates them zeroed and never populates the inherited
  `FilterOutputStream.out`; under the compact layout it deliberately skips
  even the fd tag so that slot stays null. This is the same invariant
  `route_write_through_out` and `native_printstream_flush` already branch on,
  so the new code adds no new assumption.
* A `PrintStream` that **wraps** `System.out` delegates its close to it and
  lands on that same branch.
* `FdTable::close` refuses `fd < 3` outright.

`stream_fd`'s pointer-identity walk is used only to pick the fd to flush, not
to decide whether to close. That is deliberate and it is where this body
differs from `native_printwriter_close`: a `PrintStream` whose `out` is
non-null is a stream HotSpot closes, including one installed by
`System.setOut`, and refusing to close it would be a fresh divergence.

### `textOut` / `charOut` are deliberately not driven

They are null on every `PrintStream` this VM constructs
(`native_printstream_init_outputstream` sets only `out` and slot 0;
`ensure_system_streams` allocates zeroed). Where a real ctor this VM does not
shadow does populate them — `new PrintStream(File)`,
`new PrintStream(OutputStream, boolean, Charset)` — the character layer is
still **empty**, because our own `print`/`println`/`write` natives write to
`out` directly and never buffer into `textOut`. Driving it as well would flush
the sink twice. If the `native_osw_init` / `native_bw_init` lane W7-64 names
ever makes a real `textOut` load-bearing, this is the site to revisit; the
comment at the code says so.

### Where `closing` lives

`print_error_state` resolves it **by name**, exactly as it resolves `trouble`.
In Compatible mode that lands on the real `java.io.PrintStream.closing` slot.
In synthetic mode the fabricated model grows one — and the arm the two classes
used to share is **split**, because `java.io.PrintWriter` declares no
`closing` field (it uses `out == null`), and fabricating one would make
`is_closing` answer for a `PrintWriter` whose own `close()` deliberately
latches nothing:

```rust
"java/io/PrintStream" => {
    let mut fields = instance_fields(1);
    fields.push(named_field("trouble", "Z"));
    fields.push(named_field("closing", "Z"));
    fields
}
"java/io/PrintWriter" => {
    let mut fields = instance_fields(1);
    fields.push(named_field("trouble", "Z"));
    fields
}
```

Checked against the gates W7-64 checked `trouble` against, plus the one it
did not have to: `t9c_synthetic_field_tables_cover_their_factories` asserts
`declared >= requested` and this only raises `declared`; the largest literal
request for either class is 1; `shadow_layout.rs` names neither class in its
two exact-name-list tests and has no `SAFE_POSITIONAL_CLAIMS` row for either;
no `num_total_fields` assertion in `vm/src/vm/tests.rs`, `classloading/`,
`native-builtins/tests/` or `vm/tests/` names either; and the real class's
padding floor is a `max()`. Nothing addresses `closing` positionally.

### `checkError()` stops re-flushing a closed stream

HotSpot's `checkError()` opens `if (out != null) flush();`, and a closed
`PrintStream` has `out == null`, so it skips. `native_printstream_close`
**cannot** null `out` — a null `out` is this VM's "console stream" marker, and
nulling it would redirect a closed stream's output to stdout, which is exactly
why `native_printwriter_close` refuses to null its own. The `closing` latch is
the closed-marker W7-64 said this needed ("closing it needs a closed-marker
that is not `out == null`"), so `printstream_check_error` reads that instead.

Synthetic-jdk only, because that native is registered from
`register_synthetic_overrides` alone. `java.io.PrintWriter` declares no
`closing`, so `is_closing` is `false` on that side of the shared body and its
behaviour is unchanged.

## The population of console-assuming no-ops

The handed-over count is one, and this lane's is also **one** — but the
sweep that establishes it is the deliverable, because the two prior lanes each
found their handed-over count was a sample (9 → 24, 52 → 51-with-five-false-
positives), and the reason this one does not grow is worth writing down.

Counted by walking every `registry.register*` call in `native-builtins`,
`native-io`, `native-api`, `native-collections`, `native-awt` and `vm` whose
class is `java/io/PrintStream` or `java/io/PrintWriter`, reading each distinct
native body, and asking: **does it branch on "is this stdout/stderr", and is
the other arm silent?**

**79 registrations across 2 registrars** on those two classes. The console
predicates in play are three, and all three are receiver tests rather than
name tests: `stream_fd` (pointer identity against `System.out`/`System.err`,
walking slot 0), `sink_reaches_system_stream` (the same, walking `out`/`se`/
slot 0), and `out == null`.

| # | native | console branch | other arm |
|---|---|---|---|
| **1** | **`native_printstream_close`** | **none — it never tested** | **nothing at all** |
| 2 | `native_printstream_flush` | `out != null` else `stream_fd` | flushes `out` |
| 3 | `stream_write` | surefire → `out` → `stream_fd` | writes through `out` |
| 4 | `stream_writeln_inner` | same | writes through `out` |
| 5 | `native_printstream_write` | same | writes through `out` |
| 6 | `native_printstream_write_int` | same | writes through `out` |
| 7 | `native_printwriter_flush` | `printwriter_sink` + `stream_fd` | flushes the sink |
| 8 | `native_printwriter_close` | `stream_fd` + `sink_reaches_system_stream` | flushes and closes |

**One.** Seven of the eight sites that branch on console-ness have a live
non-console arm; only `close` had no arm at all. That is a real finding rather
than a lucky one: the write and flush paths were built for the
`System.setOut`-to-a-file case (the DaCapo `stdout.log` digest), so they were
forced to grow a non-console arm. `close` never had a caller that noticed.

### Three adjacent findings of the same family, none of them fixed here

* **The terminal-silence arm.** All eight sites end with "no Java sink AND not
  a console fd → do nothing, record nothing". HotSpot's equivalent is
  `ensureOpen()` throwing `IOException("Stream closed")` into the same `catch`
  that sets `trouble`, so HotSpot *records* where we are silent. Reachable in
  practice only for a receiver whose `<init>` never ran (HotSpot's ctor
  null-checks `out` and throws), so it is a defensive fallthrough rather than
  a live no-op — but it is the same shape one step down.
* **`native_bw_close` / `native_osw_close` are "fd or nothing".** Both read
  slot 0 expecting a `Value::Int` fd and return early otherwise, so a
  `BufferedWriter` whose slot 0 is not an fd never reaches its wrapped
  `Writer`. Synthetic-only, and the sinks that land there
  (`StringWriter`) document their own `close()` as having no effect — but this
  is the same "assume the fd shape, otherwise silently nothing" reading, and
  W7-64 names a separate lane already working on `native_bw_init` /
  `native_osw_init`. Not claimed either way here.
* **`native_fos_close` absorbs its host errors.** `let _ = fd_table().flush()`
  and `let _ = fd_table().close()` where `java.io.FileOutputStream.close()`
  propagates. This is now directly downstream of the repair: a disk-full at
  close is still invisible even once `PrintStream.close()` delegates properly.
  Not a delegated-Java-call swallow, so it was outside W7-57's 51.

### One adjacent gap that is the *other* family (W7-64's shape, not this one)

Six methods of `java.io.PrintStream` have no registration anywhere and no
synthetic body, so under `--synthetic-jdk` they raise `NoSuchMethodError` the
way `checkError` did before W7-64: `print(char[])`, `println(char[])`,
`write(byte[])`, `writeBytes(byte[])`, `append(CharSequence,int,int)` and
`append(char)`, plus every constructor except the two `OutputStream` ones.
Compatible mode is unaffected (the real bytecode funnels them into
`write(byte[],int,int)`, which *is* shadowed). Recorded, not fixed — a
missing method is a different defect from a no-op one, and it needs the same
per-triple registration census W7-64 ran rather than a guess.

## `route_write_through_out`'s call sites — decided, and NOT threaded

W7-57 and W7-64 both left this and both gave the same reason: the helper
returns `bool` into helpers returning `()`, so propagating the absorbed
`Error` "is a signature change, not a one-line fix". **Both the count and the
reason were wrong, and the real reason is stronger.**

**The count.** `stream_write` has **12** call sites and `stream_writeln`
**11** — 23, all in native bodies that already return `MethodCallResult`, so
`?` would drop straight in. "Ten" was a sample. Threading is *more* mechanical
work than stated and *less* of an obstacle.

**The real reason.** The absorption and the console fallback are **one
mechanism**. `record_write_failure` answering `false` is precisely what makes
`route_write_through_out` report "not routed", which is what sends the caller
to the fd fast path and prints the text to the console. That fallback is the
picocli / JUnit-console survival path — and the failure it survives is a
`NoSuchMethodError`, i.e. an `Error`. **Propagating the `Error` with `?`
deletes the fallback on the exact case it exists for.** They cannot be
separated by threading a `Result`; they can only be separated by deciding what
"the sink refused the call" should do differently from "the sink failed", and
that is a behaviour change on the hottest path in the VM.

**A third fact, found while deciding**: the two branches of
`route_write_through_out` do not agree today. The `sink_is_writer` branch
returns `write_string_to_writer(…)`, which is `record_write_failure(…)` — so
it reports "not routed" on failure and the text is echoed to the console. The
byte branch calls `record_write_failure` and then returns `true`
**unconditionally** — so a failed byte write is not echoed. One of those two
is wrong and it is not obvious which.

**The design, for whoever takes it**, so the next lane does not have to
re-derive it. Split the answer three ways instead of two:

| delegated call returned | HotSpot | routing answer |
|---|---|---|
| cleanly | bytes delivered | routed |
| `IOException` / `InterruptedIOException` | absorbed, `trouble` set, **nothing written anywhere** | **routed** — do not echo |
| `Error` / `RuntimeException` / `InternalError` | propagates | not routed — keep the console fallback |

That makes the two branches agree, removes the double-write divergence
(HotSpot writes nowhere when the sink raises), and preserves the picocli
fallback exactly where it is load-bearing. It is not done here because it
changes `System.out.println`'s behaviour on every failure path and this lane
cannot build or measure; and because shipping it blind is how a fallback that
nobody remembered got deleted.

**The "not routed" fallback survives this branch untouched.** Nothing in
`route_write_through_out`, `stream_write`, `stream_writeln`,
`write_string_to_writer` or `record_write_failure` changed behaviour; only
their comments did, to state the coupling and correct the count.

## Prove the RED

`probes/CloseFlushSwallowProbe.java`, extended — not replaced. **96 printed
lines, 93 of them asserted**, `RESULT ok` on HotSpot 25.0.3.9 today. W7-57's
22 checks and W7-64's 42 are unchanged; 29 are new.

Both of the probe's standing properties are kept:

* **The assertion is that the effect ARRIVES** — bytes on disk, the sink's
  `close()` ran, the OS handle released — never that the call returned. A
  no-op `close()` returns perfectly cleanly, which is the whole problem with
  it, so a check that passed because nothing threw would be the defect's own
  shape.
* **Every recording row is paired with an over-correction guard on the same
  call site.** A `PrintStream` that starts throwing from `close()`, or that
  closes too eagerly, is a worse defect than one that fails to close.

| guard | HotSpot | guards against |
|---|---|---|
| `printStreamCleanCloseThrowsNothing` | `none` | a blanket `?` that throws on success |
| `printStreamCheckErrorAfterCleanFileClose` | `false` | setting `trouble` because a close happened |
| `printStreamCheckErrorAfterPropagatedCloseError` | `false` | recording a failure the JDK's `catch` never sees |
| `printStreamDoubleCloseDidNotRecloseSink` | `1` | closing too eagerly — no `closing` latch |
| `printStreamDoubleCloseDidNotReflushSink` | `1` | the same, on the flush half |
| `printStreamRetryAfterPropagatedCloseDidNotRetouchSink` | `1` | clearing `closing` on the throwing path |
| `printStreamFlushErrorSkipsTheClose` | `flush` | "flush; close;" with the failure dropped between them |
| `printStreamSinkNotClosedBeforeClose` | `0` | a check that would pass without the close |

Two construction details are load-bearing and stated at the code:

* **The buffered wrapper.** A `PrintStream` straight over a
  `FileOutputStream` has its bytes on disk before `close()` is ever called, so
  the file would be complete even with the no-op and the disk row would be
  green for the wrong reason. The row uses
  `new BufferedOutputStream(new FileOutputStream(f))` and asserts the file is
  **empty before** the close as well as complete after.
* **A new `TraceOut` sink**, rather than reusing `BoomOut`. `BoomOut` records
  "was it attempted"; the contract here is an *order*, and `BoomOut` cannot
  express "flush ran and close did not". `TraceOut`'s `write` ops are recorded
  but never asserted on — this VM writes text and separator as one buffer and
  uses `"\n"` where HotSpot on Windows uses `"\r\n"` — so the assertions run
  against a projection that drops them.

**The handle observable, and where it is vacuous.** On Windows an open handle
blocks `delete`; on Linux it does not. So the expectation is derived from
`os.name` rather than fixed: `printStreamDeleteBlockedWhileOpen` asserts
`true` on Windows and `false` on Linux — a real assertion on both — while
`printStreamDeleteSucceedsAfterClose` is load-bearing only on Windows and
vacuous on Linux. That is why the platform-free sink rows
(`printStreamSinkNotClosedBeforeClose` / `printStreamSinkClosedAfterClose`)
exist: the delete pair is corroboration, not the evidence.

**Three rows are printed and NOT asserted**, each a residual this lane names
rather than fixes — the same treatment W7-57 gave
`observed.printWriterCheckErrorAfterAbsorb` before W7-64 promoted it:

* `observed.printStreamBytesWrittenAfterClose` (HotSpot `0`)
* `observed.printStreamCheckErrorAfterWriteOnClosedStream` (HotSpot `true`)
* `observed.printStreamCheckErrorOnClosedStreamReflushed` (HotSpot `0`)

**Run it in BOTH modes.** Per finding:

| finding | arm |
|---|---|
| `close()` delivers nothing and releases nothing | **both** — the triple is registered from `register_printstream_fallback_natives`, which runs in Compatible mode too |
| the `closing` latch / double-close | **both** — the real class declares the field |
| `checkError()` skipping its flush on a closed stream | **synthetic only** — `printstream_check_error` is registered from `register_synthetic_overrides` alone; Compatible runs the real bytecode over a non-null `out` and still flushes |
| the synthetic `closing` slot | **synthetic only** |

A green Compatible-mode run is therefore not evidence about two of those four.

## Which registrar wins

Last-write-wins, so every registration of each triple was grepped before
anything changed.

| triple | registrars | Compatible winner | synthetic winner |
|---|---|---|---|
| `java/io/PrintStream.close()V` | `register_printstream_fallback_natives` (logging_shims.rs, ambient `Bridge`), `register_synthetic_overrides` (lib.rs, ambient `Intrinsic`) | the fallback registrar — the synthetic one is `#[cfg(feature = "synthetic-jdk")]` and not called | `register_synthetic_overrides`, which runs after `register_essential_natives` |
| `java/io/PrintStream.checkError()Z` | `register_synthetic_overrides` only | — (real bytecode) | `register_synthetic_overrides` |

**Both `close` registrations name the same function**, so the ambient
`NativeKind` split — `Bridge` in one block, `Intrinsic` in the other — is a
census-tag difference and not a behavioural one, and one edit to the body
covers both arms. No registration was added, moved or removed by this branch,
so no `NativeKind` boundary moved and no `retired_shadow.rs` row is affected.

## Compatible-mode justification, per change class

Compatible mode is contractually frozen except for genuine HotSpot-parity bug
fixes. Two change classes touch it; both are parity, and both name the HotSpot
behaviour they converge on.

1. **`close()` flushes and closes `out`, and records an absorbed
   `IOException` in `trouble`.** HotSpot's `close()` drives the sink `flush`
   then `close` — measured, not inferred — and its `catch (IOException x)`
   body is `trouble = true`. We did neither. Closing a file the JDK closes is
   parity; so is leaving the process console alone, which HotSpot would not do
   but which `FdTable::close` has refused for as long as it has existed and
   which this change does not alter.
2. **`closing` is latched on the real field.** HotSpot sets it at exactly that
   point. Nothing else in the image reads it — `closing` appears only inside
   `close()` itself, which our native shadows — so the write is observable
   only through the double-close behaviour it is there to produce.

Everything else is synthetic-only or comment-only: the fabricated `closing`
slot, the `checkError` gate, and the corrected comments on
`route_write_through_out` / `printwriter_autoflush_if_needed` /
`record_write_failure`.

## Tests

No test was weakened. The four tests W7-57 tightened
(`gzip_output_stream_p70`, `stream_handler_lifecycle_p61`,
`object_output_stream_p70`, `pushback_reader_basics_p66`) assert that their
sink slot is null; none of them is a `PrintStream` and none is touched. No
test in `vm/src/vm/tests.rs` calls `java/io/PrintStream.close`, and the
`alloc_receiver(…, "java/io/PrintStream", 0)` fixtures allocate zero fields,
so `get_field_by_name(this, "out")` answers `Value::Object(None)` and the new
body takes the console branch and does nothing — the same outcome those
fixtures already had.

## Blast radius

Narrower than W7-57's in kind — this changes one native — and wider in reach,
because that native is on `PrintStream`.

**Highest — anything that closes a `PrintStream` over a real sink.**

* The sink's `close()` now runs. Every `new PrintStream(…)` in every suite that
  is closed, explicitly or by `try`-with-resources, now releases its
  descriptor and delivers its buffer. That is the fix, and it is also the risk:
  code that has been writing to a stream **after** closing it has been working
  by accident and will now write to a closed sink. **Suites to run: Spring
  Boot (full), Tomcat, Surefire/JUnit-console, DaCapo (the `stdout.log` digest
  path, which is the `System.setOut`-to-a-file case this native's neighbours
  exist for).**
* An `IOException` from the sink's `flush`/`close` is now absorbed **and
  recorded**, so `checkError()` starts answering `true` where it answered
  `false` forever. A caller that treats `checkError()` as an abort signal will
  start aborting. Same risk W7-64 flagged for the write paths, now on close.
* An `Error` from the sink's `flush`/`close` now **propagates** out of
  `close()`. In this VM that is almost always a `NoSuchMethodError` from our
  own dispatch, i.e. a registration gap this makes visible. Expect *new*
  failures rather than broken working ones; each is a gap, not a regression.

**Medium — the `closing` latch.**

* A second `close()` is now a no-op where it used to be a no-op for a different
  reason, so nothing observable changes there. But a stream closed and then
  *reused* — closed, written, closed again — now has its second close skipped
  entirely. HotSpot behaves identically; code relying on the old behaviour was
  relying on there being no old behaviour.
* One field is added to the synthetic `java/io/PrintStream` layout. Additive,
  no positional access, gates checked above.

**Low — `checkError()`.**

* Synthetic mode only, and only on a closed stream: it stops flushing. A test
  that counted flushes through `checkError()` after a close will see one
  fewer.

**None.**

* The console. `System.out`/`System.err` take the null-`out` branch, which
  flushes the fd and returns — the same net effect as the old no-op plus a
  flush. `FdTable::close` still refuses `fd < 3`.
* Compatible-mode `checkError`/`setError`/`clearError` (untouched).
* Every `delegated_close` and `print_error_state` decision from W7-57 and
  W7-64 (untouched).
* `route_write_through_out` and the whole write path (comments only).

## Re-verified 2026-08-12 against the working tree, by the P3-D lane

Nothing built, nothing run.

**The repair is PRESENT and this record is accurate, not stale.**
`native_printstream_close` at `native-builtins/src/logging_shims.rs:1245` is the
body this record describes, in the order it describes: `is_closing` early return
(`:1254`), `out` read by name (`:1263`), the console branch on a non-object
`out` that flushes the fd and deliberately does **not** latch `closing`
(`:1264`–`:1277`), then `latch_closing` followed by
`absorb_io_exception_recording(flush)?` and
`absorb_io_exception_recording(close)?` (`:1278`–`:1282`). The `?` between the
two is the "a flush `Error` skips the close" behaviour the measurement table
above required. The site is in `native-builtins`, which this lane does not own;
nothing needed changing.

### One residual CLOSED, and a second site for it that this record had not counted

This record's **What is left** names `native_fos_close` as "directly downstream
of the repair: a disk-full at close is still invisible even once
`PrintStream.close()` delegates properly". That site is in `native-io/src/lib.rs`
(`:2237`), which this lane owns, and it is now fixed — see
`W7-57-close-flush-swallow-sweep.md`'s 2026-08-12 re-verification for the full
reasoning. In summary: it propagates the host `flush` and `close` failure for
`fd >= 3` (flush wins, close still attempted), keeps the swallow for `fd < 3`
because `FdTable::close` refuses those outright and the flush there is of the
shared process console, and it now agrees with its own neighbour
`native_fos_flush`, which already propagated.

**The site this record named is not the one the shipping mode uses.** The real
`FileOutputStream.close()` bytecode routes through `FileDescriptor.closeAll` →
`close()` → `close0()`, i.e. through `native_fd_close0`
(`native-io/src/lib.rs:1846`), which carried the identical
`let _ = flush; let _ = close` pair and which no record had counted. Fixing only
`native_fos_close` would have repaired the fallback body, left Compatible mode
swallowing, and taken the row off the list anyway. Its **flush** half now
propagates (blast radius outside buffered file writers is provably empty —
`FdTable::flush` ends in `_ => Ok(())` for every non-writable entry, which covers
the `FileInputStream` and `sun/nio/ch/UnixDispatcher.close0` socket
registrations that share the body); its **close** half deliberately does not,
and that is the residual left in its place rather than a claim of completion.

This is the same lesson this record already teaches about its own census — the
handed-over count is a sample until the *registrations* are walked. Here the
sample was one function name where the shipping path was a different one.

## What is left

* **A write after `close()` is not refused.** HotSpot nulls `out`, so
  `ensureOpen()` throws into the `catch` that sets `trouble` and nothing is
  written. This VM cannot null `out` — it is the console marker — so a
  post-close write reaches the closed sink instead of being refused at the
  door, and what happens then is the sink's business (a `BufferedOutputStream`
  will buffer it silently and never flush it). Closing this needs the write
  path to consult the `closing` latch, which is one `get_field_by_name` per
  `println` on the hottest path in the VM and therefore its own measurement.
  `observed.printStreamBytesWrittenAfterClose` is the gap, measured `0` on
  HotSpot.
* **Compatible-mode `checkError()` still flushes a closed stream.** The latch
  gate lives in the synthetic `checkError` native; Compatible mode runs real
  bytecode over a real, non-null `out`. The *answer* agrees with HotSpot (the
  close already recorded); only the extra flush differs.
  `observed.printStreamCheckErrorOnClosedStreamReflushed` is the gap.
* **The three-way routing answer** for `route_write_through_out`, designed
  above and deliberately not shipped.
* **The six unregistered `PrintStream` methods** listed above, which are
  `NoSuchMethodError` under `--synthetic-jdk`.
* ~~**`native_fos_close` absorbs its host `flush`/`close` errors**, so a
  disk-full at close is still invisible one layer below the repair.~~
  **FIXED 2026-08-12** — see the re-verification section above. What replaces it
  as the residual is narrower and is stated there: `native_fd_close0`'s **close**
  half is still swallowed (its flush half is not), because that body is shared
  with `sun/nio/ch/UnixDispatcher.close0` on sockets and no measurement covers
  propagating a socket close failure. `native_fis_close`
  (`native-io/src/lib.rs:1903`) likewise still swallows its close; a read-side
  close cannot lose buffered data, so it is the lowest-value member of the
  family. **Neither of the three edits has been compiled.**
* **`addSuppressed`** — still open from W7-57, untouched.
* **`java.io.PrintWriter` has no closed marker at all.** W7-64 named this; it
  is unchanged, because `PrintWriter` genuinely declares no `closing` field
  and inventing one would be fabricating a slot the real class does not have.
* Nothing in this record has run on a VM. The next lane with a build should run
  `probes/CloseFlushSwallowProbe.java` under `--real-jdk` **and**
  `--synthetic-jdk` and expect `RESULT ok` in both, then run the suites named
  under Blast radius.
