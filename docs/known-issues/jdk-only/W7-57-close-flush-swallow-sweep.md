# W7-57 — the 51 close/flush sites where a delegated Java call's failure was dropped

**Status: source landed, UNVERIFIED against a VM.** Nothing here has been built
(this lane writes code and docs; the orchestrator builds). What is stated as
measured was measured — on HotSpot 25.0.3.9 (Eclipse Adoptium), by running
Java, or by reading `lib/src.zip` from that same image. What is stated as
read-from-source was read from source. The two are kept apart on purpose.

Follows W7-52-formatter-close-and-locale.md, which found the shape and counted
the population but deliberately did not sweep it.


> **VERIFIED AGAINST A BINARY 2026-09-03.** This record's status was "source
> landed, UNVERIFIED against a VM"; it now has a CratonVM column, from
> `probes/CloseFlushSwallowProbe.java` in all three modes against Temurin
> 25.0.3+9-LTS on the same host.
>
> ```text
>                    checks failed   lines differing from HotSpot
> compatible               4                     6
> --jdk-only               4                     6
> --synthetic-jdk          9                    13
> ```
>
> **The sweep holds. One site in the census does not**, and it is this record's
> own shape — a delegated failure dropped on close:
>
> ```text
> filterOutFlushFailureWins   HotSpot java.lang.Error: flush-boom   CratonVM none
> ```
>
> `FilterOutputStream.close()` must let the flush failure win and suppress only
> a close failure into it. Ours absorbs it. That is a real, still-open site of
> the 51 this record swept, and it fails identically in all three modes, so it
> is not a mode artefact.
>
> Two further sites fail **only** under `--synthetic-jdk` and belong to this
> record's family too:
>
> ```text
> propertiesStoreStreamPropagatesFlushIOException   IOException   ->  none
> propertiesStoreWriterPropagatesFlushIOException   IOException   ->  none
> zipOutClosePropagatesError    Error: zip-close-boom  ->  NPE "this.names is null"
> ```
>
> The `zipOutClose` row is worse than a swallow: an internal
> `NullPointerException` from our own `ZipOutputStream` state replaces the
> caller's error, so the caller is told the wrong thing rather than nothing.
>
> **What this does NOT verify.** The census itself — the 51 sites and the count
> that was off by five — is a source walk and was not re-walked; this note
> verifies BEHAVIOUR on the probe's assertions only. The probe covers a sample
> of the swept sites, not all 51, so "the sweep holds" is bounded by what the
> probe dispatched. A site the probe never touches is neither confirmed nor
> refuted here. The `--synthetic-jdk` rows are additionally bounded by that
> mode's own unrelated breakage: the same run logs a missing
> `java/io/FileDescriptor.initIDs()V` and a GC array-receiver guard.
>
> Supersedes the interim ratchet note that read "129 rows vs HotSpot 120, 35
> differing". That was a 2026-09-02 measurement on an OLDER binary under
> `--synthetic-jdk` only; 35 -> 13 is mostly merged `dev` work, and the
> mode-attributable gap is 13 vs 6 on one tree.
## The shape

A native stands in for a JDK method whose whole job is to hand the call on —
`close()` propagating to the stream it wraps, `flush()` pushing the sink. The
delegation is spelled

```rust
let _ = ctx.invoke_virtual(inner, "close", "()V", &[]);
```

and that discards the whole `Result`. `MethodCallFailed` has two variants and
this drops both: every Java throwable the delegated call raised, and every
internal VM error. On a `close()` after buffered writes the consequence is not
"an exception was lost". It is **lost data reported as success**, because the
caller's `try`-with-resources sees a clean exit.

There is no global catch-all to fix centrally — W7-52 measured that: the VM's
deliberate swallow instrument (`record_swallow`) has five call sites and none
on a native path, and the native funnel propagates with `?`. This is a
hand-written idiom repeated at the call sites, and each site made its own
silent decision.

## The census, and the count that was off by five

Counted on this branch (`fix/close-flush-swallow-sweep-20260812`, on
`dev@9151543f3` + one commit), by walking every `ctx.invoke*(` call in
`native-builtins`, `native-io`, `native-collections`, `native-awt`,
`native-api` and `vm`, balancing parentheses to find the whole call, and
classifying the statement it sits in:

| disposition | close/flush sites |
|---|---|
| propagates with `?` | 19 |
| propagates by `return` | 8 |
| `Err` arm handled (`if let Err(e) = …`) | 2 |
| bound to a variable that is used | 2 |
| **`let _ = …;` — the failure is DROPPED** | **51** |
| total close/flush delegations found | 82 |

**W7-52 reported 52 and this branch has 51, and the difference is not drift.**
It is two errors that nearly cancel:

* Its grep for `let _ = ctx.invoke_` also matched **five** sites spelled
  `let _ = ctx.invoke_virtual(…)?;` — `native-builtins/src/lib.rs` at the
  `OutputStreamWriter` flush/close pair, the `<init>` input-stream close, and
  the two `invoke_virtual_bytecode_only` sites. The `let _` there discards only
  the `Option<Value>`; the `?` propagates the failure. Those five were never
  swallows.
* Its 52 was taken *after* its own four `java.util.Formatter` repairs, which
  have not merged to `dev` and are therefore still `let _ =` here.

51 − 4 (Formatter, still open here) + 5 (false positives) = 52. The two counts
agree exactly once both corrections are applied. **The lesson is the one W7-52
already stated in general form and is worth restating concretely: `let _ =` is
not the discard — the absence of `?` is.** A sweep keyed on the prefix finds
sites that are fine and would have "fixed" five call sites that were already
correct.

### Were there really only three spellings?

W7-52 named three besides `let _ =`: `if let Ok(…)` with no `Err` arm, `.ok()`,
and `.is_ok()`. Over the whole ~2,787-call `ctx.invoke*` population this branch
finds those three and no more — specifically, **zero** instances of each of the
shapes worth checking for: `drop(ctx.invoke_*(…))`, bare `_ = …` without `let`,
`.unwrap_or_default()`, `.unwrap_or(…)`, and `match … { Err(_) => {} }` with an
empty arm (16 `Err(_) =>` arms exist and all 16 have a body).

But there **is** a fourth spelling, and it is the largest of them after
`let _ =`: **215 calls bound to a local that is then never inspected** —
`let close_result = ctx.invoke_virtual(…);`. Two of those 215 are on
close/flush (`native-builtins/src/apps_h2.rs`, `native-io/src/lib.rs`'s
`bos_inner` close) and both of those two *do* consult the binding, so they are
counted above as handled. The other 213 are outside this lane's scope and are
not claimed either way — a binding is not evidence of a swallow, only a place
one can hide from a grep. Naming it here so the next sweep does not have to
rediscover that `let _ =`, `if let Ok`, `.ok()` and `.is_ok()` are four of five
shapes, not four of four.

## Method

For each of the 51: identify the JDK method the native stands in for, read that
method's body in `lib/src.zip` from JDK 25.0.3.9, and ask **what it does with a
failure at that point**. Not mechanical, and deliberately not scripted — a
blanket transformation is exactly how a correct swallow becomes a defect.

Three answers came back, and all three occur:

* **It propagates.** The `java.io` / `java.util.zip` / `sun.nio.cs` wrapper
  family declares `throws IOException` and catches nothing it does not rethrow.
  34 sites.
* **It catches, and catches something specific.** `java.io.PrintWriter` and
  `java.io.PrintStream` catch `IOException` and set `trouble`.
  `java.util.logging.StreamHandler` catches `Exception` and reports through the
  `ErrorManager`. These swallows are KEPT — converting them into throws would
  be a new defect, and a far more visible one than the one being removed. What
  is wrong is their **width**: no JDK `catch` here catches an `Error`. 9 sites.
* **HotSpot never makes this call at all.** A durability flush inside a bridge,
  a probe stream closed so it is not leaked, an error backstop. There is no JDK
  `catch` to copy, so the swallow stands — but an `Error` is still never "the
  sink misbehaved". 4 of these could not be narrowed for a structural reason
  given per row.

**None of the 51 is "cannot determine".**

### The highest-value class of fix

Narrowing, not propagating. `MethodCallFailed::InternalError` is not a Java
throwable at all and can never be the `IOException` a JDK `catch` names; and a
`NoSuchMethodError` out of a delegated call does not mean the stream failed, it
means **our own dispatch failed to find the method**. Absorbing that into a
`catch` written for `IOException` converts a broken VM into a quietly wrong
one, which is the exact fault W7-52 found in `Formatter` and the reason this
lane exists. Every one of the 9 narrowed sites, and 2 of the 4 kept ones, now
report it.

The shared policy lives in `native-api/src/delegated_close.rs`
(`absorb_io_exception`, `absorb_exception`, `vm_only_best_effort`,
`absorb_thrown`). The type test is by `ClassId` hierarchy — the question the
`instanceof` opcode asks — never by class name.

**No new `CRATONVM_*` flag was added.** Extending `record_swallow` to cover
these was considered and rejected: `record_swallow` lives in
`vm/src/runtime/diagnostics.rs`, which the three native crates sit below, and
the sites that keep a swallow now keep it *at documented JDK parity* — a
counter that fires on `PrintWriter.close()` absorbing an `IOException` would be
counting the JDK, not a defect.

## The 51 rows

Cited by function or registration rather than by line, so the table survives
the next edit. "JDK body" is what `lib/src.zip` says the method we stand in for
does with a failure at that point.

### Propagates now — 34

| # | file | native (class.method) | delegated call | JDK body |
|---|---|---|---|---|
| 1 | phases_late/io_streams.rs | `java/io/PushbackInputStream.close` | `in.close` | `in.close()`, no catch |
| 2 | phases_late/io_streams.rs | `java/io/PushbackReader.close` (p58) | `in.close` | `super.close()` → `FilterReader.close()` → `in.close()` |
| 3 | phases_late/io_streams.rs | `java/io/PushbackReader.close` (2nd registrar) | `in.close` | same |
| 4 | phases_late/io_streams.rs | `java/io/ObjectOutputStream.flush` | `out.flush` | `bout.flush()`, no catch |
| 5 | phases_late/io_streams.rs | `java/io/ObjectOutputStream.close` | `out.flush` | `flush(); clear(); bout.close();` |
| 6 | phases_late/io_streams.rs | `java/io/ObjectOutputStream.close` | `out.close` | same |
| 7 | phases_late/io_streams.rs | `java/io/ObjectInputStream.close` | `in.close` | `bin.close()`, no catch |
| 8 | serialization.rs | `java/io/ObjectOutputStream.close` | `this.flush` | as row 5 |
| 9 | serialization.rs | `java/io/ObjectOutputStream.close` | `out.close` | as row 6 |
| 10 | servlet.rs | `java/io/InputStreamReader.close` (synthetic) | `in.close` | `sd.close()` → `implClose()` → `in.close()` |
| 11 | classloader.rs | `java/io/DataInputStream.close` | `in.close` | `FilterInputStream.close()` = `in.close()` |
| 12 | lib.rs | `native_input_stream_reader_close` (**unregistered**) | `in.close` | as row 10 |
| 13 | phases_late/zip_streams.rs | `java/util/zip/ZipInputStream.close` | `in.close` | `super.close()` → `InflaterInputStream.close()` → `in.close()` |
| 14 | phases_late/zip_streams.rs | `java/util/zip/ZipOutputStream.close` | `out.close` | `super.close()` → `DeflaterOutputStream.close()`, `out.close()` in `finally` |
| 15 | phases_late/zip_streams.rs | `java/util/zip/InflaterInputStream.close` | `in.close` | `in.close()`, no catch |
| 16 | phases_late/zip_streams.rs | `java/util/zip/DeflaterOutputStream.flush` | `out.flush` | `out.flush()`, no catch |
| 17 | phases_late/zip_streams.rs | `java/util/zip/DeflaterOutputStream.close` | `out.close` | `out.close()` in `finally` |
| 18 | phases_late/zip_streams.rs | `java/util/zip/GZIPOutputStream.flush` | `out.flush` | inherits row 16 |
| 19 | phases_late/zip_streams.rs | `java/util/zip/GZIPOutputStream.close` | `out.close` | inherits row 17 |
| 20 | properties_sidetable.rs | `java/util/Properties.store(OutputStream,String)` / `save` | `out.flush` | `store0` ends in `bw.flush()`, no catch |
| 21 | properties_sidetable.rs | `java/util/Properties.store(Writer,String)` | `writer.flush` | same |
| 22 | phases_late/xml_json.rs | `Transformer.transform`, `StreamResult.getWriter` branch | `writer.flush` | serializer flush → `TransformerException` |
| 23 | phases_late/xml_json.rs | `Transformer.transform`, `getOutputStream` branch | `stream.flush` | same |
| 24 | native-io/lib.rs | `java/io/InputStreamReader.close` (`native_isr_close`) | `in.close` | as row 10 |
| 25 | native-io/lib.rs | `java/io/FilterOutputStream.close` | `this.flush` | `try{flush()} catch(Throwable){rethrow} finally{out.close()}` |
| 26 | native-io/lib.rs | `java/io/FilterOutputStream.close` | `out.close` | same |
| 27 | native-io/lib.rs | `java/io/Reader.close` (`native_reader_close`) | `in.close` | decorator family, no catch |
| 28 | native-io/lib.rs | `java/io/DataOutputStream.close` | `inner.flush` | inherits row 25 |
| 29 | native-io/lib.rs | `java/io/LineNumberReader.close` | `in.close` | `BufferedReader.close()` = `in.close()` in `try`/`finally` |
| 30 | native-io/stream_decoder.rs | `sun/nio/cs/StreamDecoder.close`/`implClose` | `in.close` | `if (ch != null) ch.close(); else in.close();` |
| 31 | native-io/stream_decoder.rs | same | `ch.close` | same |
| 32 | native-io/stream_encoder.rs | `sun/nio/cs/StreamEncoder.flush`/`flushBuffer` | `out.flush` | `implFlush()` = `implFlushBuffer(); out.flush();` |
| 33 | native-io/stream_encoder.rs | `sun/nio/cs/StreamEncoder.close`/`implClose` | `out.flush` | `try (out) { …; out.flush(); } catch (IOException x) { …; throw x; }` |
| 34 | native-io/stream_encoder.rs | same | `out.close` | the `try`-with-resources close |

Ordering was preserved per JDK body wherever the JDK's is observable:

* Row 5/6 and 8/9 — `ObjectOutputStream.close` is straight-line, so a failing
  flush skips the close.
* Rows 25/26, 28, 33/34 — the flush failure **wins**; the close is attempted
  either way and surfaces only when the flush succeeded.
* Rows 14/17/19 and 30/31 — the JDK's `finally` still runs, so the closed
  marker and our side-table drops still run on the failing path and the failure
  is reported after.
* Row 2/3 — `PushbackReader` clears its buffer only after a successful close,
  because HotSpot's `buf = null` sits after `super.close()` with no `finally`.

### Correctly swallows, kept and narrowed — 9

The JDK really does catch here. The swallow stays; its width is reduced to the
type the JDK's own `catch` names.

| # | file | native | delegated call | JDK `catch` | now |
|---|---|---|---|---|---|
| 35 | lib.rs | `java/io/PrintWriter.flush` | `out.flush` | `catch (IOException x) { trouble = true; }` | `absorb_io_exception` |
| 36 | lib.rs | `java/io/PrintWriter.close` | `sink.flush` | (ours; same policy) | `absorb_io_exception` |
| 37 | lib.rs | `java/io/PrintWriter.close` | `out.close` | `catch (IOException x) { trouble = true; }` | `absorb_io_exception` |
| 38 | logging_shims.rs | `java/io/PrintStream.flush` | `out.flush` | `catch (IOException x) { trouble = true; }` | `absorb_io_exception` |
| 39 | phases_late.rs | `java/util/logging/StreamHandler.flush` | `writer.flush` | `catch (Exception ex) { reportError(…, FLUSH_FAILURE) }` | `absorb_exception` |
| 40 | phases_late.rs | `java/util/logging/StreamHandler.close` | `writer.flush` | `flushAndClose`'s `catch (Exception ex)` | `absorb_exception` |
| 41 | phases_late.rs | `java/util/logging/StreamHandler.close` | `writer.close` | same | `absorb_exception` |
| 42 | logmanager.rs | `publish_to_jul_handlers_full` | `handler.flush` | none — HotSpot makes no such call | `vm_only_best_effort` |
| 43 | phases_late/net_channels.rs | `HttpHandler.handle` 500 backstop | `exchange.close` | none — ours | `vm_only_best_effort` |

Row 36 deserves its own line: HotSpot's `PrintWriter.close()` does **not**
flush — it closes and lets the sink's own `close()` deliver. The flush there is
ours, so it takes the same `IOException`-absorbing policy as its neighbours
rather than a stricter one that would make our extra work visible where
HotSpot's is not.

### Correctly swallows, kept whole, commented — 4

Each has a structural reason it could not be narrowed. None is "we did not get
to it".

| # | file | native | why the swallow is right | why it is not narrowed |
|---|---|---|---|---|
| 44 | classloader.rs | `probe_resource_exists` | `URLClassPath$Loader.getResource` wraps its whole `openConnection`/`getInputStream` region in `catch (Exception e) { return null; }` — a failed probe is "no such resource" | returns `bool`, and its caller holds two native pins across the call |
| 45 | lib.rs | `printwriter_autoflush_if_needed` | dispatches `PrintWriter.flush()`, which absorbs `IOException` itself; HotSpot's autoflush cannot raise one either | this helper and `stream_writeln` both return `()` across ten call sites |
| 46 | logmanager.rs | `publish_existing_record_to_jul_handlers` | HotSpot's `Logger.log` makes no `Handler.flush()` call at all | returns `bool` (its sibling, row 42, returns a `Result` and IS narrowed) |
| 47 | net_phase_e.rs | `re10_dispatch_pending` auth-reject backstop | no JDK body to copy a `catch` from | sits inside the pending-exchange dispatch LOOP holding `ex_pin`; propagating would abort serving every remaining exchange because one rejected request failed to close — a worse defect than the one removed |

The residual on all four is the same and is named in each comment: an `Error`
is absorbed where HotSpot would let it out.

### Owned by another lane — 4

| # | file | native |
|---|---|---|
| 48 | lib.rs | `java/util/Formatter.close` (registrar 1, Compatible) |
| 49 | lib.rs | `java/util/Formatter.flush` (registrar 1, Compatible) |
| 50 | lib.rs | `java/util/Formatter.close` (registrar 2, synthetic) |
| 51 | lib.rs | `java/util/Formatter.flush` (registrar 2, synthetic) |

Deliberately untouched. W7-52-formatter-close-and-locale.md has landed source
for all four in its own worktree, including the `Closeable`/`Flushable`
`instanceof` guards that this lane did not analyse, and repairing them here
would mean a conflicting double-fix. **If that lane does not merge, these four
remain open** — they are the only close/flush swallows on `dev` that this
branch does not resolve.

## Prove the RED

`probes/CloseFlushSwallowProbe.java`. 22 asserted checks plus one printed
observation; every expected value measured on HotSpot 25.0.3.9 before it was
written down, where it prints `RESULT ok` today.

The assertion is always that the failure **arrives**, never that the call
returned — a check that passes because nothing threw is the defect's own shape.
A `Closeable` whose `close()` raises `new Error("close-boom")` must deliver
`java.lang.Error: close-boom` out of `BufferedOutputStream`,
`DataOutputStream`, `InputStreamReader`, `PushbackInputStream`,
`GZIPOutputStream`, `ZipOutputStream`, `PrintWriter` and `StreamHandler`;
`Properties.store` must deliver the `IOException` from the flush that *is* its
byte delivery.

The counter-checks against over-correction sit on the same call sites, because
a fix can go wrong in exactly two directions and only one of them is the one
being repaired:

| check | HotSpot | guards against |
|---|---|---|
| `printWriterCloseAbsorbsIOException` | `none` | turning `catch (IOException x) { trouble = true; }` into a throw |
| `streamHandlerCloseAbsorbsIOException` | `none` | same, for `catch (Exception ex)` |
| `streamHandlerCloseAbsorbsRuntimeException` | `none` | narrowing `Exception` to `IOException` by mistake |
| `filterOutFlushFailureWins` | `java.lang.Error: flush-boom` | reporting the close where the JDK reports the flush |
| `filterOutCloseAttemptedAfterFailedFlush` | `true` | skipping the `finally` close |
| `cleanCloseThrowsNothing` / `cleanCloseDeliveredTheByte` | `none` / `1` | a blanket `?` that throws on success |

`observed.printWriterCheckErrorAfterAbsorb` is printed and **not** asserted: it
is `true` on HotSpot and will be `false` here, because we do not set `trouble`
(residual below). Asserting it would make the probe red for something this lane
did not claim to fix.

**Run it in both modes.** Several of the natives these checks land on are
registered only under `--synthetic-jdk`; in Compatible mode the real bytecode
runs and the same check is green for a reason that has nothing to do with the
repair. A green Compatible-mode run is not evidence that a synthetic-mode body
was fixed.

## A test that was quiet for an unstated reason, tightened not weakened

Four tests in `vm/src/vm/tests.rs` carried a "close is a no-op" comment:
`gzip_output_stream_p70`, `stream_handler_lifecycle_p61`,
`object_output_stream_p70`, `pushback_reader_basics_p66`.

The close is not a no-op — it delegates. Those tests are quiet because their
`<init>` was handed a **null** sink, so the delegation is skipped entirely.
They did not pass *through* the swallow, but the comment said something false
about why they passed, which is the same failure mode one step earlier. Nothing
was removed: each now **asserts** that the sink slot is null, so the reason is
pinned rather than assumed and a later edit cannot hand one a real sink and
silently change what the test means.

## Compatible-mode justification, per change class

Compatible mode is contractually frozen except for genuine HotSpot-parity bug
fixes. Each class here is one, and names the HotSpot behaviour it converges on:

1. **Rows 1–34** — the JDK method declares `throws IOException` and catches
   nothing it does not rethrow. Read from `lib/src.zip`, quoted per row. A
   `close()` that failed reported success; it now reports the failure HotSpot
   reports.
2. **Rows 35–43** — the JDK `catch` is kept, exactly as written, and only its
   width is corrected. An `Error` is not an `IOException` and is not an
   `Exception`; HotSpot lets it out of all nine of these.
3. **Rows 44–47** — no behaviour change at all; comments only.
4. **Ordering** (flush-before-close precedence, `finally` semantics) — matched
   to the JDK body per row, listed above.

## Blast radius

This is a **behaviour-widening** change in aggregate: code that silently worked
may now throw. Ordered by how likely a currently-passing path is to turn into a
throw.

**Highest — anything that closes a stream over a sink that can fail.**

* Rows 25/26/28 (`FilterOutputStream.close`, `DataOutputStream.close`) are the
  widest blast radius in the set: every `java.io` output decorator inherits
  that body, and the flush now propagates where it did not. Any suite that
  closes a wrapper over a stream whose `flush()` was quietly failing will now
  see the failure. **Suites to run: Spring Boot (full), Tomcat, Kafka** — the
  Kafka `MemoryRecordsBuilder` path is named in that native's own comment as
  the reason it exists.
* Rows 14/17/19 (`ZipOutputStream` / `DeflaterOutputStream` /
  `GZIPOutputStream` close) — same argument for archive writing. **Kafka
  (compressed record batches), Spring Boot loader, WildFly deployment.**
* Rows 20/21 (`Properties.store`) — the flush **is** the byte delivery on the
  `BufferedWriter` the JDK wraps the sink in. Anything that stores properties
  to a stream that is not perfectly healthy now fails where it silently
  half-wrote. **Spring Boot, any fixture writing a `.properties` file.**
* Rows 33/34 (`StreamEncoder`) — every `OutputStreamWriter` /
  `PrintWriter(File)` / `Files.newBufferedWriter` bottoms out here. Broadest
  reach of any single row.

**Medium — closes over a sink that is usually null or in-memory.**

* Rows 1/2/3, 7, 10/12/24, 27, 29, 30/31 (reader/input-stream closes). A
  `ByteArrayInputStream` or `StringReader` cannot fail on close, so most
  fixtures are unaffected; a file or socket underneath can.
* Rows 4/5/6, 8/9 (`ObjectOutputStream`) — serialization suites.
* Rows 22/23 (XSLT `StreamResult`) — **the XML suite**. Note these two already
  propagated the `write` and dropped the `flush` beside it, so the site was
  already half-throwing; this makes the two halves agree.

**Low — narrowing only, and only an `Error` newly escapes.**

* Rows 35–43. An `IOException` (rows 35–38) or any `Exception` (rows 39–43) is
  absorbed exactly as before, so no currently-passing path changes **unless it
  was raising an `Error`** — and an `Error` on those paths is almost always a
  `NoSuchMethodError` from our own dispatch, i.e. a real defect this makes
  visible. Expect these to surface *new* failures rather than break working
  ones; each such failure is a registration gap, not a regression.
* Row 43 changes an HTTP error backstop. **Run the net/HTTP suite** — an
  `Error` there now escapes `HttpHandler.handle`.

**None.**

* Rows 44–47 (comments only), row 12 (an unregistered function), and rows
  48–51 (untouched).

## Residual audit, 2026-08-12 — by a lane that owns none of the close/flush files

Re-grepped on this tree (`dev@44044c7e2` plus in-flight lane edits) by the
`--jdk-only` lane holding `classloading/**`,
`native-builtins/src/{classloader_real,classloader_value_sidetable,lookup_define}.rs`.
Nothing built, nothing run. Three results, all verifiable by re-running the
greps quoted:

**1. This record has ZERO residuals inside those ten files.** The scan was
`grep -nE '"(close|flush)"'` over all ten plus a read of every
`ctx.invoke*` in them. The only hits are in `classloading/src/class_manager.rs`
and every one is a `mk("close", "()V")` entry in a **fabricated method table**
(`synthetic_stub_methods`), not a delegation — there is no `ctx` and no call.
`classloader_value_sidetable.rs`'s single `invoke_virtual` propagates with
`?`. So the close/flush family really is confined to `native-builtins/src`,
`native-io`, `native-collections`, `native-awt`, `native-api` and `vm`, as the
census says.

**2. Rows 48–51 are STILL OPEN, and the condition this record set for that has
been met.** The record says the four `java.util.Formatter` sites "remain open"
if `W7-52-formatter-close-and-locale.md` does not merge. It has not:

```
native-builtins/src/lib.rs:21440   let _ = ctx.invoke_virtual(target, "close", "()V", &[]);
native-builtins/src/lib.rs:21455   let _ = ctx.invoke_virtual(target, "flush", "()V", &[]);
native-builtins/src/lib.rs:41498   let _ = ctx.invoke_virtual(target, "close", "()V", &[]);
native-builtins/src/lib.rs:41506   let _ = ctx.invoke_virtual(target, "flush", "()V", &[]);
```

No `Closeable`/`Flushable` `instanceof` guard is present at any of the four;
registrar 1 still uses `ctx.read_string(target).is_none()` as its
StringBuilder discriminator, which is a *value-shape* test of exactly the kind
`docs/architecture/natives-over-real-jdk-classes.md` §4 warns about. These are
in `native-builtins/src/lib.rs`, which another lane holds, so they are an
out-of-file item and not touched here.

**3. The five false positives this record identified are confirmed still
`?`-terminated**, so its arithmetic (51 − 4 + 5 = 52) still reconciles:
`lib.rs:312`, `:326`, `:9725`, `:9966`, `:9985` all read
`let _ = ctx.invoke_virtual*(…)?;`. Rows 44–47's documented kept-whole
swallows are also all present (`classloader.rs:6938`, `lib.rs:26662`,
`logmanager.rs:4403`, `net_phase_e.rs:16245`).

**One cross-record note.** `native-api/src/delegated_close.rs`'s
`absorb_thrown` turned out to be the right shape for a *different* species one
crate over: `W7-26`'s loader ladders needed "absorb the class-absent
throwables, propagate the rest", which is the same decision with two absorbed
roots instead of one. That helper is now duplicated as `absorb_class_absent`
in `native-builtins/src/classloader_real.rs` rather than added to
`delegated_close.rs`, because `native-api` was another lane's file that wave.
If the two are ever merged, the two roots and `absorb_class_absent`'s
`InternalError` residual have to move with it — `delegated_close` propagates
`InternalError` and `absorb_class_absent` deliberately does not, for a reason
stated at its definition.

## Re-verified 2026-08-12 against the working tree, by the P3-D lane

Owned files this pass: `native-io/src/**` plus the three close-family records.
Nothing built, nothing run. Four results.

**1. The `native-io` half of the sweep is LANDED and correct.** Rows 24–34 were
re-read in the tree rather than trusted:

| row | site | today |
|---|---|---|
| 24 | `native_isr_close`, `native-io/src/lib.rs:2599` | `ctx.invoke_virtual(stream, "close", "()V", &[])?` — propagates, with the JDK-body citation in the comment |
| 27 | `native_reader_close`, `native-io/src/lib.rs:10599` | `ctx.invoke_virtual(inner, "close", "()V", &[])?` — propagates |
| 30/31 | `native-io/src/stream_decoder.rs:511`, `:516` | `.map(\|_\| ())` on the `Result`, consumed by the caller — propagates |
| 32 | `native-io/src/stream_encoder.rs:783` | `ctx.invoke_virtual(os, "flush", "()V", &[])?` |
| 33/34 | `native-io/src/stream_encoder.rs:822`, `:824` | bound to `flushed` / `closed` and both inspected — the flush-wins ordering row 33 describes |

`native-api/src/delegated_close.rs` is referenced from **nowhere** in
`native-io`, and that is correct rather than a gap: every `native-io` row in this
record is a *propagate* row, and the helper exists only for the *absorb* rows
(35–43), all of which are in `native-builtins`.

**2. Rows 48–51 are STILL OPEN. The line numbers in the residual audit have
drifted and are re-cited here** so the next grep lands:

```
native-builtins/src/lib.rs:21485   let _ = ctx.invoke_virtual(target, "close", "()V", &[]);
native-builtins/src/lib.rs:21500   let _ = ctx.invoke_virtual(target, "flush", "()V", &[]);
native-builtins/src/lib.rs:41584   let _ = ctx.invoke_virtual(target, "close", "()V", &[]);
native-builtins/src/lib.rs:41592   let _ = ctx.invoke_virtual(target, "flush", "()V", &[]);
```

(was `:21440 / :21455 / :41498 / :41506`). `W7-52-formatter-close-and-locale.md`
has still not merged. Out-of-file for this lane.

**3. A row of the same species that this record's scope excluded, and which is
now FIXED** — `native-io/src/lib.rs`, `native_fos_close` (`:2237`). Not a
delegated-Java-call swallow, so correctly outside the 51, but the identical fault
shape one layer down: `let _ = ctx.fd_table().flush(fd); let _ = ctx.fd_table()
.close(fd);` where `java.io.FileOutputStream.close()` declares `throws
IOException` and catches nothing. Our writer entries are `BufWriter`s — HotSpot's
`FileOutputStream` is unbuffered — so **the flush is the byte delivery** and
dropping it is lost data reported as success, which is what this record exists
for. It now propagates for `fd >= 3`, flush-failure-wins with the close still
attempted; `fd < 3` keeps the swallow because `FdTable::close` answers `Ok(())`
there anyway and the flush half would be flushing the shared process console.
Its own neighbour `native_fos_flush` (`:2224`) already propagated, so this body
was the odd one out rather than a considered policy.

**4. And a SECOND site for the same defect that no record had counted** —
`native_fd_close0`, `native-io/src/lib.rs:1846`. This is the body the
**Compatible / real-JDK arm actually reaches**: the real
`FileOutputStream.close()` bytecode routes through `FileDescriptor.closeAll` →
`close()` → `close0()`. Repairing only `native_fos_close` would have fixed the
fallback and left the shipping default swallowing while the row came off the
census — the shape this family's records keep refusing. Only the **flush** half
is propagated there, deliberately: the body is registered for three receivers,
one of which is `sun/nio/ch/UnixDispatcher.close0` on sockets, and
`FdTable::flush` ends in `_ => Ok(())` for every non-writable entry, so the
blast radius outside buffered file writers is provably empty. The close half
stays swallowed and is named, not quietly counted.
`native_fis_close` (`:1903`) has the same swallowed close and was left alone for
the same reason — a read-side close cannot lose buffered data.

## What is left

* **Rows 48–51 — the four `java.util.Formatter` sites**, if
  W7-52-formatter-close-and-locale.md does not merge.
* **`trouble` / `checkError()`.** HotSpot records the absorbed `IOException` in
  `PrintWriter`/`PrintStream`'s `trouble` field and `checkError()` reports it.
  We drop it, so an absorbed `IOException` is **unobservable** rather than
  merely unpropagated. `observed.printWriterCheckErrorAfterAbsorb` in the probe
  is that gap, measured `true` on HotSpot. Same shape as the `lastException` /
  `ioException()` residual W7-52 left on `Formatter`, and the same reason:
  writing the field is a two-mode decision about a synthetic layout.
* **`ErrorManager`.** `StreamHandler` routes its absorbed `Exception` to the
  handler's `ErrorManager`; rows 39–41 drop it. Same class of gap.
* **`addSuppressed`.** Rows 25/26, 33/34 and 19 reproduce the JDK's *precedence*
  between a failing flush and a failing close but not the suppressed-exception
  link between them. A caller reading `getSuppressed()` sees an empty array
  where HotSpot has one entry.
* **Row 40/41 ordering.** HotSpot's `flushAndClose` runs the flush and the
  close inside ONE `try`, so a failing flush skips the close. `absorb_exception`
  answers `Ok(None)` for a clean void return and for an absorbed exception
  alike, so that branch is not reconstructible at the call site; the close is
  attempted either way, which for a logging sink is the safer of the two.
* **`native_input_stream_reader_close` (row 12) is dead** — no `register` call
  names it anywhere in the workspace. Repaired for consistency with the two
  live `InputStreamReader.close` bodies rather than left as the odd one out;
  the deadness is recorded, not resolved.
* **The other ~271 non-close/flush swallow sites** W7-52 counted, plus the 213
  bound-but-uninspected calls named above. Out of scope here by design: this
  lane took close/flush because that is where a dropped error means lost data.
* Nothing in this record has run on a VM. The next lane with a build should run
  `probes/CloseFlushSwallowProbe.java` under `--real-jdk` **and**
  `--synthetic-jdk` and expect `RESULT ok` in both, then run the suites named
  under Blast radius.
