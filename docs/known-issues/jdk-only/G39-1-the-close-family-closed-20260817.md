# G39-1 — the open half of the close/flush family, and the over-correction the sweep before it left behind

**Status:** oracle arm **MEASURED** on HotSpot 25.0.3+9-LTS (Temurin, Windows 11,
NTFS), 2026-08-17. Reachability arm **MEASURED** on CratonVM with
`--dump-native-registry --jdk-only`, including live `invocations` counts.
CratonVM behavioural arm **PREDICTED**: no `cargo` command of any kind was run,
the available binary (`C:/craton/target-rel2/release/cratonvm.exe`, from
`9964ca733`) predates these edits, and nothing below claims to have observed
this source running. `target-rel3` was checked for and has no `release/*.exe`.

That split is stated first because `HANDOFF-20260814.md` §2 says *"a prediction
is not a result"*, and because this record's largest single finding is a
**divergence that a previous lane's fix introduced** while repairing the
opposite one. The pairing discipline — every refusal test written next to its
non-refusal — is not decoration here; it is what the whole record is about.

Lane scope, two files, nothing else edited:

* `native-io/src/lib.rs`
* `native-builtins/src/phases_late/io_streams.rs`

Everything found outside them is in [§8 NOMINATIONS](#8-nominations).

Parent records: `W7-70-printstream-close-noop.md` (three named residuals, two of
them in this lane's files), `W7-57-close-flush-swallow-sweep.md`,
`W7-53-blocking-close-family.md`, `W7-64-printstream-trouble-and-errormanager.md`,
and `G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md`, whose
after-close repairs this record both extends and corrects.

---

## 0. The headline

| | before (source) | after (source) | reach |
|---|---|---|---|
| `fos.write(b, 0, 0)` after close | `IOException: Stream Closed` | `void` | **`--jdk-only`** |
| `fis.read(new byte[0])`, open OR closed | `-1` (EOF) | `0` | Compatible/synthetic |
| `fis.skip(0)` after close | `0` | `IOException: Stream Closed` | **`--jdk-only`** |
| `FileDescriptor.close0` close failure | dropped | propagated | **`--jdk-only`, invocations=1 measured** |
| `FileInputStream.close` close failure | dropped | propagated | Compatible/synthetic |
| `pi.read()/available()/unread()` after close | kept working on the closed stream | `IOException: Stream closed` | synthetic only |
| `pi.close(); pi.close();` | closed the wrapped stream **twice** | once | synthetic only |
| `pr.close(); pr.close();` | closed the wrapped reader **once** | twice | synthetic only |

The last two rows disagree with each other on purpose. That is measured, it is
the single least derivable thing in this record, and [§4.2](#42-the-two-sibling-classes-disagree-about-double-close-and-that-is-the-measurement)
is about nothing else.

---

## 1. Reachability, established with the dump before anything was adjudicated

`--dump-native-registry` was run in `--jdk-only` before a line was changed, per
`G4-1` §10 item 1 and this lane's own brief. Three of its answers are
load-bearing and **two of them contradict a comment that is currently in the
tree.**

### 1.1 `native_fd_close0` does NOT serve a socket dispatcher

`native-io/src/lib.rs` registers `native_fd_close0` twice — once for
`java/io/FileDescriptor.close0()V` and once for
`sun/nio/ch/UnixDispatcher.close0(Ljava/io/FileDescriptor;)V`. The body's own
comment used the second registration as the stated reason **not** to propagate a
close failure: *"this body is registered for three different receivers, one of
which is a socket dispatcher"*.

The dump says otherwise:

```
sun/nio/ch/UnixDispatcher.close0 (Ljava/io/FileDescriptor;)V
    registered_by native-io/src/lib.rs:6361   owns_slot: FALSE
sun/nio/ch/UnixDispatcher.close0 (Ljava/io/FileDescriptor;)V
    registered_by native-io/src/net.rs:4178   owns_slot: TRUE
java/io/FileDescriptor.close0 ()V
    registered_by native-io/src/lib.rs:6342   owns_slot: TRUE
```

`net.rs` registers the same triple later and wins. **`native_fd_close0` serves
exactly one triple, and there is no socket on the other end of it.** The stated
blocker for W7-70's residual did not exist. This is the same lesson `F41-1` §2
and `G4-1` §2.1 each record independently: the registry dump, not source
reading, settles ownership — and here it settled it in the direction of *doing*
the work rather than deferring it.

### 1.2 The body is not merely registered, it RUNS in strict mode

`owns_slot` is trustworthy and `invocations == 0` proves nothing, so the
invocation counts that are non-zero are worth more than either. Measured under
`--jdk-only` on the existing binary, running `RFileTimes`:

| triple | body | invocations |
|---|---|---|
| `java/io/FileDescriptor.close0()V` | `native_fd_close0` | **1** |
| `java/io/FileOutputStream.close()V` | `native_fos_close` | **7** |
| `java/io/FileOutputStream.write(I)V` | `native_fos_write_byte` | **658** |
| `java/io/FileOutputStream.write([BII)V` | `native_fos_write_bytes` | **24** |
| `java/io/FileOutputStream.write([B)V` | `native_fos_write_byte_array` | **1** |
| `java/io/FileInputStream.read([BII)I` | `native_fis_read_bytes` | **13** |
| `java/io/FileInputStream.available0()I` | `native_fis_available` | **10** |

`RJdkNio` independently drives `FileDescriptor.close0` once. So the two
repairs with the widest blast radius in this record are on bodies a **currently
green `--jdk-only` vector already executes**, not on bodies inferred to be live.

### 1.3 `FileInputStream.close()V` has NO `--jdk-only` reach, and `FileOutputStream.close()V` does

The `--jdk-only` dump lists twelve `java/io/FileInputStream` rows and **`close`
is not among them**: its registration sits inside the
`NativeKind::SyntheticStub` block in `register_io_natives`, which strict mode
drops. `java/io/FileOutputStream.close()V` is registered `Bridge` outside that
block and survives.

This asymmetry is not in any record, and it changes how W7-70's residual list
reads: of its two open close halves, `native_fd_close0` is the shipping one and
`native_fis_close` is Compatible/synthetic only. Both are repaired here; only
the first can move a strict-mode vector, and the source now says so at each
site rather than leaving the census to infer it.

### 1.4 `io_streams.rs` is confirmed synthetic-only — by measurement, not by list

`G4-1` §2 states this from `SYNTHETIC_ONLY_CLOSURE`. The dump confirms it
independently: under `--jdk-only` there are **zero** registrations for
`java/io/PushbackInputStream`, `java/io/PushbackReader` and
`java/io/ObjectOutputStream` (the single `java/io/ObjectInputStream` row is
`resolveProxyClass`, from `reflect_annotations.rs`). Everything in
[§4](#4-what-changed-in-io_streamsrs) is Compatible/`--synthetic-jdk` only and
**cannot move a `--jdk-only` vector**. It is fixed anyway because the defects
are real, in-file and cheap; it is not counted as strict-mode progress.

---

## 2. The measured oracle tables

All rows MEASURED on `openjdk 25.0.3 2026-04-21 LTS` (`Temurin-25.0.3+9-LTS`),
Windows 11, NTFS, 2026-08-17. `msg=null` means `getMessage()` returned null; an
empty message would print as `msg=[]`. Labels are ASCII throughout
(`HANDOFF-20260814.md` §7). Probe sources are in [§9](#9-probe-sources).

### 2.1 The precedence order, which is four checks deep and was not known

This is the table this record exists for. On a **closed** stream:

| call | oracle result |
|---|---|
| `fis.read(b, 0, 0)` | `RET [0]` |
| `fis.read(new byte[0])` | `RET [0]` |
| `fis.read(b, -1, 2)` | `THROW java.lang.IndexOutOfBoundsException msg=null` |
| `fis.read(b, 0, 99)` | `THROW java.lang.IndexOutOfBoundsException msg=null` |
| `fis.read(b, 2, -1)` | `THROW java.lang.IndexOutOfBoundsException msg=null` |
| `fis.read(null, 0, 1)` | `THROW java.lang.NullPointerException msg=null` |
| `fis.read((byte[]) null)` | `THROW java.lang.NullPointerException msg=[Cannot read the array length because "b" is null]` |
| `fis.read()` | `THROW java.io.IOException msg=[Stream Closed]` |
| `fos.write(b, 0, 0)` | `RET [void]` |
| `fos.write(new byte[0])` | `RET [void]` |
| `fos.write(b, -1, 2)` / `(b, 0, 99)` / `(b, 2, -1)` | `THROW java.lang.IndexOutOfBoundsException msg=null` |
| `fos.write(null, 0, 1)` | `THROW java.lang.NullPointerException msg=null` |
| `fos.write(65)` | `THROW java.io.IOException msg=[Stream Closed]` |
| `fos.flush()` | `RET [void]` |
| `fis.skip(0)` | **`THROW java.io.IOException msg=[Stream Closed]`** |
| `fis.skip(-5)` | **`THROW java.io.IOException msg=[Stream Closed]`** |
| `fis.skip(1)` | `THROW java.io.IOException msg=[Stream Closed]` |

**null array → bounds → zero length → closed**, on both sides of the family, and
`skip` is exempt from the third step entirely. The structural reason is in
HotSpot's `io_util.c`: `readBytes`/`writeBytes` do their null check, then their
bounds check, then `if (len == 0) return …;`, and only then read the descriptor
and raise `"Stream Closed"` for `fd == -1`; `skip0` reads the descriptor as its
first act and never looks at `toSkip` until after.

None of the four steps is derivable from the other three. All four were measured.

### 2.2 Double close, flush after close, write after close

| call | oracle result |
|---|---|
| `fis.close()` ×3 | `RET [void]` each |
| `fos.close()` ×2, then `fos.flush()` ×2 | `RET [void]` each — **flush after close does NOT throw** |
| bytes written before close, after close | still on disk (`Files.size == 2`) |
| `fis.getFD().valid()` / `fos.getFD().valid()` after close | `RET [false]` |
| `fd.sync()` after close | `THROW java.io.SyncFailedException msg=[sync failed]` |
| `raf.close()` ×2 | `RET [void]` each |
| `raf.read()` / `write(65)` / `length()` / `seek(0)` / `getFilePointer()` / `setLength(1)` after close | all `THROW java.io.IOException msg=[Stream Closed]` |
| `bw` / `osw` / `isr` / `br` / `lnr` / `bis` / `sr` / `pi` / `pr` after close | all `THROW java.io.IOException msg=[Stream closed]` — lower-case `c` |
| `bos` / `dos` over a `ByteArrayOutputStream`: `write` / `flush` after close | `RET [void]` — no closed check at all |
| `dis.readInt()` after close | `RET [1633837924]` — reads on |
| `baos` / `bais` / `sw` after close | `RET [void]` / reads and writes still work |
| `Files.newInputStream(p)` after close: `read` / `available` | `THROW java.nio.channels.ClosedChannelException msg=null` |
| `Files.newOutputStream(p)` after close: `write` / `flush` | `ClosedChannelException` / **`RET [void]`** |
| socket `out.write` / `in.read` after `socket.close()` | `THROW java.net.SocketException msg=[Socket closed]` |
| socket `out.flush()` / `out.close()` after `socket.close()` | `RET [void]` |
| `socket.getOutputStream()` after close | `THROW java.net.SocketException msg=[Socket is closed]` |
| `piped: po.close(); pin.read()` | `RET [-1]` |
| `piped: pin.close(); pin.read()` | `THROW java.io.IOException msg=[Pipe closed]` |

Two capitalisations, two families, one run:
`FileInputStream`/`FileOutputStream`/`RandomAccessFile` say **`Stream Closed`**;
the `Reader`/`Writer`/pushback family says **`Stream closed`**. Confirmed again
here; the two strings must not be shared.

### 2.3 Close over a handle that already failed

| call | oracle result |
|---|---|
| `fos.getChannel().close()` then `fos.write(65)` | `THROW java.io.IOException msg=[Stream Closed]` |
| … then `fos.close()`, and again | `RET [void]`, `RET [void]` |
| … bytes written before | still on disk |
| `fis.getChannel().close()` then `fis.read()` | `THROW java.io.IOException msg=[Stream Closed]` |
| … then `fis.close()` | `RET [void]` |

A close whose descriptor is already gone is a **clean void**, not an error. This
is the control that keeps the two propagation repairs in
[§3.3](#33-both-close-halves-now-propagate--w7-70s-residual-closed) from
becoming a new divergence: `FdTable::close` answers `Ok(())` for an fd that is
not in the table, which is exactly this row.

### 2.4 Propagation and suppression — and where W7-57's stated JDK body is now wrong

| call | oracle result |
|---|---|
| `bos.flush()` over a failing sink | `THROW java.io.IOException msg=[write boom] suppressed=0` |
| `bos.close()` | `THROW IOException msg=[close boom] suppressed=1 {IOException:write boom}` |
| `bos.close()` again | `RET [void]`; sink close count stays `1`, flush count `0` |
| `dos.close()` over a failing sink | `THROW IOException msg=[close boom] suppressed=2 {write boom, write boom}` |
| **`FilterOutputStream.close()`** over a failing sink | **`THROW IOException msg=[close boom] suppressed=1 {IOException:flush boom}`** |
| … sink flush count / close count | `1` / `1` — the `finally` close runs |
| `FilterOutputStream.close()` again | `RET [void]`; close count stays `1` |
| `bw.flush()` over a failing `Writer` | `THROW IOException msg=[w write boom] suppressed=0` |
| `bw.close()` | `THROW IOException msg=[w write boom] suppressed=1 {IOException:w close boom}` |
| close over a sink whose `flush` throws `Error` | `THROW java.lang.Error msg=[close ERROR] suppressed=1 {Error:flush ERROR}`; both ran |
| `try (r) { throw ISE }`, close throws | `THROW IllegalStateException msg=[body boom] suppressed=1 {IOException:close boom}` |
| `try (r) { ok }`, close throws | `THROW IOException msg=[close boom] suppressed=0` |
| `try (a; b)`, both closes throw | `THROW IOException msg=[close B] suppressed=1 {IOException:close A}` |
| `AutoCloseable`/`Closeable` closed 3× | delegate ran **3 times** — there is no built-in idempotence |

**`W7-57`'s row 25/26 states the wrong JDK body, and its §"Ordering was
preserved" bullet states the wrong winner.** It records
`FilterOutputStream.close()` as `try{flush()} catch(Throwable){rethrow}
finally{out.close()}` and says *"the flush failure **wins**"*. On JDK 25 the
measured answer is the opposite: the **close** exception is thrown and the flush
exception is attached as suppressed. `BufferedOutputStream` agrees with
`FilterOutputStream`; the `Writer` family does not (there the flush wins). Both
were measured in one run.

This is not acted on — see [§7](#7-what-this-lane-did-not-do) — because
CratonVM has no `addSuppressed` plumbing on these paths and reporting one of two
failures is what it already does. It is recorded because W7-57's table is
currently a citation people will trust, and its claim about this class is
falsified.

**`Closeable` idempotence is not a language guarantee.** `close()` called three
times on a lambda `Closeable` ran the body three times. Every "closing twice is
safe" property in this family is a per-class property, and [§4.2](#42-the-two-sibling-classes-disagree-about-double-close-and-that-is-the-measurement)
is the demonstration that two sibling classes in one package differ.

### 2.5 `PrintStream` / `PrintWriter` — the error flag across `close()`

| call | oracle result |
|---|---|
| `ps.checkError()` before any write, sink fails | `RET [true]` — `checkError()` flushes first |
| `ps.println` / `flush` / `close` over a failing sink | `RET [void]` each — never throws |
| `ps.checkError()` after close | `RET [true]` — **the flag survives `close()`** |
| flag already `true`, then `close()` | still `true` — **`close()` does not clear it** |
| clean stream: `checkError()` initial / after a good write / after a clean `close()` | `false` / `false` / **`false`** |
| clean stream: sink ops after `close()` | `flush,flush,flush,flush,flush,close` — one close |
| `ps.println("after")` **after** the clean close | `RET [void]`, and **no bytes reach the sink** |
| `ps.checkError()` after that | **`RET [true]`** |
| `ps.close()` a second time | `RET [void]`; sink close count stays `1`, flush count unchanged |
| `ps.flush()` after close | `RET [void]`, flag stays `true` |
| `checkError()` on an OPEN stream | drives one sink `flush` |
| `checkError()` on a CLOSED stream | drives **zero** sink flushes |
| `pw.close()` over a failing `Writer`, twice | inner close count **`2`** — `PrintWriter` has no `closing` latch |
| `pw2` clean: `checkError()` after clean close / after a write-after-close | `false` / **`true`** |

The assertable contract, in one line, and it is two-sided so it cannot pass
vacuously: **a clean `close()` leaves `checkError()` false; the first write after
that close makes it true, delivers nothing, and does not throw.**

Two rows settle open questions in W7-70's own **What is left**:
`observed.printStreamBytesWrittenAfterClose` is `0` (measured, sink byte counter
unchanged across the post-close `println`), and
`observed.printStreamCheckErrorOnClosedStreamReflushed` is `0` (measured, sink
flush counter unchanged across `checkError()` on a closed stream). No
`PrintStream` body is in this lane's two files — `native_printstream_close` is
`logging_shims.rs` — so this is [nomination N1](#8-nominations) with the
measurement attached rather than a fix.

### 2.6 W7-64's run banner versus its own FIXED rows

`W7-64-printstream-trouble-and-errormanager.md` was read sceptically, as
instructed. Its banner says **"source landed, UNVERIFIED against a VM"** and
"nothing here has been built", while its body marks rows FIXED. Read together,
"FIXED" in that record means *source was written*, not *behaviour was observed* —
the same equivocation `HANDOFF-20260814.md` §2 exists to stop. Nothing in this
lane depends on any W7-64 row being true of a running VM; the two things it
would have supplied (the `trouble` flag's `close()` behaviour, and whether
`checkError()` re-flushes a closed stream) are **measured here instead**, in
§2.5, and can be asserted from Java by anyone with a binary. `W7-70`'s
2026-08-12 re-verification of `native_printstream_close` is the one claim in the
family that was re-read against source rather than against a record, and it
holds.

### 2.7 Channels — close, double close, and close during a blocking call

| call | oracle result |
|---|---|
| `FileChannel.open(...)` class | `sun.nio.ch.FileChannelImpl` |
| `fc.close()` ×2 | `RET [void]`, `RET [void]`; `isOpen()` `false` |
| `fc.read/write/size/position/force/truncate/lock/tryLock/map` after close | all `THROW java.nio.channels.ClosedChannelException msg=null` |
| `AsynchronousFileChannel.open(...)` class | `sun.nio.ch.WindowsAsynchronousFileChannelImpl` |
| `afc.close()` ×2 | `RET [void]` each |
| `afc.force(true)` / `size()` / `truncate(1)` / `tryLock()` after close | `THROW ClosedChannelException msg=null` |
| `afc.lock()` after close | `RET [sun.nio.ch.CompletedFuture]` — **no throw** |
| `afc.lock().get()` after close | `THROW ExecutionException cause=ClosedChannelException` |
| `afc.read(buf,0)` after close / `.get()` | `CompletedFuture` / `ExecutionException cause=ClosedChannelException` |
| **blocking `ssc.accept()` woken by another thread's `close()`** | **`THROW java.nio.channels.AsynchronousCloseException msg=null`** |
| **blocking `ssc.accept()` woken by `Thread.interrupt()`** | **`THROW java.nio.channels.ClosedByInterruptException msg=null`**, `isOpen()` then `false` |
| **blocking `SocketChannel.read()` woken by `close()`** | `AsynchronousCloseException msg=null` |
| **blocking `SocketChannel.read()` woken by `interrupt()`** | `ClosedByInterruptException msg=null`, `isOpen()` `false` |
| **blocking `Pipe.source().read()` woken by `close()`** | `AsynchronousCloseException msg=null` |
| `pipe.source().read()` / `sink().write()` after close | `ClosedChannelException msg=null` |
| `ssc.accept()` / `sc.read` / `sc.write` after close | `ClosedChannelException msg=null` |
| `dc.receive` / `read` after close | `ClosedChannelException msg=null` |
| `dc.socket().setSoTimeout(-1)` on a **closed** socket | `THROW java.net.SocketException msg=[Socket is closed]` |

The last row refines `G4-1` §3.5 rather than contradicting it: on an **open**
adaptor a negative timeout is `IllegalArgumentException("timeout < 0")` (G4-1's
measurement); on a **closed** one the closed check runs first. Both are true and
the order is the fact. That matters for [nomination N5](#8-nominations), which
would otherwise be implemented with the checks in the wrong order.

### 2.8 The `io_streams.rs` surface

| call | oracle result |
|---|---|
| `pi.close(); pi.close();` | wrapped stream's `close()` count stays **`1`** |
| `pi.read()` / `available()` / `unread(65)` / `skip(1)` after close | all `THROW java.io.IOException msg=[Stream closed]` |
| … wrapped stream's `read()`/`available()` counts after those calls | **UNCHANGED** — the refusals never reach it |
| `pi.mark(1)` / `markSupported()` after close | `RET [void]` / `RET [false]` |
| `pi.reset()` after close | `THROW java.io.IOException msg=[mark/reset not supported]` |
| `pr.close(); pr.close();` | wrapped reader's `close()` count is **`2`** |
| `pr.read()` / `ready()` / `unread('A')` after close | `THROW java.io.IOException msg=[Stream closed]` |
| `oos.close(); oos.close();` | sink close count **`2`**, sink re-flushed |
| `oos.writeInt/writeObject/writeUTF/flush/reset` after close | `RET [void]` each, and the bytes **reach the sink** |
| `new ObjectOutputStream(<failing sink>)` | `THROW java.io.IOException msg=[oos write boom]` — the header write |
| `oos.close()` over a sink whose flush AND close throw | `THROW IOException msg=[late flush boom]` — the flush wins |
| `ois.close(); ois.close();` | quiet |
| `ois.readInt/readUTF/readByte/readBoolean` after close | `THROW java.io.EOFException msg=null` |
| `ois.readObject()` after close | `RET [obj]` — succeeds |
| `ois.available()` after close | `RET [0]` |

The `ObjectOutputStream`/`ObjectInputStream` rows are **control rows and were
acted on by NOT acting**: HotSpot's own `Object*Stream` has no closed refusal in
this configuration, its close is not idempotent, and CratonVM's stubs already
behave that way. A "closed latch" here would have been an invented divergence
that looked like a fix. Its `close()` ordering — flush wins over close — is also
already what the tree does. See [§7](#7-what-this-lane-did-not-do).

---

## 3. What changed in `native-io/src/lib.rs`

### 3.1 The over-correction `G4-1` left, and why it is the most important row here

`G4-1` §5.1 rows 14–18 repaired the purest fabricated success in the tree: five
`FileOutputStream` write bodies that returned `void` having written nothing to a
closed stream. That repair is right and stays. But it put the closed check
**first**, and the oracle row it did not have says a zero-length write answers
**before** the descriptor is consulted:

```text
CLOSED fos.write(b,0,0)      | RET [void]
CLOSED fos.write(new byte[0])| RET [void]
```

So `out.write(buf, 0, 0)` on a closed stream began throwing where HotSpot returns
quietly. That is the **opposite direction** from the defect being repaired, and
it is exactly the failure mode `W7-57`'s "Prove the RED" section pairs every
recording row against and `W7-70` repeats: *"a fix can go wrong in exactly two
directions and only one of them is the one being repaired."*

The rule is now one named predicate, `is_empty_transfer`, with the full
four-step precedence order and its `io_util.c` mechanism in one doc comment,
cited from all five call sites rather than re-derived at each:

| # | site | before | after | reach |
|---|---|---|---|---|
| 1 | `native_fos_write_bytes` (`write([BII)V`) | `len == 0` fell through to the closed check → threw | answers `void` first | **`--jdk-only`**, 24 invocations measured |
| 2 | `native_fos_write_bytes_ignore_append` (`writeBytes([BIIZ)V`) | same | same | **`--jdk-only`** (`owns_slot`) |
| 3 | `native_fos_write_byte_array` (`write([B)V`) | an EMPTY array threw | answers `void` first | **`--jdk-only`**, 1 invocation measured |
| 4 | `native_fis_read_bytes` (`readBytes`, `read([BII)`) | already correct | comment only, cites the rule | — |
| 5 | `native_fis_read_byte_array` (`read([B)`) | an EMPTY array answered **`-1`** on an OPEN stream too | answers `0` first | synthetic |

Row 5 is a second, independent defect that this row of the oracle exposed:
`read(new byte[0])` allocated a zero-length buffer, got `Ok(0)` back from
`read_bytes`, and hit the `n == 0 => -1` arm — so it reported **end of stream on
a stream that had not been read at all**, open or closed. Fabricated EOF, reached
by the argument rather than by the descriptor.

### 3.2 `skip` has no zero-length escape hatch — and `skip(0)` is how you poll

| # | site | before | after | reach |
|---|---|---|---|---|
| 6 | `native_fis_skip` (`skip0(J)J`) | `if n <= 0 { return 0 }` ran **before** the closed check | descriptor first, then the count | **`--jdk-only`** (`owns_slot`) |

`G4-1` row 13 added the closed refusal to this body but left it below the
`n <= 0` shortcut, so the one spelling most likely to be used as a liveness poll
— `skip(0)` — still answered `0` on a closed stream. Measured: HotSpot throws
`Stream Closed` for `skip(0)`, `skip(-5)` and `skip(1)` alike, because `skip0`
reads the descriptor as its first act.

Deliberately **not** widened at the same site: an OPEN stream keeps its `0` for a
non-positive count. HotSpot's `skip0` implements a negative count as a backward
`lseek`, which this forward-only `BufReader` has no counterpart for; the repair
is about *when the descriptor is consulted*, not about growing a backward seek.
Named at the site.

### 3.3 Both close halves now propagate — W7-70's residual CLOSED

| # | site | before | after | reach |
|---|---|---|---|---|
| 7 | `native_fd_close0` | flush propagated, **close dropped** | `flushed.and(closed)` — the flush wins, the close is attempted either way | **`--jdk-only`, invocations = 1 measured** |
| 8 | `native_fis_close` | `let _ = close(fd)` | propagated, raised **after** the closed marker is written | Compatible/synthetic only ([§1.3](#13-fileinputstreamclosev-has-no---jdk-only-reach-and-fileoutputstreamclosev-does)) |

Row 7's premise was refuted by the dump ([§1.1](#11-native_fd_close0-does-not-serve-a-socket-dispatcher)). With no socket
receiver, the blast radius is the same provably-empty one the flush half already
argued: `FdTable::close` returns `Ok(())` for `fd < 3` outright, and its match
ends in `_ => Ok(())` for every entry kind that is not `FileWrite` or
`ChildStdinPipe`. The only thing that can now surface is a buffered writer whose
final flush fails at close — which is precisely the disk-full-at-close that
`java.io.FileOutputStream.close()` declares `throws IOException` for. The body
now agrees with its sibling `native_fos_close`, which has had exactly this shape
since 2026-08-12.

Row 8 is landed with its own limitation stated rather than hidden: on the read
side it **cannot fire**, because a `FileInputStream` resolves to a `FileRead`
entry whose close arm is `Ok(())`. It is landed because a discarded `Result`
reads as a decision and this one was not one, and because `fis_get_fd` also
answers for the legacy synthetic slot-0 layout, where nothing constrains the
entry kind.

Both raise **after** writing the closed marker, for the reason HotSpot's
`FileDescriptor.closeAll` latches `closed = true` before running the delegate: a
retry after a failing close must not re-close, and §2.2's double-close row must
stay a clean `void`.

**Not touched, deliberately:** `native_fos_flush` still answers `Ok(None)` for a
missing descriptor. §2.2 measures `fos.flush()` after close as `RET [void]` while
`bw.flush()` and `osw.flush()` throw. That is the control row that keeps this
from being a mass edit, and there is now a unit test pinning it.

---

## 4. What changed in `io_streams.rs`

**No row in this section has `--jdk-only` reach** ([§1.4](#14-io_streamsrs-is-confirmed-synthetic-only--by-measurement-not-by-list)). Stated here, at the
top of the section, so a green strict-mode run is not read as evidence about it.

### 4.1 `PushbackInputStream.close()` did not close anything, in the sense that matters

| # | site | before | after |
|---|---|---|---|
| 9 | `close()` | delegated (propagating, per W7-57 row 1) and then **performed neither nulling** | HotSpot's `if (in == null) return; in.close(); in = null; buf = null;` in full |
| 10 | `read()` | fell through to the delegate arm and answered **`-1`** on a closed stream | `IOException("Stream closed")` |
| 11 | `available()` | answered a number, and reached the wrapped stream to get it | `IOException("Stream closed")` |
| 12 | `unread(I)` | mutated the buffer of a closed stream | `IOException("Stream closed")`, checked **before** the overflow test |

W7-57 fixed the *propagation* of this `close()` and nothing else; the two
nullings are not bookkeeping, they are the entire closed state of the class.
Without them the class had no closed state at all, so:

* `close()` was **not idempotent** — a second call re-closed the wrapped stream
  (oracle: the inner close count stays `1`);
* a post-close `read()` answered `-1`, the fabricated EOF this whole campaign is
  named for, one package over from `native_fis_read`;
* worse, with a sink whose own `close()` is a no-op — `ByteArrayInputStream`, the
  commonest thing to wrap — a post-close `read()` went on returning **real
  bytes** from a stream the caller had closed, and a post-close `unread()` went
  on mutating its buffer. Not a missing diagnostic; a working stream after close.

The closed marker is **slot 1 (`buf`)**, not slot 0 (`in`), even though HotSpot's
own early return keys on `in`. HotSpot nulls the pair together so the two are
interchangeable for the purpose; slot 0 is legitimately null on a receiver built
with no wrapped stream, which is exactly what `pushback_input_stream_p58` in
`vm/src/vm/tests.rs` constructs, and keying on it would declare that fixture
closed before it had been. It is also the convention the `PushbackReader` half of
the same file already uses, so the two classes now agree on the mechanism while
disagreeing — correctly — on the behaviour.

`unread`'s ordering is its own row: `ensureOpen()` is the first statement of the
JDK's `unread`, so a closed stream with a full buffer reports `Stream closed`,
not `Push back buffer is full`. Both messages exist, both are measured, and
which one you get is an ordering fact.

### 4.2 The two sibling classes disagree about double close, and that is the measurement

| # | site | before | after |
|---|---|---|---|
| 13 | `register_p66_pushback_reader`'s `close()` | nulled slot 0, so a second close was a silent no-op | slot 0 left alone; a second close reaches the wrapped `Reader` again |

```text
pr.close(); pr.close();  -> inner Reader.close() count = 2
pi.close(); pi.close();  -> inner InputStream.close() count = 1
```

Same package, same session, same probe. `PushbackInputStream.close()` early-returns
on `in == null`; `java.io.PushbackReader.close()` is
`synchronized (lock) { super.close(); buf = null; }` and never touches `in` at
all, so `FilterReader.close()`'s bare `in.close()` runs again.

Nulling slot 0 was correct-looking and wrong: it made the second close silent,
which matters for any wrapped stream whose `close()` is not idempotent — a
counting or refcounting sink saw one release where HotSpot delivers two. The
closed marker stays slot 1, which `read`/`ready`/`unread` already key on, so
refusing the post-close surface does not depend on slot 0 being cleared.

This is the row that would have been "unified" by anyone tidying the two classes
into a shared helper, and it is why the record spends a section on it.

### 4.3 One dead body, named rather than deleted

`register_p58_pushback` also registers five `java/io/PushbackReader` triples that
`register_p66_pushback_reader` overwrites later in `register_synthetic_overrides`
(phase58 then phase66, last-write-wins). None of them dispatches. They are left
in place — deleting them is a registration change and this lane is not making one
— and the `close` body now says so at the site, so the next reader does not repair
a body that cannot run. That is exactly what `G4-1` found had already happened to
`p58_pushback_reader_read`.

Two `close` bodies were lifted out of their registrar closures into named
`pub(crate)` functions (`p58_pushback_in_close`, `p66_pushback_reader_close`) so
`mod tests` can pin them. **The registrations are unchanged** — same triples,
same order, same `NativeKind` — so no registry slot moved.

---

## 5. Which of W7-53's seven open sites this lane closed: NONE, and why that is the right answer

W7-53's seven are `DatagramChannel.receive`; four TLS stream sites; the
multi-acceptor race in `s2_blocking_accept`; and the **Windows arm of the pipe
sink write**. Re-derived from the record and checked against the tree:

| open site | file | in this lane? |
|---|---|---|
| `DatagramChannel.receive` | `native-builtins/src/phases_late/net_channels.rs` | no |
| `s2_tls_read_direct`, `s2_tls_write` | `native-builtins/src/servlet.rs` | no |
| the other two TLS sites | `native-builtins/src/t27_tls.rs` | no |
| `s2_blocking_accept` multi-acceptor race | `native-builtins/src/servlet.rs` | no |
| **Windows pipe sink write** | **`native-io/src/pipe.rs`** | **no** |

The last row is the one worth spelling out, because it is the near miss. This
lane owns `native-io/src/lib.rs`; `pipe.rs` is a **sibling module file**
(`pub mod pipe;` at `lib.rs:111`), not part of it. W7-53's own narrowing section
records that `native-io/src/pipe.rs` was edited by the lane that wrote that
update and that **the row still says open**, needing
`CreateNamedPipe(FILE_FLAG_OVERLAPPED)` plus a bounded `GetOverlappedResultEx` —
a change to how the pipe is *created*, which W7-53 explicitly calls "not landable
on inspection."

`G4-1` §6 reached the same conclusion for its own three files. Two independent
derivations, same answer. **Zero of the seven closed**, and each is
[nominated](#8-nominations) rather than counted.

What this lane *did* contribute to that family is the oracle it was missing:
§2.7 measures all three blocking-close outcomes end to end —
`AsynchronousCloseException` for a close from another thread,
`ClosedByInterruptException` for `Thread.interrupt()` with `isOpen()` then
`false`, and `ClosedChannelException` for a call after close — on
`ServerSocketChannel.accept`, `SocketChannel.read` **and** `Pipe.source().read`,
which is precisely the Windows arm W7-53 leaves open. `RChannelInterrupt` and
`RSocketChannelInterrupt` are green on this host today, so the CratonVM side of
those three rows already agrees for sockets; the pipe row is the one with no
CratonVM measurement, and it is now the only thing between the record and a
falsifiable fixture.

---

## 6. Which claimed fixes are actually in the tree

Every claim this lane depended on was re-checked against source or against the
dump, never against the record that made it.

| claim | verdict on this tree |
|---|---|
| `G4-1` §5.1 rows 9–13, 14–18 (`native-io` after-close family) | **PRESENT**, all ten, and correct except for the precedence order — see §3.1/§3.2 |
| `G4-1` §5.1: those bodies are "live in every mode" | **TRUE for the reads and writes** (`read0`/`readBytes`/`skip0`/`available0` are all `Bridge`, `owns_slot`) but **FALSE for `FileInputStream.close`**, which strict mode drops. §1.3 |
| `G4-1` §2: `io_streams.rs` is synthetic-only | **CONFIRMED independently by the dump** — zero `--jdk-only` rows for all three classes |
| `W7-70`: `native_fd_close0`'s close half still open | **WAS open, now FIXED** — §3.3 |
| `W7-70`: `native_fis_close`'s close still open | **WAS open, now FIXED** — §3.3 |
| `W7-70`: `native_fd_close0` is "shared with a socket dispatcher" | **WRONG** — `net.rs:4178` owns that slot. §1.1 |
| `W7-70` residual: write after `PrintStream.close()` is not refused | **STILL OPEN**, and now measured on both sides (§2.5). Outside this lane — N1 |
| `W7-57` rows 1–7 (`io_streams.rs` propagation) | **PRESENT**, all seven — and rows 1/2/3 were propagation-only, which §4.1 shows was half the contract |
| `W7-57` rows 24–29 (`native-io` propagation) | **PRESENT**, all six |
| `W7-57` row 12 (`native_input_stream_reader_close`) | **STALE**, function deleted — as `G4-1` §6 records |
| `W7-57` rows 25/26: `FilterOutputStream.close()`'s JDK body and winner | **WRONG on JDK 25** — the close wins and the flush is suppressed. §2.4 |
| `W7-64`: its FIXED rows | **UNVERIFIABLE from that record** — its own banner says nothing was built or run. §2.6 |
| `W7-53`: 7 open blocking-close sites | **NONE in this lane's two files.** §5 |

---

## 7. What this lane did NOT do

* **It did not run CratonVM with these edits.** No `cargo` command was run; the
  binary predates the source. Every "after" column is a reading of source.
* **It did not act on §2.4's suppression asymmetry.** `FilterOutputStream` and
  `BufferedOutputStream` report the **close** failure and suppress the flush;
  `BufferedWriter` reports the **flush** and suppresses the close. Both measured,
  neither derivable. CratonVM has no `addSuppressed` plumbing on these paths and
  already reports one of the two; changing which one, without the ability to
  attach the other, trades one partial answer for a different partial answer.
  Measured, corrected against W7-57's text, and left for whoever owns
  `addSuppressed` — still open from W7-57.
* **It did not invent a closed latch for `Object{Input,Output}Stream`.** §2.8
  measures HotSpot as having none: post-close writes succeed and reach the sink,
  post-close `readObject` succeeds, and `oos.close()` is not idempotent. The
  tree's stubs already behave that way. A latch would have been a fabrication
  that looked like a fix, and it is named here so the next sweep does not add one.
* **It did not touch `native_fos_flush`.** Measured correct, and now pinned.
* **It did not widen `native_fis_available`'s trailing `unwrap_or(0)`.** G4-1's
  reasoning stands unchanged: `FdTable::available` ends in `_ => Err(..)` for
  every entry kind that is not a file read or a child pipe, and `0` is a legal
  answer for `available()` in a way `-1` is not for `read()`.
* **It did not repair `native_raf_close`'s swallowed close** (`lib.rs`, the
  synthetic `RandomAccessFile` block). That whole block is behind
  `!real_raf_enabled()`, i.e. `CRATONVM_SYNTHETIC_RAF=1`, and the dump confirms
  `java/io/RandomAccessFile` is served entirely by
  `native-io/src/random_access_file.rs` in `--jdk-only`. The record's own note on
  that block calls the synthetic RAF path "broken". Repairing a close inside an
  opt-in-broken path is work with no reader; §2.2 measures the whole RAF
  after-close family for whoever revisits `random_access_file.rs`, which is not
  this lane's file.
* **It did not restore or repair the dead `p58_pushback_reader_*` bodies.**
  Named at the site instead — §4.3.
* **It did not add anything under `regression-suite/`.** The vector is in
  [§10](#10-the-regression-vector-source-only) as source only.
* **It ran no state-changing git command**, including `git stash`.

---

## 8. NOMINATIONS

Ordered by severity. Every one is outside this lane's two files.

**N1 — `native-builtins/src/logging_shims.rs`: a write after
`PrintStream.close()` is not refused, and the flag it should set is measured.**
W7-70's largest residual, now with a two-sided oracle (§2.5): a clean `close()`
leaves `checkError()` **false**; the first write after it makes it **true**,
throws nothing, and delivers **zero bytes** to the sink. W7-70 states the
obstacle honestly — the write path would have to consult the `closing` latch,
which is one `get_field_by_name` per `println` on the hottest path in the VM.
Two things are now available that were not: the target is falsifiable from Java
without any VM instrumentation, and the sink-side counters (`0` bytes after
close, `0` extra flushes from `checkError()` on a closed stream) give an
over-correction guard for each half. Also still open there: six unregistered
`PrintStream` methods (`print(char[])`, `println(char[])`, `write(byte[])`,
`writeBytes(byte[])`, `append(CharSequence,int,int)`, `append(char)`).

**N2 — `W7-57`'s rows 25/26 and its "Ordering was preserved" bullet are
factually wrong for `FilterOutputStream` on JDK 25, and `native-io`'s
`native_filter_out_close` / `native_dos_close` were written from them.**
Measured (§2.4): `FilterOutputStream.close()` over a sink whose `flush` and
`close` both throw reports the **close** exception with the flush attached as
suppressed, and the `finally` close runs either way (`flushes == 1`,
`closes == 1`). W7-57 says the flush wins. The bodies in this lane's file report
the flush and are therefore reporting the wrong one of two failures — but
changing that without `addSuppressed` would only move the loss, so it is
nominated together with N3 rather than half-done here. **The record itself should
be corrected regardless**: it is a citation people trust, and this claim is
falsified.

**N3 — `addSuppressed` has no plumbing anywhere in the native close paths.**
Every measured multi-failure close in §2.4 attaches the loser to the winner:
`bos.close()` → `suppressed=1`, `dos.close()` → `suppressed=2`, try-with-resources
→ `suppressed=1`, two resources → `suppressed=1`. CratonVM reports one and drops
the other silently. This is the single change that would make N2 answerable
rather than a coin flip, and it is a `types`/`vm` change, not a native one. Open
from W7-57; restated with a measured target shape.

**N4 — `native-builtins/src/phases_late.rs:7538`, `oos_write_bytes` returns
`()`.** Confirmed live by measurement from the other end: `new
ObjectOutputStream(<failing sink>)` **throws** on HotSpot (§2.8 — the 4-byte
`0xACED0005` header write), and CratonVM's `<init>` calls `oos_write_bytes` and
drops it, so constructing a serializer over a dead sink succeeds. Eleven
`writeInt`/`writeLong`/`writeUTF`/… bodies in this lane's `io_streams.rs` call
the same helper; once the signature carries a `Result`, the eleven call sites are
a mechanical follow-up **inside this lane's file**. `--synthetic-jdk` only.
Restated from `G4-1` N4 with the constructor row added.

**N5 — `native-builtins/src/phases_late/net_channels.rs` /
`native-io/src/datagram.rs`: `setSoTimeout`'s `.max(0)`, and the check order.**
`G4-1` N8 supplies the open-socket answer
(`IllegalArgumentException("timeout < 0")`); §2.7 adds the half that decides the
implementation: on a **closed** socket the answer is
`SocketException("Socket is closed")`, i.e. **the closed check runs first**.
Implementing the range check alone would produce the wrong exception for the
closed case, which is a fresh divergence introduced while fixing a real one — the
same shape as §3.1.

**N6 — `native-io/src/pipe.rs`: the Windows pipe sink write, W7-53's last
structural open row.** Not in this lane (sibling module). §2.7 now supplies the
target for the whole family measured end to end, including
`Pipe.source().read()` woken by `close()` → `AsynchronousCloseException` and
`ClosedChannelException` after close, which W7-53 had for sockets but not for
pipes. W7-53's own analysis of the fix (`CreateNamedPipe(FILE_FLAG_OVERLAPPED)` +
bounded `GetOverlappedResultEx`) is unchanged and still correct.

**N7 — `native-builtins/src/phases_late/net_channels.rs`:
`DatagramChannel.receive`, and `servlet.rs` / `t27_tls.rs`: the four TLS sites
and the `s2_blocking_accept` multi-acceptor race.** The remaining six of W7-53's
seven. Nothing new is added here beyond §2.7's oracle and the confirmation that
they are not reachable from this lane.

**N8 — `native-io/src/random_access_file.rs`: the `RandomAccessFile` after-close
family has never been swept.** The dump shows it is the `--jdk-only` winner for
every RAF triple (`open0`, `read0`, `readBytes0`, `write0`, `writeBytes0`,
`seek0`, `length0`, `setLength0`, `getFilePointer`). §2.2 measures the whole
surface: `read`, `write`, `length`, `seek`, `getFilePointer` and `setLength`
after close are all `IOException("Stream Closed")` — capital `C`, the
`FileInputStream` family's string — and a double close is a clean `void`. The
`is_empty_transfer` precedence order almost certainly applies to
`readBytes0`/`writeBytes0` as well and has NOT been measured on RAF; measure it
before porting the rule.

**N9 — `vm/src/vm/vm_init.rs` and `native-builtins/tests/essential_wiring_ratchet.rs`:
`java/io/FileDescriptor.close0()V` deserves a ratchet row.** It is the single
funnel through which every real-JDK `FileInputStream.close()` and
`FileOutputStream.close()` passes (measured: `invocations = 1` in two separate
`--jdk-only` vectors), it is registered twice in `native-io/src/lib.rs` under two
different class names, and one of those two registrations is silently
overwritten by `net.rs`. A body that important with an ownership question that
subtle belongs in the pinned set. Same species as `G4-1` N6.

---

## 9. Probe sources

Three probes, run with

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
"$JAVA_HOME/bin/java" <Probe>.java     # single-file source mode, no javac step
```

`CloseFamilyProbe.java` (§2.2–2.5, §2.7), `CloseOrderProbe.java` (§2.1, §2.3),
`SynthCloseProbe.java` (§2.8). All three share this reporter, which prints
`null` distinctly from the empty string, prints the suppressed chain, and keeps
every label ASCII:

```java
static void p(String label, Object v) {
    String s;
    if (v == null) s = "null";
    else if (v instanceof Throwable) {
        Throwable t = (Throwable) v;
        String m = t.getMessage();
        StringBuilder sb = new StringBuilder();
        sb.append("THROW ").append(t.getClass().getName())
          .append(" msg=").append(m == null ? "null" : "[" + m + "]");
        Throwable[] sup = t.getSuppressed();
        sb.append(" suppressed=").append(sup.length);
        if (sup.length > 0) {
            sb.append(" {");
            for (int i = 0; i < sup.length; i++) {
                if (i > 0) sb.append(",");
                sb.append(sup[i].getClass().getName()).append(":").append(sup[i].getMessage());
            }
            sb.append("}");
        }
        if (t.getCause() != null) sb.append(" cause=").append(t.getCause().getClass().getName());
        s = sb.toString();
    } else s = "RET [" + v + "]";
    System.out.println(label + " | " + s);
}
interface Act { Object run() throws Throwable; }
static void t(String label, Act a) {
    try { p(label, a.run()); } catch (Throwable e) { p(label, e); }
}
```

The counting sinks are what make §2.4, §2.5 and §2.8 assertions about *effects*
rather than about return values — a no-op `close()` returns perfectly cleanly,
which is the whole problem with it:

```java
static class TraceOut extends OutputStream {
    StringBuilder ops = new StringBuilder();
    int closes = 0, flushes = 0, bytes = 0;
    public void write(int b) { bytes++; }
    public void write(byte[] b, int o, int l) { bytes += l; }
    public void flush() { flushes++; ops.append(ops.length() > 0 ? ",flush" : "flush"); }
    public void close() { closes++; ops.append(ops.length() > 0 ? ",close" : "close"); }
}
static class CountIn extends InputStream {
    int closes = 0, reads = 0, avails = 0;
    byte[] data = "abcdef".getBytes(); int pos = 0;
    public int read() { reads++; return pos < data.length ? data[pos++] : -1; }
    public int available() { avails++; return data.length - pos; }
    public void close() { closes++; }
}
```

**One row is deliberately withheld.** `OPEN fis.skip(-1)` at position 0 fails
with an OS message rendered through the Windows console code page and came out
as mojibake. Its *type* is `java.io.IOException`; its message is a
locale-dependent host string and is **not** transcribed, because transcribing a
mojibake string is how a differential fails with every assertion passing
(`HANDOFF-20260814.md` §7). `OPEN fis.skip(-1)` at position 1 returns `-1`, which
is transcribed, because it is a number.

---

## 10. The regression vector, source only

Not created under `regression-suite/` — this lane may not add files there. The
class name and the `JDKONLY_CLASSES` entry are left to whoever lands it.

Every assertion is a MEASURED oracle row from §2, and every refusal is **paired**
with the call that must still succeed, so no check can pass vacuously and no
check can pass because a body started throwing for everything.

```java
import java.io.*;
import java.nio.file.*;

/**
 * Pins the close/flush family measured in
 * docs/known-issues/jdk-only/G39-1-the-close-family-closed-20260817.md
 *
 * Every expected value here was MEASURED on HotSpot 25.0.3+9-LTS, not derived.
 * Labels are ASCII only (HANDOFF-20260814 section 7).
 */
public class RJdkCloseFamily {
    static int checks = 0, failures = 0;

    static void check(boolean ok, String what) {
        checks++;
        if (!ok) { failures++; System.out.println("FAIL: " + what); }
    }

    static void throwsAs(Class<? extends Throwable> expected, String what, Runnable body) {
        checks++;
        try {
            body.run();
            failures++;
            System.out.println("FAIL: " + what + " -> returned normally, expected "
                               + expected.getName());
        } catch (Throwable e) {
            Throwable t = (e instanceof RuntimeException && e.getCause() != null
                           && !expected.isInstance(e)) ? e.getCause() : e;
            if (!expected.isInstance(t)) {
                failures++;
                System.out.println("FAIL: " + what + " -> " + t.getClass().getName()
                                   + ", expected " + expected.getName());
            }
        }
    }

    /** Run `body`, failing if it throws at all. The over-correction guard. */
    static void quiet(String what, Runnable body) {
        checks++;
        try { body.run(); }
        catch (Throwable e) {
            failures++;
            System.out.println("FAIL: " + what + " -> threw " + e.getClass().getName()
                               + ": " + e.getMessage() + " (expected a clean return)");
        }
    }

    public static void main(String[] args) throws Exception {
        Path base = Files.createTempDirectory("rclose");
        Path rf   = Files.write(base.resolve("r.txt"), "abcdef".getBytes());
        Path wf   = base.resolve("w.txt");

        // ---- FileOutputStream: the closed refusal, and its zero-length hole --
        FileOutputStream fos = new FileOutputStream(wf.toFile());
        fos.write("hi".getBytes());
        fos.close();
        fos.close();                                        // idempotent
        quiet("fos.flush() after close",    () -> { try { fos.flush(); }
                 catch (IOException e) { throw new RuntimeException(e); } });
        quiet("fos.write(b,0,0) after close", () -> { try { fos.write(new byte[4], 0, 0); }
                 catch (IOException e) { throw new RuntimeException(e); } });
        quiet("fos.write(new byte[0]) after close", () -> { try { fos.write(new byte[0]); }
                 catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "fos.write(int) after close",
                 () -> { try { fos.write(65); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "fos.write(b,0,4) after close",
                 () -> { try { fos.write(new byte[4], 0, 4); } catch (IOException e) { throw new RuntimeException(e); } });
        // bounds beat closed, on a CLOSED stream
        throwsAs(IndexOutOfBoundsException.class, "fos.write(b,-1,2) after close",
                 () -> { try { fos.write(new byte[4], -1, 2); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(NullPointerException.class, "fos.write(null,0,1) after close",
                 () -> { try { fos.write(null, 0, 1); } catch (IOException e) { throw new RuntimeException(e); } });
        // the bytes written BEFORE the close must still be there
        check(Files.size(wf) == 2, "bytes written before close must survive; size=" + Files.size(wf));

        // ---- FileInputStream: EOF is not the answer, but 0 sometimes is ------
        FileInputStream fis = new FileInputStream(rf.toFile());
        check(fis.read() == 'a', "an OPEN FileInputStream must still read");
        check(fis.read(new byte[0]) == 0, "read(new byte[0]) on an OPEN stream is 0, not -1");
        fis.close();
        fis.close();                                        // idempotent
        check(fis.read(new byte[4], 0, 0) == 0, "read(b,0,0) after close is 0");
        check(fis.read(new byte[0]) == 0,       "read(new byte[0]) after close is 0");
        throwsAs(IOException.class, "fis.read() after close",
                 () -> { try { fis.read(); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "fis.read(b) after close",
                 () -> { try { fis.read(new byte[4]); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "fis.available() after close",
                 () -> { try { fis.available(); } catch (IOException e) { throw new RuntimeException(e); } });
        // skip has NO zero-length escape hatch: all three spellings refuse
        throwsAs(IOException.class, "fis.skip(0) after close",
                 () -> { try { fis.skip(0); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "fis.skip(-5) after close",
                 () -> { try { fis.skip(-5); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "fis.skip(1) after close",
                 () -> { try { fis.skip(1); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IndexOutOfBoundsException.class, "fis.read(b,0,99) after close",
                 () -> { try { fis.read(new byte[4], 0, 99); } catch (IOException e) { throw new RuntimeException(e); } });

        // ---- the descriptor, and a close over an already-dead handle --------
        check(!fis.getFD().valid(), "getFD().valid() is false after close");
        FileOutputStream w4 = new FileOutputStream(base.resolve("w4.txt").toFile());
        w4.write("data".getBytes());
        w4.getChannel().close();                            // kill the fd underneath
        quiet("close over an already-released handle", () -> { try { w4.close(); }
                 catch (IOException e) { throw new RuntimeException(e); } });
        check(Files.size(base.resolve("w4.txt")) == 4, "bytes must reach disk anyway");

        // ---- the two capitalisations, which must not be unified -------------
        String fisMsg = "";
        try { fis.read(); } catch (IOException e) { fisMsg = String.valueOf(e.getMessage()); }
        BufferedWriter bw = new BufferedWriter(new OutputStreamWriter(new ByteArrayOutputStream()));
        bw.close();
        String bwMsg = "";
        try { bw.write("y"); } catch (IOException e) { bwMsg = String.valueOf(e.getMessage()); }
        check(fisMsg.equals("Stream Closed"), "FileInputStream says 'Stream Closed'; saw " + fisMsg);
        check(bwMsg.equals("Stream closed"),  "BufferedWriter says 'Stream closed'; saw " + bwMsg);

        // ---- Closeable idempotence is per-class, not a language guarantee ----
        final int[] n = new int[1];
        Closeable c = () -> n[0]++;
        c.close(); c.close(); c.close();
        check(n[0] == 3, "Closeable.close() is NOT auto-idempotent; saw " + n[0]);

        System.out.println("RJdkCloseFamily: " + checks + " checks, " + failures + " failures");
        if (failures != 0) throw new AssertionError(failures + " failures");
    }
}
```

Note for whoever lands it: the `PrintStream` block is deliberately **absent**.
Its contract is measured in §2.5 and it is [N1](#8-nominations), not fixed here;
including it would make the vector red for something this lane did not claim.

---

## 11. Unit tests added

All in the existing `#[cfg(test)]` modules, none weakened, none removed.

`native-io/src/lib.rs`, `mod io_tests` — 8 tests. Each refusal is paired with the
call on the same body that must still succeed:

| test | pins |
|---|---|
| `is_empty_transfer_is_zero_only` | the rule itself |
| `fis_read_bytes_zero_length_answers_zero_on_a_closed_stream` | `0` for `len == 0`, **and** the refusal for `len == 4` |
| `fis_read_empty_array_answers_zero_not_eof` | `0`, never `-1` |
| `fos_write_bytes_zero_length_is_void_on_a_closed_stream` | `void` on **both** descriptors, **and** the refusal for a real write on both |
| `fos_write_empty_array_is_void_on_a_closed_stream` | `void`, **and** the refusal for a 1-byte array |
| `fis_skip_consults_the_descriptor_before_the_count` | the refusal for `skip(0)`/`skip(-5)`/`skip(1)`, **and** the quiet `0` for an UNKNOWN receiver |
| `fd_close0_marks_closed_and_a_second_close_is_quiet` | the marker, the clean double close, and that the marker is the one `io_stream_is_closed` reads |
| `fis_close_marks_closed_and_is_idempotent` | the marker, the clean double close, and the read side refusing after it |
| `fos_flush_after_close_stays_quiet` | the control: this body must NOT grow a refusal |

`native-builtins/src/phases_late/io_streams.rs`, new `mod tests` — 5 tests, with
the module doc stating up front that they are Compatible/synthetic only:

| test | pins |
|---|---|
| `pushback_input_stream_close_is_idempotent_and_latches` | inner close count `1` after two closes; both slots nulled |
| `pushback_input_stream_refuses_its_whole_surface_after_close` | all three methods succeed OPEN, all three refuse CLOSED, and the refusals never reach the wrapped stream |
| `pushback_input_stream_closed_beats_overflow` | overflow while open, `Stream closed` once closed — the ordering |
| `pushback_reader_close_reaches_the_inner_reader_twice` | inner close count `2`; slot 0 NOT nulled, slot 1 nulled |
| `pushback_messages_differ_where_measured_and_agree_where_measured` | the closed strings coincide, the overflow strings do not, and neither is `"Stream Closed"` |

---

## 12. What the orchestrator must check at build time

Ordered by how much a wrong answer costs.

1. **`native-io` must compile, or nothing downstream is checked at all.**
   `native-builtins` depends on it. `rustfmt --edition 2021 --check` parses both
   files clean and reports no diff inside any edited region (`io_streams.rs` is
   fully clean, exit 0; `native-io/src/lib.rs` has 35 pre-existing diffs, the same
   35 as before these edits, none of them in an edited range). That is a parse
   gate, not a type gate.
2. **Highest behavioural risk: `native_fd_close0` now propagates its close.**
   Measured live in `--jdk-only` (`invocations = 1` in both `RFileTimes` and
   `RJdkNio`). If a `--jdk-only` vector starts failing with an `IOException` out
   of a `FileOutputStream.close()`, this is it, and the failure is a real
   disk/flush error that was previously invisible — not a regression. The
   argument that it cannot fire on non-writer entries is
   [§3.3](#33-both-close-halves-now-propagate--w7-70s-residual-closed); if it
   fires anyway, `FdTable::close`'s match has grown an arm since.
3. **`FileOutputStream.write([BII)V` runs 24 times in `RFileTimes` alone.** The
   `is_empty_transfer` short-circuit sits directly on that hot path. It is three
   lines and a comparison against a value already in a register, but it is on the
   path, and a `len == 0` write now returns *earlier* than before rather than
   later.
4. **`PushbackInputStream` becomes refusing.** Anything in the synthetic corpus
   that read from a `PushbackInputStream` after closing it was working by
   accident and now gets `IOException: Stream closed`. Expect *new* failures that
   name a real defect in the caller, not broken working code. `--synthetic-jdk`
   only.
5. **`PushbackReader.close()` now reaches the wrapped `Reader` twice on a double
   close.** If any synthetic-corpus sink counts its closes, this is where the
   count changes. HotSpot behaves identically.
6. **`vm/src/vm/tests.rs`'s `pushback_input_stream_p58` and
   `pushback_reader_basics_p66` were checked against the new closed-state
   marker and are unaffected**: the first sets slot 1 to a real buffer and never
   closes; the second asserts slot 0 is null *before* the close and is not
   sensitive to whether the close nulls it. Neither was edited.
7. **Run `RJdkCloseFamily` (§10) in BOTH modes** once it lands. Its
   `PushbackInputStream` half is meaningful only under `--synthetic-jdk`; its
   `FileInputStream`/`FileOutputStream` half is meaningful in both, and the
   `skip(0)` and `write(b,0,0)` rows are the two that this record adds.

---

## 13. The nine named vectors, re-run

All nine were re-run under `--jdk-only` on
`C:/craton/target-rel2/release/cratonvm.exe`. **This binary does not carry these
edits** — the lane may not build — so the run is a "did the tree move under me"
control, not evidence about the source above. Stated plainly because a green
table here is exactly the kind of thing that gets read as verification.

| vector | exit | result |
|---|---|---|
| `RJdkNio` | 0 | `PASS RJdkNio (101 checks)` |
| `RFileTimes` | 0 | `PASS RFileTimes (68 checks)` |
| `RNioNoFollow` | 0 | `PASS RNioNoFollow (22 checks)` |
| `RFsSingleton` | 0 | `PASS RFsSingleton (12 checks)` |
| `RJdkAsyncChannel` | 0 | `PASS RJdkAsyncChannel (141 checks)` |
| `RChannelInterrupt` | 0 | `PASS RChannelInterrupt` |
| `RSocketChannelInterrupt` | 0 | `PASS RSocketChannelInterrupt` |
| `RDataInputFastPull` | 0 | `PASS RDataInputFastPull (22 checks)` |
| `RCrypto` | 0 | `PASS RCrypto (57 checks)` |

`RJdkNio` reports `asyncClose=AsynchronousCloseException`, which is §2.7's
oracle row for a blocking `accept()` woken by another thread's `close()` — so
that half of W7-53's family is already agreeing on this host, on sockets.

## 14. `git status --short`

```
 M native-builtins/src/phases_late/io_streams.rs
 M native-io/src/lib.rs
```

Two files, both in this lane. LF only (zero CR bytes in either). No
`regression-suite/` file, no `INDEX.md`, no `README.md`, no registration added,
moved or removed, and no state-changing git command was run.
