# G4-1 — the `java.io` / `java.nio.file` fabricated-success sweep, with the oracle arm MEASURED

> **RECONCILED 2026-08-17 (lane G40) — the provenance premise "no JDK source was
> read" was avoidable.** `C:\craton\jdk25src` is indeed absent, and this record
> is right about that. But the JDK's sources ship with the oracle itself, at
> `$JAVA_HOME/lib/src.zip` (52,462,198 bytes) — the sources of the exact
> HotSpot 25.0.3+9-LTS build used here. Nothing measured in this record is
> invalidated. Related: the `Files.newDirectoryStream` filter defect this sweep's
> family produced was fixed in `d378eee51`, which turned `RCrypto` red for a
> correct reason — it had only ever been green because that filter was never
> called. `RCrypto` is green again at `783685c34`. See `INDEX.md` §B.3.

Status: **oracle arm MEASURED on HotSpot 25.0.3+9-LTS. CratonVM arm entirely
PREDICTED.** No `cargo` command of any kind was run for this record, no binary
carries these edits, and no CratonVM output appears anywhere below. Every
"CratonVM" column in this document is a claim about *source I read*, not about
behaviour I observed. The oracle columns are transcripts.

That split is stated first because it is the directory's biggest recurring sin
and `HANDOFF-20260814.md` §2 says so outright: *"a prediction is not a result."*

Lane scope — three files, and nothing else may be edited:

* `native-builtins/src/phases_late/nio_file.rs`
* `native-builtins/src/phases_late/io_streams.rs`
* `native-io/src/lib.rs`

Everything found outside them is in [§8 NOMINATIONS](#8-nominations).

Parent record: `W7-8-fabricated-success-io-sweep.md`, whose §9 the INDEX still
marks OPEN. §9's two largest rows turned out to be **already fixed in this
tree** — see [§6](#6-which-claimed-fixes-are-actually-in-the-tree). The rows
this lane found are new.

---

## 1. The instrument, and the one question

Same instrument as W7-8 §1B: find every `Err(_) =>`, `unwrap_or`, `let _ =`,
and every `match` arm whose failure branch returns a plausible value, then ask
one question per hit — **can the caller tell this from a real success?**

What made this pass different is that the *answer sheet* was measured rather
than read. Four probes were run against
`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot` in single-file source
mode; their sources and full transcripts are in [§9](#9-probe-sources-and-raw-transcripts).
226 cells. Labels are ASCII throughout, for the reason `HANDOFF-20260814.md` §7
records: a single em-dash once failed a differential with every assertion
passing.

Three findings could not have come from reading:

* `Files.newInputStream(p, WRITE)` says `'WRITE' not allowed` **with the
  quotes**, and its mirror `Files.newOutputStream(p, READ)` says
  `READ not allowed` **without them**, and the two are different exception
  *types*.
* `FileInputStream` after close says `Stream Closed`; `BufferedWriter` after
  close says `Stream closed`. One character, two families.
* `PushbackReader` overflow says `Pushback buffer overflow`;
  `PushbackInputStream` overflow says `Push back buffer is full`. Sibling
  classes, unrelated strings.

None of the three is derivable. All three are now transcribed at the site.

---

## 2. Reachability, established before anything was adjudicated

A verdict is worthless without knowing whether the code runs, and this lane
spent its first hour on that rather than on greps. Source of truth:
`native-builtins/tests/registrar_reachability.rs`, whose `SYNTHETIC_ONLY_CLOSURE`
is a maintained enumeration of every registrar reachable **only** through
`register_synthetic_overrides` — i.e. absent from the shipping `cratonvm-cli`
binary entirely.

| registrar | file | reach |
|---|---|---|
| `register_phase57_nio_file` | `nio_file.rs` | **SHIPPING** — called directly by `vm_init.rs:2354` and `:2919` |
| `register_phase57_file` | `nio_file.rs` | **SHIPPING** — `vm_init.rs:2366`, `:2929` |
| `register_nio_channel_extras` | `native-io/src/lib.rs` | **SHIPPING** — via `register_io_natives` |
| `register_phase57_file_channel` | `nio_file.rs` | synthetic-only |
| `register_p71_files_bridge`, `register_p61_files_path`, `register_p66_watch_service` | `nio_file.rs` | synthetic-only |
| `register_p58_pushback`, `register_p66_pushback_reader`, `register_p70_object_streams` | `io_streams.rs` | **synthetic-only — the whole file** |

**`io_streams.rs` has no reach in `--jdk-only` at all.** Its three registrars
are all in `SYNTHETIC_ONLY_CLOSURE`. Its rows are still fixed (they are cheap,
in-file and mechanical, and W7-8 §2 sets that precedent), but nothing in
[§5](#5-what-changed-in-io_streamsrs) should be read as predicting a change in a
`--jdk-only` measurement.

### 2.1 A correction to W7-8 §9.1: `register_io_natives` is NOT last

W7-8 §9.1 states that `register_io_natives` is the final registrar in all three
arms, and the whole §9 analysis of who-wins-what rests on it. On this tree that
is true of the synthetic arm and **false of both real-JDK arms**:

| arm | order in `vm/src/vm/vm_init.rs` |
|---|---|
| `--synthetic-jdk` | `register_builtins` `:1934` → `register_io_natives` `:1935` |
| feature build, real-JDK | `register_io_natives` **`:2267`** → … → `register_phase57_nio_file` **`:2354`** → `register_phase57_file` `:2366` |
| shipping `cratonvm-cli` | `register_io_natives` **`:2849`** → … → `register_phase57_nio_file` **`:2919`** → `register_phase57_file` `:2929` |

This is not a footnote. `java/nio/file/Files.list(Path)Stream` and
`Files.walk(Path,[FileVisitOption)Stream` are registered **twice** — in
`nio_file.rs:4757/4790` and in `native-io/src/lib.rs:19771/19777` — with
different bodies, so **the winner differs by mode**: `native-io`'s in
`--synthetic-jdk`, `nio_file.rs`'s in `--jdk-only` and `--real-jdk`.

Had I trusted §9.1, both `Files.list` repairs would have landed in a body with
`invocations = 0` in the mode this project is named after. That is precisely
`HANDOFF-20260814.md` §5's first trap, and W7-8's own §7.1 lesson recurring a
third time. **Both halves are repaired here**, and the ordering table above is
now a doc comment on `files_listing_missing_refusal` in `native-io/src/lib.rs`
so the next reader does not have to rediscover it.

*Caveat, stated as a caveat:* the ordering above is read from `vm_init.rs`
source. It has not been confirmed with `--dump-native-registry`, which is the
only instrument that settles it. **This is the first thing the orchestrator
should check** — see [§10](#10-what-the-orchestrator-must-check-at-build-time).

---

## 3. The measured oracle tables

All rows below are **MEASURED** on `openjdk 25.0.3 2026-04-21 LTS`
(`Temurin-25.0.3+9-LTS`), Windows 11, NTFS. `msg=null` means
`getMessage()` returned null; an empty message would print as `msg=[]`.

### 3.1 Directory listing — the family this lane is built on

| call | oracle result |
|---|---|
| `Files.list(<missing>)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` — **at construction** |
| `Files.list(<regular file>)` | `THROW java.nio.file.NotDirectoryException msg=[<path>]` |
| `Files.list(<dir>).count()` | `RET [2]` |
| `Files.walk(<missing>)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` — **at construction** |
| `Files.walk(<missing>).count()` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `Files.walk(<regular file>).count()` | `RET [1]` — a file is a legal start element |
| `Files.walk(<dir>).count()` | `RET [3]` |
| `Files.walk(<dir>, 0).count()` | `RET [1]` |
| `Files.walk(<dir>, -1)` | `THROW java.lang.IllegalArgumentException msg=['maxDepth' is negative]` |
| `Files.find(<missing>, 1, (p,a)->true).count()` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `Files.find(<dir>, -1, …)` | `THROW java.lang.IllegalArgumentException msg=['maxDepth' is negative]` |
| `provider.newDirectoryStream(<missing>, all)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `provider.newDirectoryStream(<regular file>, all)` | `THROW java.nio.file.NotDirectoryException msg=[<path>]` |
| `Files.newDirectoryStream(<dir>, "*.txt")` | `RET [[a.txt]]` — one of two entries |
| `Files.newDirectoryStream(<dir>, q -> false)` | `RET [[]]` |
| `Files.newDirectoryStream(<dir>, q -> { throw new IOException("filter boom"); })` | `THROW java.nio.file.DirectoryIteratorException msg=[java.io.IOException: filter boom] cause=java.io.IOException` — **from the iterator, not the open** |
| `ds.iterator()` twice | `THROW java.lang.IllegalStateException msg=[Iterator already obtained]` |
| `ds.close(); ds.iterator()` | `THROW java.lang.IllegalStateException msg=[Directory stream is closed]` |
| `ds.close(); ds.close()` | `RET [void]` |
| `it.hasNext()` after `ds.close()` | `RET [false]` |
| `it.next()` after `ds.close()` | `THROW java.util.NoSuchElementException msg=null` |
| `Files.newDirectoryStream(<dir>).spliterator()` class | `java.util.Spliterators$IteratorSpliterator` |
| `Files.walkFileTree(<missing>, v)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `Files.walkFileTree(<regular file>, v)` | `RET [<path>]` |

### 3.2 `java.io` streams after `close()`

| call | oracle result |
|---|---|
| `fos.close()` then `fos.close()` | `RET [void]`, `RET [void]` |
| `fos.flush()` after close | **`RET [void]`** — no throw |
| `fos.write(65)` after close | `THROW java.io.IOException msg=[Stream Closed]` |
| `fos.getFD().valid()` after close | `RET [false]` |
| `fis.close()` twice | `RET [void]`, `RET [void]` |
| `fis.read()` after close | `THROW java.io.IOException msg=[Stream Closed]` |
| `fis.read(byte[])` after close | `THROW java.io.IOException msg=[Stream Closed]` |
| `fis.available()` after close | `THROW java.io.IOException msg=[Stream Closed]` |
| `fis.skip(1)` after close | `THROW java.io.IOException msg=[Stream Closed]` |
| `bw.write("y")` after close | `THROW java.io.IOException msg=[Stream closed]` |
| `bw.newLine()` after close | `THROW java.io.IOException msg=[Stream closed]` |
| `bw.flush()` after close | `THROW java.io.IOException msg=[Stream closed]` |
| `osw.write("z")` after close | `THROW java.io.IOException msg=[Stream closed]` |
| `osw.flush()` after close | `THROW java.io.IOException msg=[Stream closed]` |
| `osw.close()` twice | `RET [void]`, `RET [void]`, and the file on disk is 5 bytes |

Note the split: **`FileOutputStream.flush()` after close does NOT throw**, while
`BufferedWriter.flush()` and `OutputStreamWriter.flush()` do. CratonVM's
`native_fos_flush` already returns `Ok(None)` in that state and is therefore
**correct as it stands** — it was deliberately not touched. That row is the
control that keeps this from being a mass edit.

### 3.3 Close / flush failure propagation and suppression

Measured with an `OutputStream` whose `write`, `flush` and `close` all throw.

| call | oracle result |
|---|---|
| `bos.write(65)` (buffered) | `RET [void]` |
| `bos.flush()` | `THROW java.io.IOException msg=[write boom] suppressed=0` |
| `bos.close()` | `THROW java.io.IOException msg=[close boom] suppressed=1 {java.io.IOException:write boom}` |
| `bos.close()` again | `RET [void]` |
| underlying `close()` calls / `flush()` calls | `1` / `0` |
| `bw.flush()` (failing `Writer`) | `THROW java.io.IOException msg=[w write boom] suppressed=0` |
| `bw.close()` | `THROW java.io.IOException msg=[w write boom] suppressed=1 {java.io.IOException:w close boom}` |
| `bw.close()` again | `RET [void]` |
| `try (r) { throw ISE }`, close throws | `THROW java.lang.IllegalStateException msg=[body boom] suppressed=1 {java.io.IOException:close boom}` |
| `try (r) { ok }`, close throws | `THROW java.io.IOException msg=[close boom] suppressed=0` |

The two `close()` rows disagree about which half wins: `BufferedOutputStream`
reports the **close** error and suppresses the flush error;
`BufferedWriter` reports the **flush** error and suppresses the close error.
That is measured, is the opposite of what one would derive from
`try (out) { flush(); }` in both cases, and is the kind of thing this lane
transcribes rather than unifies. It is **not** acted on here — see
[§7](#7-what-this-lane-did-not-do).

### 3.4 `PrintStream` — the error flag, and what survives `close()`

| call | oracle result |
|---|---|
| `ps.checkError()` on a stream over a failing sink, before any write | `RET [true]` — `checkError()` flushes first, and the flush fails |
| `ps.println("q")` (sink throws) | `RET [void (no throw)]` |
| `ps.checkError()` after | `RET [true]`, and `true` again — sticky |
| `ps.flush()`, `ps.close()` (sink throws) | `RET [void (no throw)]` both |
| `ps.checkError()` after close | `RET [true]` — **the flag survives `close()`** |
| `ps.println("r")` after close | `RET [void (no throw)]` |
| `ps2` over a working sink: `checkError()` initial / after a good write / after a clean `close()` | `false` / `false` / **`false`** |
| `ps2.println("after")` **after** the clean close | `RET [void]` |
| `ps2.checkError()` after that | **`RET [true]`** |
| `ps.print((String) null)` / `print((Object) null)` / `append(null)` on an OPEN stream | `RET [void]` (they print `"null"`) |
| `ps.print((char[]) null)` on an OPEN stream | `THROW java.lang.NullPointerException msg=[Cannot read the array length because "cbuf" is null]` |
| `ps.write((byte[]) null, 0, 0)` on an OPEN stream | `THROW java.lang.NullPointerException msg=[Cannot read the array length because "b" is null]` |
| `pw.println` over a failing `Writer`, then `checkError()` | `RET [void (no throw)]`, then `true` |
| `System.out.getClass().getName()` | `java.io.PrintStream` |

The assertable contract, in one line: **a clean `close()` leaves `checkError()`
false, and the first write *after* that close turns it true, without throwing.**
That is a two-sided witness a fixture can pin, and W7-70's residual "write after
close is not refused" is exactly it. No `PrintStream` body is in this lane's
three files (`native_printstream_close` is `logging_shims.rs`), so this is
[nomination N7](#8-nominations) with the measurement attached.

### 3.5 Channels — `FileChannel`, `AsynchronousFileChannel`, async close

| call | oracle result |
|---|---|
| `FileChannel.open(...)` class | `sun.nio.ch.FileChannelImpl` |
| `fc.close()` twice | `RET [void]`, `RET [void]` |
| `read` / `read(buf,pos)` / `write` / `write(buf,pos)` / `size` / `position` / `position(0)` / `truncate` / `force` / `transferTo` / `lock` / `map` after close | all `THROW java.nio.channels.ClosedChannelException msg=null` |
| `c2.position(-1)` | `THROW java.lang.IllegalArgumentException msg=null` |
| `c2.read(buf, -1)` / `c2.write(buf, -1)` | `THROW java.lang.IllegalArgumentException msg=[Negative position]` |
| `c2.truncate(-1)` | `THROW java.lang.IllegalArgumentException msg=[Negative size]` |
| `transferTo(-1,1,c)` / `transferTo(0,-1,c)` / `transferFrom(c,-1,1)` / `transferFrom(c,0,-1)` | `THROW java.lang.IllegalArgumentException msg=null` |
| `c2.read((ByteBuffer) null)` | `THROW java.lang.NullPointerException msg=[Cannot invoke "java.nio.ByteBuffer.isReadOnly()" because "dst" is null]` |
| `c2.write((ByteBuffer) null)` | `THROW java.lang.NullPointerException msg=[Cannot invoke "java.nio.ByteBuffer.position()" because "src" is null]` |
| `c2.truncate(1000)` on an 11-byte file | `RET [11]` — never grows |
| `c2.lock()` twice | `java.nio.channels.OverlappingFileLockException` |
| read-only channel: `write` / `truncate` / `lock` / `transferFrom` | `THROW java.nio.channels.NonWritableChannelException msg=null` |
| read-only channel: `force(true)` | **`RET [void]`** |
| `AsynchronousFileChannel.open(...)` class | `sun.nio.ch.WindowsAsynchronousFileChannelImpl` |
| `afc.force(true)` / `force(false)` open | `RET [void]` |
| `afc.lock()` future class | `sun.nio.ch.PendingFuture` |
| `afc.read(buf,-1)` / `write(buf,-1)` | `THROW java.lang.IllegalArgumentException msg=[Negative position]` |
| `afc.truncate(-1)` | `THROW java.lang.IllegalArgumentException msg=[Negative size]` |
| `afc.force(true)` **closed** | `THROW java.nio.channels.ClosedChannelException msg=null` |
| `afc.size()` / `afc.truncate(1)` / `afc.tryLock()` **closed** | `THROW java.nio.channels.ClosedChannelException msg=null` |
| `afc.lock()` **closed** | **`RET [sun.nio.ch.CompletedFuture@…]`** — no throw; the failure arrives at `get()` |
| `afc.read(buf,0)` **closed** | **`RET [sun.nio.ch.CompletedFuture@…]`** — same shape |
| AFC opened READ-only: `force(true)` / `force(false)` | `RET [void]` |
| AFC opened READ-only: `write` / `truncate` / `lock` | `THROW java.nio.channels.NonWritableChannelException msg=null` |
| blocking `accept()` interrupted by another thread's `close()` | `THROW java.nio.channels.AsynchronousCloseException msg=null` |
| blocking `accept()` interrupted by `Thread.interrupt()` | `THROW java.nio.channels.ClosedByInterruptException msg=null`, and `isOpen()` is then `false` |
| `ssc.accept()` before `bind` | `THROW java.nio.channels.NotYetBoundException msg=null` |
| `ssc.bind(...)` twice | `THROW java.nio.channels.AlreadyBoundException msg=null` |
| `ssc.accept()` / `getLocalAddress()` after close | `THROW java.nio.channels.ClosedChannelException msg=null` |
| `ssc.socket().setSoTimeout(-1)` | `THROW java.lang.IllegalArgumentException msg=[timeout < 0]` |
| `dc.socket().setSoTimeout(-1)` | `THROW java.lang.IllegalArgumentException msg=[timeout < 0]` |
| `dc.socket().setSoTimeout(10)` after close | `THROW java.net.SocketException msg=[Socket is closed]` |

Two of these settle open questions in other records:

* **W7-8 §7.3 / §3.6** asked whether `setSoTimeout`'s `.max(0)` had a
  specification. It does, on both the `DatagramSocket` and the
  `ServerSocket` adaptor: `IllegalArgumentException("timeout < 0")`.
* **W7-8 §9.2 point 3** worried that a `force` implemented by re-opening the
  path `.write(true)` would fail on a READ-only channel. The oracle confirms
  the exposure — HotSpot's `force` on a read-only channel returns cleanly —
  and the tree's current `afc_sync_at` already handles it with an explicit
  `if !handle.writable { return Ok(()) }`. That is a fix that is present, live,
  and agrees with a measurement nobody had taken.

### 3.6 `Files.*` refusals and boolean contracts (the control set)

| call | oracle result |
|---|---|
| `Files.deleteIfExists(<missing>)` | `RET [false]` |
| `Files.deleteIfExists(<non-empty dir>)` | `THROW java.nio.file.DirectoryNotEmptyException msg=[<path>]` |
| `Files.delete(<missing>)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `Files.createDirectory(<existing dir>)` | `THROW java.nio.file.FileAlreadyExistsException msg=[<path>]` |
| `Files.createDirectories(<existing dir>)` | `RET [<path>]` |
| `Files.createDirectories(<existing regular file>)` | `THROW java.nio.file.FileAlreadyExistsException msg=[<path>]` |
| `Files.createDirectories(<file>/child)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `Files.size(<dir>)` | `RET [0]` on NTFS |
| `Files.readAllBytes(<dir>)` / `readString(<dir>)` / `lines(<dir>)` / `write(<dir>,…)` | `THROW java.nio.file.AccessDeniedException msg=[<path>]` |
| `Files.newInputStream(p, WRITE)` | `THROW java.lang.UnsupportedOperationException msg=['WRITE' not allowed]` |
| `Files.newInputStream(p, APPEND)` | `THROW java.lang.UnsupportedOperationException msg=['APPEND' not allowed]` |
| `Files.newInputStream(p, CREATE)` / `(p, TRUNCATE_EXISTING)` / `(p, READ)` / `(p, NOFOLLOW_LINKS)` | all **accepted** |
| `Files.newOutputStream(p, READ)` | `THROW java.lang.IllegalArgumentException msg=[READ not allowed]` |
| `Files.newByteChannel(p, READ, APPEND)` | `THROW java.lang.IllegalArgumentException msg=[READ + APPEND not allowed]` |
| `Files.copy(p, dst, ATOMIC_MOVE)` | `THROW java.lang.UnsupportedOperationException msg=[Unsupported copy option: ATOMIC_MOVE]` |
| `Files.copy(in, dst, COPY_ATTRIBUTES)` | `THROW java.lang.UnsupportedOperationException msg=[COPY_ATTRIBUTES not supported]` |
| `Files.copy(in, dst, NOFOLLOW_LINKS)` | `THROW java.lang.UnsupportedOperationException msg=[NOFOLLOW_LINKS not supported]` |
| `Files.move(p, dst, COPY_ATTRIBUTES)` | `THROW java.lang.UnsupportedOperationException msg=[Unsupported option: COPY_ATTRIBUTES]` |
| `Files.setAttribute(<missing>, "lastModifiedTime", …)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `Files.setAttribute(p, "bogus:x", v)` | `THROW java.lang.UnsupportedOperationException msg=[View 'bogus' not available]` |
| `Files.setAttribute(p, "size", 5L)` | `THROW java.lang.IllegalArgumentException msg=['basic:size' not recognized]` |
| `Files.getAttribute(p, "bogus")` | `THROW java.lang.IllegalArgumentException msg=['bogus' not recognized]` |
| `Files.get/setPosixFilePermissions(p)` on Windows | `THROW java.lang.UnsupportedOperationException msg=null` |
| `Files.createLink(new, <missing>)` | `THROW java.nio.file.NoSuchFileException msg=[<link> -> <target>]` — **both paths** |
| `Files.isSameFile(<missing>, <missing>)` | `RET [true]` — equal paths short-circuit before any stat |
| `Files.isSameFile(<file>, <missing>)` | `THROW java.nio.file.NoSuchFileException msg=[<missing>]` |
| `FileSystems.getDefault().close()` | `THROW java.lang.UnsupportedOperationException msg=null`, and `isOpen()` stays `true` |
| `FileSystems.getDefault()` identity / `.provider()` identity | `RET [true]` / `RET [true]` |
| `provider.checkAccess(<missing>)` / `provider.isHidden(<missing>)` / `provider.getFileStore(<missing>)` | `THROW java.nio.file.NoSuchFileException msg=[<path>]` |
| `File.delete/mkdir/mkdirs/renameTo/setReadOnly/setLastModified` failing | `RET [false]` — the boolean contract, **correct as CratonVM has it** |
| `File.setLastModified(p, -1)` | `THROW java.lang.IllegalArgumentException msg=[Negative time]` |
| `File.list(<missing>)` / `File.list(<regular file>)` / `File.listFiles(<missing>)` | `null` |
| `File.length(<missing>)` / `File.lastModified(<missing>)` | `RET [0]` |

**§7.6 of W7-8 is closed by measurement.** Its two "not asserted because the
shipping natives do not implement them" rows —
`fsp_new_input_stream` accepting `WRITE` and `fsp_new_output_stream` accepting
`READ` — are **implemented in this tree now**, in
`fsp_input_stream_option_refusal` / `fsp_output_stream_option_refusal`
(`nio_file.rs`), and the strings they build are **character-for-character what
the oracle printed**, quotes and all. Nothing to do; recorded because W7-8 still
lists them as open.

### 3.7 Pushback streams

| call | oracle result |
|---|---|
| fresh `PushbackReader(new StringReader("abc"))`: `read()`×4 | `97`, `98`, `99`, `-1` |
| after `unread('Z')`: `read()` | `90` |
| `pr.ready()` at end of input | `RET [true]` |
| `pr.unread` with a full buffer | `THROW java.io.IOException msg=[Pushback buffer overflow]` |
| `pr.read()` / `ready()` / `unread()` after `close()` | `THROW java.io.IOException msg=[Stream closed]` |
| `pr.close()` twice | `RET [void]` |
| `pi.unread` with a full buffer | `THROW java.io.IOException msg=[Push back buffer is full]` |
| `pi.read()` / `available()` / `unread()` after `close()` | `THROW java.io.IOException msg=[Stream closed]` |
| `new PushbackReader(r, 0)` / `(r, -1)` | `THROW java.lang.IllegalArgumentException msg=[size <= 0]` |
| `new PushbackInputStream(i, 0)` / `(i, -1)` | `THROW java.lang.IllegalArgumentException msg=[size <= 0]` |

---

## 4. What changed in `nio_file.rs` — the shipping surface

Every row here is in `register_phase57_nio_file`, which `vm_init.rs` calls
directly in **both** shipping arms. The "before" column is what the source did;
the "after" column is what the source now does. **Neither has been run.**

| # | site | before (source) | after (source) | oracle |
|---|---|---|---|---|
| 1 | `FileSystemProvider.newDirectoryStream(Path, Filter)` — host arm | `match std::fs::read_dir(&p) { …, Err(_) => vec![] }` → **empty DirectoryStream** for a missing dir, a regular file, or a permission failure | `p57_dir_listing_refusal` before any allocation: `NoSuchFileException` / `NotDirectoryException`; the surviving `read_dir` error maps to `AccessDeniedException` or `p57_io_error` | §3.1 |
| 2 | same — the `Filter` argument | **never read.** Every entry returned regardless of the filter | `accept(Object)Z` invoked per entry under pins; only accepted entries survive | §3.1 |
| 3 | `Files.list(Path)` | `vfs_or_host_list` → empty Stream on any failure | refuses first, same two types | §3.1 |
| 4 | `Files.walk(Path,…)` (both overloads) | `out.push(p)` unconditionally → a **one-element Stream containing a path that does not exist** | refuses a missing start element; a regular file still yields itself | §3.1 |
| 5 | `Files.walk(Path,I,…)` maxDepth | `Some(Int(n)) if *n >= 0 => n, _ => usize::MAX` → a **negative depth became an UNBOUNDED walk** | `IllegalArgumentException("'maxDepth' is negative")` | §3.1 |
| 6 | `Files.find(Path,I,…)` | both defects above | both refusals | §3.1 |
| 7 | `DirectoryStream.iterator()` closed message | `"directory stream is closed"` | `"Directory stream is closed"` | §3.1 |
| 8 | `DirectoryStream.iterator()` called twice | a **second live iterator over the same listing** | `IllegalStateException("Iterator already obtained")`, latched in the new slot 2 | §3.1 |

Row 1 is the largest thing this lane found and the reason it exists. An empty
listing is a *legal* answer, so it propagates without a diagnostic anywhere: a
`Files.list(dir)` over a mistyped or not-yet-created path deletes nothing,
copies nothing, scans nothing, and reports success. Rows 3, 4 and 6 are the same
defect reached through the three `Stream`-returning front doors.

Row 2 is worse than "incomplete": `Files.newDirectoryStream(dir, "*.txt")`
builds a glob-backed `Filter`, and ignoring it means
`for (Path q : ds) Files.delete(q);` deletes the files the glob **excluded**.

Supporting helpers added (all `pub(crate)`, all new names, no existing signature
touched): `p57_not_directory`, `p57_dir_listing_refusal`,
`p57_max_depth_refusal`.

### 4.1 Two deliberate deviations, stated rather than buried

* **The filter runs eagerly.** HotSpot applies it in `hasNext()` and wraps a
  throwing filter in `DirectoryIteratorException` (measured, §3.1). CratonVM's
  listing has always been eager — the whole `Object[]` is materialised at open —
  so a throwing filter now surfaces from `newDirectoryStream` carrying its own
  exception. Both are loud. The silent "filter ignored" it replaces was not.
* **`p57_dir_listing_refusal` classifies with `symlink_metadata`/`metadata`,
  not with `read_dir`'s `ErrorKind`.** `ErrorKind::NotADirectory` is a recent
  stabilisation and Windows reports the same condition as a raw OS code, so
  keying on it would make the check toolchain- and platform-dependent. jar: and
  jrt: paths are classified by `vfs_classify`, which is the same authority
  `Files.exists`/`isDirectory` already use — including its deliberate
  "the root of an ABSENT jar is a `Dir`" rule, which exists so javac can walk a
  `Class-Path:` entry that is not on disk. That rule is preserved.

---

## 5. What changed in `native-io/src/lib.rs`

### 5.1 The `java.io` after-close family — shipping, and the biggest row here

`native-io`'s registrations are live in every mode. Every body below resolved
its descriptor with `fis_get_fd`/`fos_get_fd` and answered a *success-shaped
value* when that returned `None`:

| # | site | before (source) | after (source) | oracle |
|---|---|---|---|---|
| 9 | `native_fis_read` | `None => Ok(Some(Int(-1)))` — **EOF** | `None if fis_is_closed(..) => IOException("Stream Closed")` | §3.2 |
| 10 | `native_fis_read_bytes` | `None => Ok(Some(Int(-1)))` | same | §3.2 |
| 11 | `native_fis_read_byte_array` | `None => Ok(Some(Int(-1)))` | same | §3.2 |
| 12 | `native_fis_available` | `None => Ok(Some(Int(0)))` | same | §3.2 |
| 13 | `native_fis_skip` | `None => Ok(Some(Long(0)))` | same | §3.2 |
| 14 | `native_fos_write_byte` | `None => Ok(None)` — **a void write that wrote nothing** | `None if fos_is_closed(..) => IOException("Stream Closed")` | §3.2 |
| 15 | `native_fos_write_bytes` | `None => Ok(None)` | same | §3.2 |
| 16 | `native_fos_write_byte_array` | `None => Ok(None)` | same | §3.2 |
| 17 | `native_fos_write_byte_ignore_append` (the JDK 25 `write(I,Z)` descriptor) | `None => Ok(None)` | same | §3.2 |
| 18 | `native_fos_write_bytes_ignore_append` (the JDK 25 `writeBytes([BIIZ)` descriptor) | `None => Ok(None)` | same | §3.2 |

Row 9 is `W7-8` §3.1 rows 1–2 one package over, on the *shipping* path:
`-1` is the value every copy loop in the world stops on, and
`native_fis_close`'s own comment said so out loud — *"Mark the descriptor closed
so a double-close / post-close read is a **clean EOF**"*. Rows 14–18 are the
purest form of the species in this whole record: a `void` method that returns
having done nothing.

**The safety gate is the point.** `fis_get_fd`/`fos_get_fd` answer `None` for
two unrelated reasons — closed, or "this receiver never carried a descriptor
this crate understands" — and only the first is an `IOException`. Turning every
`None` into a throw is precisely the blanket widening
`HANDOFF-20260814.md` §5 warns about, and it would fire on `System.in`
lookalikes, subprocess streams and legacy synthetic layouts. So the new
`io_stream_is_closed` refuses **only** on a marker a close is the only thing
that writes: `fd` *and* `handle` both negative on the `FileDescriptor`, or
`Int(v < 0)` in instance slot 0. Every other `None` keeps its previous answer,
byte for byte.

Both close routes write that marker — `native_fd_close0` (which is what the
real-JDK `FileOutputStream.close()` bytecode reaches, via
`FileDescriptor.closeAll`) and the fallback `native_fis_close`/`native_fos_close`
— so the gate is reachable however the close was dispatched.

**A control that was NOT changed:** `native_fos_flush` returns `Ok(None)` when
the fd is gone, and the oracle says `fos.flush()` after close returns `void`.
That is correct and stays.

### 5.2 `Files.list` / `Files.walk` — the `--synthetic-jdk` half of the twin

| # | site | before | after |
|---|---|---|---|
| 19 | `native_files_list` | `collect_dir_entries` swallows `read_dir`'s `Err` → empty Stream for a missing directory | `files_listing_missing_refusal` → typed `NoSuchFileException` |
| 20 | `native_files_walk` | same | same |

The `<regular file>` half is **not** repaired here:
`cratonvm_types::error::RuntimeError` has no `NotDirectoryException` variant and
adding one is a change to `types/src/error.rs`, outside this lane — see
[nomination N1](#8-nominations). The shipping half in `nio_file.rs` *does* raise
the typed exception, so the two halves are deliberately unequal and the doc
comment on `files_listing_missing_refusal` says so, along with the full
per-mode winner table from §2.1.

---

## 6. Which claimed fixes are actually in the tree

`HANDOFF-20260814.md` records two occasions where a fix existed but never ran.
Every claim this lane depended on was therefore re-checked against the source,
not against the record.

| record's claim | verdict on this tree |
|---|---|
| W7-8 §7.6 — `newInputStream(WRITE)` / `newOutputStream(READ)` refusals "not implemented" | **PRESENT and CORRECT.** `fsp_input_stream_option_refusal` / `fsp_output_stream_option_refusal` exist and their strings match the oracle exactly, quotes and all. The record is stale. |
| W7-8 §9.6 item 7 — `AsynchronousFileChannel.force(Z)V` is a silent no-op; `afc_sync_at` "does not exist" | **BOTH LANDED.** `afc_sync_at` is at `native-io/src/lib.rs`, and `net_channels.rs:1756` registers a `force` that calls it. Its read-only guard (`if !handle.writable { return Ok(()) }`) agrees with an oracle row (§3.5) nobody had measured. |
| W7-8 §9.5 — `native_afc_size` on a closed channel is a bare `IOException` | **FIXED.** The body now checks `AFC_FIELD_OPEN` before touching the fd and raises the typed closed-channel error. |
| W7-57 rows 1–7 (`io_streams.rs` close/flush propagation) | **PRESENT**, all seven. |
| W7-57 rows 24–29 (`native-io` close/flush propagation) | **PRESENT**, all six; the record's line anchors have drifted by up to ~1300 lines and must not be used as anchors. |
| W7-57 row 12 — `native_input_stream_reader_close` "repaired for consistency, deadness recorded" | **STALE.** The function no longer exists; it was deleted with a do-not-restore note. |
| W7-70 — `native_fd_close0`'s CLOSE half still swallowed | **STILL OPEN**, exactly as described, and deliberately so. Not widened here; see §7. |
| W7-70 — `native_fis_close`'s close still swallowed | **STILL OPEN**, same standing. |
| W7-53 — 7 open blocking-close sites | **NONE are in this lane's three files.** All seven live in `pipe.rs`, `net_channels.rs`, `servlet.rs` and `t27_tls.rs`. Nothing actionable here. |
| W7-8 §9.1 — "`register_io_natives` is last in all three arms" | **WRONG for both real-JDK arms.** See §2.1. This one would have cost a whole fix. |

---

## 7. What this lane did NOT do

* **It did not run CratonVM.** There is no binary and the tree is mid-merge. The
  entire CratonVM column of every table above is a reading of source. No claim
  in this record has been observed.
* **It did not build, check or test anything.** `rustfmt --edition 2021 --check`
  was run on all three files as a syntax check; all three parse, and the only
  formatting diffs reported are pre-existing and outside the edited regions
  (`io_streams.rs` is now fully rustfmt-clean, exit 0).
* **It did not touch `native_fos_flush`.** Measured correct (§3.2).
* **It did not widen `native_fis_available`'s trailing `unwrap_or(0)`.**
  `FdTable::available` ends in `_ => Err(..)` for every entry kind that is not a
  file read or a child pipe, so propagating it would start throwing on receivers
  no measurement here covers, and `0` is a legal answer for `available()` in a
  way that `-1` is not for `read()`. Named at the site, left alone.
* **It did not repair `ois_read_byte`'s `_ => -1`.** A sentinel (`i32::MIN` for
  the `Err` arm) was written and then **withdrawn**, because `readByte` does
  `b as i8 as i32` and `i32::MIN as i8` is `0` — the sentinel silently turned a
  failed read into the byte zero at one of four call sites. Half-distinguishing
  a failure is worse than naming it. The real repair is a signature change
  across eleven call sites in a `--synthetic-jdk`-only stub, and it is not worth
  it. The withdrawal is recorded in the source comment so the next reader does
  not re-derive it.
* **It did not act on §3.3's suppression asymmetry.**
  `BufferedOutputStream.close()` reports the close error and suppresses the
  flush error; `BufferedWriter.close()` does the opposite. Both are measured;
  neither matches what one would derive; and every CratonVM close body that
  could express the difference is either already propagating one half or is
  outside this lane. Measured and left for whoever owns `addSuppressed`.
* **It did not repair `Files.walk`'s mid-walk swallow.** `vfs_or_host_walk`
  descends via `vfs_or_host_list`, which still answers an unreadable
  *subdirectory* with an empty Vec. Only the START element is now checked. The
  JDK surfaces mid-walk failures as `UncheckedIOException` at stream
  consumption, which this eager walker has no place to raise; doing it properly
  means making the walk lazy. Named, not counted.
* **It did not repair the legacy-synthetic `FileOutputStream` close marker.**
  `native_fos_close` writes `fd = -1` only when a real `FileDescriptor` object
  exists; for the legacy slot-0-Int layout it leaves the old positive fd in
  place, so `fos_get_fd` still answers `Some` after a close and a subsequent
  write reaches a released (possibly recycled) fd. That is a different defect
  from this record's species and widening into it blind is what §5.1's gate
  exists to avoid.
* **It did not add anything under `regression-suite/`.** The vector is in
  [§11](#11-the-regression-vector-source-only) as source only, per the lane's
  constraints.
* **It ran no state-changing git command.** The tree is mid-merge.

---

## 8. NOMINATIONS

Ordered by severity. Every one is outside this lane's three files.

**N1 — `types/src/error.rs`: add a `NotDirectoryException` variant.**
`RuntimeError` has `NoSuchFileException` but no `NotDirectoryException`, so
`native-io`'s `Files.list`/`Files.walk` (the `--synthetic-jdk` winners) cannot
raise the type the oracle raises for a regular file (§3.1). Add
```rust
#[error("NotDirectoryException: {path}")]
NotDirectoryException { path: String },
```
beside `NoSuchFileException` (`types/src/error.rs:1063-1064`), and the matching
arm in the class-mapping table (`:1595-1597`):
```rust
RuntimeError::NotDirectoryException { path } => {
    ("java/nio/file/NotDirectoryException", Some(path.as_str()))
}
```
Then extend `files_listing_missing_refusal` in `native-io/src/lib.rs` to refuse
a non-directory. Small, mechanical, and it closes the last asymmetry between the
two halves of the `Files.list` twin.

**N2 — `native-builtins/src/phases_late/net_channels.rs:1506-1516`:
`AsynchronousFileChannel.lock()` still mints a `java/util/concurrent/FutureTask`
by field index and `get()` never returns.** W7-8 §9.3/§9.4 diagnosed this and
§9.6 item 8 carries the patch; `force` beside it has since been repaired but
`lock` has not. The oracle now supplies the target shape this record did not
have: `afc.lock()` on an **open** channel returns a `sun.nio.ch.PendingFuture`
that completes with a real `FileLock`; on a **closed** channel it returns a
`CompletedFuture` and does **not** throw — the `ClosedChannelException` arrives
at `get()`. So the honest fix is (a) route to `native_afc_try_lock`'s existing
OS-advisory-lock plumbing and complete the future with the resulting lock, and
(b) for a closed channel complete the future *exceptionally* rather than
throwing from `lock()`. Do **not** land §9.6's one-line
`aio_completed_future(ctx, Value::Object(None))` on its own: a `Future<FileLock>`
completing with `null` trades a hang for a `NullPointerException` in the
caller's `try (FileLock l = f.get())`.

**N3 — `native-builtins/src/phases_late/net_channels.rs:1816`:
`AsynchronousServerSocketChannel.accept()` has the same `FutureTask` mint and is
the sole registrant.** Same species, same fix shape. A ratchet on the string
`try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/FutureTask"` in that
file is worth more than either individual repair — W7-8 §9.6 item 8 says so and
it is still true.

**N4 — `native-builtins/src/phases_late.rs:7538`, `oos_write_bytes`: the
`ObjectOutputStream` write family drops every delegate failure.** Eleven
`writeInt`/`writeLong`/`writeUTF`/`writeBoolean`/… bodies in
`io_streams.rs::register_p70_object_streams` call it and it returns `()`, so a
failing sink is reported as a successful serialization. This is W7-8 §3.4 rows
50–58's shape, one class over. The helper is one file outside this lane; give it
a `Result` return and propagate at the eleven call sites (which ARE in this
lane's `io_streams.rs`, so the follow-up is cheap once the signature moves).
`--synthetic-jdk` only.

**N5 — `native-builtins/src/lib.rs` (`register_hex_format_real_jdk_natives`
pattern): promote `register_p58_pushback` + `register_p66_pushback_reader` out
of `register_synthetic_overrides`, or delete them.** The whole of
`io_streams.rs` is in `SYNTHETIC_ONLY_CLOSURE`, so in `--jdk-only` the real
`java.io.PushbackReader` bytecode serves those triples and everything in §5 of
this record is invisible there. Either state is fine; the current state — a
registrar that reads as coverage and is not coverage — is the one
`registrar_reachability.rs`'s own module doc names as the campaign's most
repeated mistake. Decide and record which.

**N6 — `vm/src/vm/vm_init.rs`: the `register_io_natives` /
`register_phase57_nio_file` ordering needs a comment or a ratchet.** Two files
register `Files.list`/`Files.walk` with different bodies and the winner flips
between the synthetic and real-JDK arms (§2.1). W7-8 §9.1 got this wrong in
print. `essential_wiring_ratchet.rs` already pins six named triples; these two
belong in it.

**N7 — `native-builtins/src/logging_shims.rs`, `native_printstream_close` and
the `PrintStream` write family: a write after `close()` is not refused, and the
error flag is never set.** §3.4 measures the assertable contract precisely — a
clean `close()` leaves `checkError()` **false**, and the first write after that
close makes it **true**, with no exception thrown. W7-70 lists this as an open
residual without a measurement; the measurement is now in §3.4 and can be
asserted from Java directly. Also open there: six unregistered `PrintStream`
methods (`print(char[])`, `println(char[])`, `write(byte[])`,
`writeBytes(byte[])`, `append(CharSequence,int,int)`, `append(char)`).

**N8 — `native-builtins/src/phases_late/net_channels.rs` /
`native-io/src/datagram.rs`: `setSoTimeout`'s `.max(0)`.** W7-8 §3.6 and §7.3
left this unsettled for want of a specification sentence. §3.5 supplies the
measurement instead, and it is the same on both adaptors:
`IllegalArgumentException("timeout < 0")`, plus `SocketException("Socket is
closed")` after close. Clamping a negative to zero converts "refuse this" into
"block forever".

**N9 — `native-builtins/src/phases_late/nio_file.rs` is not named once in
W7-53, W7-57 or W7-70, and it is the largest shipping `java.nio.file` surface in
the tree.** 237 registrations in `register_phase57_nio_file` alone. This lane
adjudicated the directory-listing family and the option-scan family; the
attribute, copy/move and `FileStore` families in the same registrar were **not**
swept against §3.6's oracle table, which is now available for whoever does.

---

## 9. Probe sources and raw transcripts

Four probes, run with

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
"$JAVA_HOME/bin/java" <Probe>.java     # single-file source mode, no javac step
```

`java -version` on the host:

```
openjdk version "25.0.3" 2026-04-21 LTS
OpenJDK Runtime Environment Temurin-25.0.3+9 (build 25.0.3+9-LTS)
OpenJDK 64-Bit Server VM Temurin-25.0.3+9 (build 25.0.3+9-LTS, mixed mode, sharing)
```

`C:\craton\jdk25src` is **ABSENT on this host**, so no JDK source was read.
Every JDK claim in this record is a measurement or a `javap`-visible fact, never
a quotation of a body.

All four probes share this reporter, which prints `null` distinctly from the
empty string and keeps every label ASCII:

```java
static void p(String label, Object v) {
    String s;
    if (v == null) s = "null";
    else if (v instanceof Throwable) {
        Throwable t = (Throwable) v;
        String m = t.getMessage();
        s = "THROW " + t.getClass().getName() + " msg=" + (m == null ? "null" : "[" + m + "]");
    } else s = "RET [" + v + "]";
    System.out.println(label + " | " + s);
}
interface Act { Object run() throws Throwable; }
static void t(String label, Act a) {
    try { p(label, a.run()); } catch (Throwable e) { p(label, e); }
}
```

### 9.1 `FilesProbe.java` — the `Files.*` surface (§3.6)

```java
Path base    = Files.createTempDirectory("fprobe");
Path missing = base.resolve("missing.txt");
Path file    = Files.write(base.resolve("file.txt"), "hello".getBytes());
Path dir     = Files.createDirectory(base.resolve("dir"));
Path fullDir = Files.createDirectory(base.resolve("fulldir"));
Files.write(fullDir.resolve("inner.txt"), "x".getBytes());

t("deleteIfExists(fullDir)",      () -> Files.deleteIfExists(fullDir));
t("createDirectories(existingFile)", () -> Files.createDirectories(file));
t("newInputStream(file, WRITE)",  () -> Files.newInputStream(file, StandardOpenOption.WRITE));
t("newOutputStream(file, READ)",  () -> Files.newOutputStream(file, StandardOpenOption.READ));
t("copy(file,dst,ATOMIC_MOVE)",   () -> Files.copy(file, base.resolve("cd5.txt"),
                                                   StandardCopyOption.ATOMIC_MOVE));
t("setAttribute(file2,size)",     () -> Files.setAttribute(file2, "size", 5L));
t("default fs close()",           () -> { FileSystems.getDefault().close(); return "void"; });
```

Selected transcript (full table in §3.6):

```
deleteIfExists(fullDir) | THROW java.nio.file.DirectoryNotEmptyException msg=[...\fulldir]
createDirectories(existingFile) | THROW java.nio.file.FileAlreadyExistsException msg=[...\file.txt]
newInputStream(file, WRITE) | THROW java.lang.UnsupportedOperationException msg=['WRITE' not allowed]
newInputStream(file, APPEND) | THROW java.lang.UnsupportedOperationException msg=['APPEND' not allowed]
newInputStream(file, CREATE) | RET [sun.nio.ch.ChannelInputStream@6f603e89]
newOutputStream(file, READ) | THROW java.lang.IllegalArgumentException msg=[READ not allowed]
newByteChannel(file, READ,APPEND) | THROW java.lang.IllegalArgumentException msg=[READ + APPEND not allowed]
copy(file,dst,ATOMIC_MOVE) | THROW java.lang.UnsupportedOperationException msg=[Unsupported copy option: ATOMIC_MOVE]
copy(stream,dst,COPY_ATTRIBUTES) | THROW java.lang.UnsupportedOperationException msg=[COPY_ATTRIBUTES not supported]
move(file,dst,COPY_ATTRIBUTES) | THROW java.lang.UnsupportedOperationException msg=[Unsupported option: COPY_ATTRIBUTES]
setAttribute(file2,bogus:x) | THROW java.lang.UnsupportedOperationException msg=[View 'bogus' not available]
setAttribute(file2,size) | THROW java.lang.IllegalArgumentException msg=['basic:size' not recognized]
setPosixFilePermissions(file2) | THROW java.lang.UnsupportedOperationException msg=null
createLink(new,missing) | THROW java.nio.file.NoSuchFileException msg=[...\hard2.txt -> ...\missing.txt]
isSameFile(missing,missing) | RET [true]
default fs isOpen | RET [true]
default fs close() | THROW java.lang.UnsupportedOperationException msg=null
default fs isOpen after close | RET [true]
exists(null) | THROW java.lang.NullPointerException msg=[Cannot invoke "java.nio.file.Path.getFileSystem()" because "path" is null]
```

**Two rows from this probe are withheld deliberately.**
`Files.createSymbolicLink` and `Files.readSymbolicLink` both failed with an OS
message rendered through the Windows console code page and came out as
mojibake. Their *types* are `java.nio.file.FileSystemException` and
`java.nio.file.NotLinkException`; their messages are locale-dependent host
strings and are **not** transcribed here, because transcribing a mojibake string
is how a differential fails with every assertion passing
(`HANDOFF-20260814.md` §7).

### 9.2 `StreamProbe.java` — close/flush semantics and `PrintStream` (§3.2–3.4)

```java
static class BadOut extends OutputStream {
    int closes = 0, flushes = 0;
    public void write(int b) throws IOException { throw new IOException("write boom"); }
    public void flush() throws IOException { flushes++; throw new IOException("flush boom"); }
    public void close() throws IOException { closes++; throw new IOException("close boom"); }
}

BadOut bad = new BadOut();
BufferedOutputStream bos = new BufferedOutputStream(bad);
t("bos.write (buffered, no flush)", () -> { bos.write(65); return "void"; });
t("bos.flush -> underlying flush",  () -> { bos.flush(); return "void"; });
t("bos.close -> flush then close",  () -> { bos.close(); return "void"; });

PrintStream ps2 = new PrintStream(new ByteArrayOutputStream(), true);
t("ps2.close",                            () -> { ps2.close(); return "void"; });
t("ps2.checkError after clean close",     () -> ps2.checkError());
t("ps2.println after close",              () -> { ps2.println("after"); return "void"; });
t("ps2.checkError after write-after-close", () -> ps2.checkError());
```

Selected transcript:

```
fos.close #1 | RET [void]
fos.close #2 (double) | RET [void]
fos.flush after close | RET [void]
fos.write after close | THROW java.io.IOException msg=[Stream Closed] suppressed=0
fis.read after close | THROW java.io.IOException msg=[Stream Closed] suppressed=0
fis.available after close | THROW java.io.IOException msg=[Stream Closed] suppressed=0
bos.flush -> underlying flush | THROW java.io.IOException msg=[write boom] suppressed=0
bos.close -> flush then close | THROW java.io.IOException msg=[close boom] suppressed=1 {java.io.IOException:write boom}
bos.close #2 | RET [void]
bad.closes | RET [1]
bad.flushes | RET [0]
bw.close | THROW java.io.IOException msg=[w write boom] suppressed=1 {java.io.IOException:w close boom}
bw.write after close | THROW java.io.IOException msg=[Stream closed] suppressed=0
osw.write after close | THROW java.io.IOException msg=[Stream closed] suppressed=0
osw file len | RET [5]
bw.write(String,0,-1) | RET [void]
bw.write(String,-1,2) | THROW java.lang.StringIndexOutOfBoundsException msg=[Range [-1, 1) out of bounds for length 6]
bw.write(String,3,10) | THROW java.lang.StringIndexOutOfBoundsException msg=[Range [3, 13) out of bounds for length 6]
bw.write(char[],0,-1) | THROW java.lang.IndexOutOfBoundsException msg=[Range [0, 0 + -1) out of bounds for length 6]
bw.write(char[],-1,2) | THROW java.lang.IndexOutOfBoundsException msg=[Range [-1, -1 + 2) out of bounds for length 6]
twr body throws, close throws | THROW java.lang.IllegalStateException msg=[body boom] suppressed=1 {java.io.IOException:close boom}
twr body ok, close throws | THROW java.io.IOException msg=[close boom] suppressed=0
ps2.checkError after clean close | RET [false]
ps2.println after close | RET [void]
ps2.checkError after write-after-close | RET [true]
System.out class | RET [java.io.PrintStream]
```

Two bonus confirmations of W7-8 §3.2/§3.5 that were previously source-only:
`BufferedWriter.write(String, 0, -1)` really does write nothing and throw
nothing, while `write(char[], 0, -1)` really does throw — and the two message
*formats* differ (`Range [a, b)` for the String overload, `Range [off, off + len)`
literally for the char[] one). A single shared helper cannot produce both.

### 9.3 `ChanProbe.java` — channels (§3.5)

```java
FileChannel ch = FileChannel.open(f, StandardOpenOption.READ, StandardOpenOption.WRITE);
ch.close();
t("fc.read after close", () -> ch.read(ByteBuffer.allocate(4)));

AsynchronousFileChannel afc = AsynchronousFileChannel.open(f, READ, WRITE);
afc.close();
t("afc.force(true) closed", () -> { afc.force(true); return "void"; });
t("afc.lock() closed",      () -> afc.lock());

ServerSocketChannel s2 = ServerSocketChannel.open();
s2.bind(new InetSocketAddress("127.0.0.1", 0));
final Object[] box = new Object[1];
Thread th = new Thread(() -> { try { box[0] = s2.accept(); } catch (Throwable e) { box[0] = e; } });
th.start(); Thread.sleep(400); s2.close(); th.join(3000);
p("blocking accept interrupted by close", box[0]);
```

Selected transcript:

```
fc class | RET [sun.nio.ch.FileChannelImpl]
fc.read after close | THROW java.nio.channels.ClosedChannelException msg=null
c2.position(-1) | THROW java.lang.IllegalArgumentException msg=null
c2.read(buf,-1) | THROW java.lang.IllegalArgumentException msg=[Negative position]
c2.truncate(-1) | THROW java.lang.IllegalArgumentException msg=[Negative size]
c2.truncate(bigger than size) | RET [11]
ro.force | RET [void]
afc class | RET [sun.nio.ch.WindowsAsynchronousFileChannelImpl]
afc.lock() class | RET [sun.nio.ch.PendingFuture]
afc.force(true) closed | THROW java.nio.channels.ClosedChannelException msg=null
afc.tryLock() closed | THROW java.nio.channels.ClosedChannelException msg=null
afc.lock() closed | RET [sun.nio.ch.CompletedFuture@7f811d00]
afc.read closed | RET [sun.nio.ch.CompletedFuture@f19c9d2]
afcro.force(true) | RET [void]
afcro.write | THROW java.nio.channels.NonWritableChannelException msg=null
ssc.socket().setSoTimeout(-1) | THROW java.lang.IllegalArgumentException msg=[timeout < 0]
blocking accept interrupted by close | THROW java.nio.channels.AsynchronousCloseException msg=null
blocking accept interrupted by Thread.interrupt | THROW java.nio.channels.ClosedByInterruptException msg=null
s3.isOpen after interrupt | RET [false]
dc.socket().setSoTimeout after close | THROW java.net.SocketException msg=[Socket is closed]
```

### 9.4 `WalkProbe.java` / `MsgProbe.java` — listing (§3.1) and `File` booleans

```java
t("list(missing) construct", () -> Files.list(missing));
t("list(file) construct",    () -> Files.list(file));
t("walk(missing).count()",   () -> Files.walk(missing).count());
t("walk(file).count()",      () -> Files.walk(file).count());
t("walk(dir,-1)",            () -> Files.walk(dir, -1));
t("newDirectoryStream(dir,glob *.txt)", () -> {
    List<String> names = new ArrayList<>();
    try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir, "*.txt")) {
        for (Path q : ds) names.add(q.getFileName().toString());
    }
    return names;
});

DirectoryStream<Path> ds = Files.newDirectoryStream(dir);
ds.iterator();
try { ds.iterator(); } catch (Throwable e) { /* Iterator already obtained */ }
```

Transcript:

```
list(missing) construct | THROW java.nio.file.NoSuchFileException msg=[...\missing]
list(file) construct | THROW java.nio.file.NotDirectoryException msg=[...\f.txt]
list(dir).count | RET [2]
walk(missing) construct | THROW java.nio.file.NoSuchFileException msg=[...\missing]
walk(file).count | RET [1]
walk(dir).count | RET [3]
walk(dir,-1) | THROW java.lang.IllegalArgumentException msg=['maxDepth' is negative]
find(missing,1,(p,x)->true).count | THROW java.nio.file.NoSuchFileException msg=[...\missing]
find(dir,-1,...) | THROW java.lang.IllegalArgumentException msg=['maxDepth' is negative]
newDirectoryStream(dir,glob *.txt) | RET [[a.txt]]
newDirectoryStream(dir,filter reject-all) | RET [[]]
newDirectoryStream(dir,filter throws) | THROW java.nio.file.DirectoryIteratorException msg=[java.io.IOException: filter boom] cause=java.io.IOException
second iterator | THROW java.lang.IllegalStateException msg=[Iterator already obtained]
closed iterator | THROW java.lang.IllegalStateException msg=[Directory stream is closed]
hasNext after close | RET false
next after close | THROW java.util.NoSuchElementException msg=[null]
walk(dir,0).count | 1
provider.newDirectoryStream(file, all) | THROW java.nio.file.NotDirectoryException msg=[...\f.txt]
File.delete(missing) | RET [false]
File.mkdir(existing) | RET [false]
File.setLastModified(file,-1) | THROW java.lang.IllegalArgumentException msg=[Negative time]
File.list(missing) | null
File.list(file) | null
File.length(missing) | RET [0]
```

### 9.5 `PushProbe.java` — pushback (§3.7)

```java
PushbackReader pr = new PushbackReader(new StringReader("abc"));
t("pr.read #1 (no pushback)",   () -> pr.read());
t("pr.unread twice (overflow)", () -> { pr.unread('A'); pr.unread('B'); return "void"; });
t("pr.close",                   () -> { pr.close(); return "void"; });
t("pr.read after close",        () -> pr.read());
t("new PushbackReader(r,0)",    () -> new PushbackReader(new StringReader("x"), 0));
```

Transcript:

```
pr.read #1 (no pushback) | RET [97]
pr.read #2 | RET [98]
pr.read after unread | RET [90]
pr.read at EOF | RET [-1]
pr.ready at EOF | RET [true]
pr.unread twice (overflow) | THROW java.io.IOException msg=[Pushback buffer overflow]
pr.read after close | THROW java.io.IOException msg=[Stream closed]
pr.ready after close | THROW java.io.IOException msg=[Stream closed]
pr.close twice | RET [void]
pi.unread overflow | THROW java.io.IOException msg=[Push back buffer is full]
pi.read after close | THROW java.io.IOException msg=[Stream closed]
new PushbackReader(r,0) | THROW java.lang.IllegalArgumentException msg=[size <= 0]
new PushbackInputStream(i,-1) | THROW java.lang.IllegalArgumentException msg=[size <= 0]
```

---

## 10. What the orchestrator must check at build time

Ordered by how much a wrong answer costs.

1. **`--dump-native-registry`, on `java/nio/file/Files.list` and
   `java/nio/file/Files.walk`.** §2.1's per-mode winner table is read from
   `vm_init.rs` source, and W7-8 §9.1 got the same question wrong in print.
   Confirm `owns_slot=true` and a non-zero `invocations` for the
   `nio_file.rs` bodies in `--jdk-only`. **If `native-io` wins instead, half of
   §4 is inert** and the repairs must move.
2. **Same check on `java/nio/file/spi/FileSystemProvider.newDirectoryStream`.**
   It has exactly one registration in the tree, so this should be
   uncontroversial — but "exactly one registration" is what §7.1 of W7-8
   believed too.
3. **Highest regression risk: `Files.list` and `Files.walk` over a path that is
   not there now THROW.** Classpath and resource scanners that walked a missing
   directory and silently saw nothing will now see a `NoSuchFileException`.
   HotSpot throws, so anything relying on the old answer was relying on a
   divergence — but it will surface as *new* failures, not as fixed ones. If a
   vector goes red on `NoSuchFileException` out of a scan, that is this change
   and the caller is what needs the `Files.exists` guard.
4. **Second-highest: the `DirectoryStream$Filter` is now invoked.** If
   `ctx.invoke_virtual(f, "accept", "(Ljava/lang/Object;)Z", …)` cannot dispatch
   on `Files$AcceptAllFilter` (the filter the no-arg
   `Files.newDirectoryStream(Path)` passes), **every** directory listing in the
   VM fails loudly. This is the single riskiest edit in the lane. It follows
   `Files.find`'s `BiPredicate` loop exactly, including the pin discipline, but
   it has not been run.
5. **`java/nio/file/DirectoryStream` is now allocated with 3 fields, not 2.**
   Slot 2 is the "iterator already obtained" latch. Only `nio_file.rs` allocates
   or reads this object (verified by grep), but `try_alloc_concurrent_synthetic`
   resolves the real interface and clamps widths, so confirm nothing rejects the
   wider allocation.
6. **The after-close refusals in `native-io` fire on a marker, not on
   `None`.** If `--jdk-only` starts throwing `IOException: Stream Closed` from a
   stream that is demonstrably open, `io_stream_is_closed` is matching something
   it should not — most likely a `FileDescriptor` whose `handle` field is
   absent and reads back as a negative `Long`. That predicate is the first place
   to look, and relaxing it to require a *positive* prior fd is the fallback.
7. **`register_p66_pushback_reader` now delegates `read`/`ready` to the inner
   `Reader`.** `--synthetic-jdk` only, but it changes `PushbackReader` from
   "always EOF" to "actually reads", which will change the behaviour of anything
   that was silently getting nothing.
8. **`rustfmt --edition 2021 --check`** passes on all three files (parse-clean).
   `io_streams.rs` is fully clean (exit 0); the other two have pre-existing
   diffs only, none inside an edited region. Zero CR bytes, zero conflict
   markers, no duplicate `fn` names, and every new item is `pub(crate)` or
   private with exactly one definition.

---

## 11. The regression vector, source only

Not created under `regression-suite/` — this lane may not add files there. The
class name and the `JDKONLY_CLASSES` entry are left to whoever lands it.

Every assertion below is a **measured** oracle row from §3. The vector is
written so that each check names the divergence it pins, and so that no check
can pass vacuously: the listing arms count entries, and the refusal arms assert
the exception **class**, because that is what a `catch` clause keys on.

```java
import java.io.*;
import java.nio.file.*;
import java.util.*;

/**
 * Pins the fabricated-success rows measured in
 * docs/known-issues/jdk-only/G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
 *
 * Every expected value here was MEASURED on HotSpot 25.0.3+9-LTS, not derived.
 * Labels are ASCII only: a non-ASCII character in a printed label makes the
 * HotSpot comparison encoding-dependent (HANDOFF-20260814 section 7).
 */
public class RJdkIoFabricated {
    static int checks = 0, failures = 0;

    static void check(boolean ok, String what) {
        checks++;
        if (!ok) { failures++; System.out.println("FAIL: " + what); }
    }

    /** Assert that `body` throws exactly `expected` (or a subclass). */
    static void throwsAs(Class<? extends Throwable> expected, String what, Runnable body) {
        checks++;
        try {
            body.run();
            failures++;
            System.out.println("FAIL: " + what + " -> returned normally, expected "
                               + expected.getName());
        } catch (Throwable e) {
            Throwable t = (e instanceof RuntimeException && e.getCause() != null
                           && !(expected.isInstance(e))) ? e.getCause() : e;
            if (!expected.isInstance(t)) {
                failures++;
                System.out.println("FAIL: " + what + " -> " + t.getClass().getName()
                                   + ", expected " + expected.getName());
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Path base    = Files.createTempDirectory("rjdkio");
        Path missing = base.resolve("no-such-dir");
        Path file    = Files.write(base.resolve("f.txt"), "hello".getBytes());
        Path dir     = Files.createDirectory(base.resolve("d"));
        Files.write(dir.resolve("a.txt"), "a".getBytes());
        Files.write(dir.resolve("b.log"), "b".getBytes());

        // ---- listing refusals: an empty listing is NOT the answer ----------
        throwsAs(NoSuchFileException.class, "Files.list(missing)",
                 () -> { try { Files.list(missing).count(); }
                         catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(NotDirectoryException.class, "Files.list(regular file)",
                 () -> { try { Files.list(file).count(); }
                         catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(NoSuchFileException.class, "Files.walk(missing)",
                 () -> { try { Files.walk(missing).count(); }
                         catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(NoSuchFileException.class, "Files.newDirectoryStream(missing)",
                 () -> { try { Files.newDirectoryStream(missing).close(); }
                         catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(NotDirectoryException.class, "Files.newDirectoryStream(regular file)",
                 () -> { try { Files.newDirectoryStream(file).close(); }
                         catch (IOException e) { throw new RuntimeException(e); } });

        // ---- the listing still works, so none of the above is vacuous -----
        check(Files.list(dir).count() == 2,  "Files.list(dir) must still see 2 entries");
        check(Files.walk(dir).count() == 3,  "Files.walk(dir) must still see 3 elements");
        check(Files.walk(file).count() == 1, "Files.walk(regular file) yields the file itself");
        check(Files.walk(dir, 0).count() == 1, "Files.walk(dir, 0) yields only the root");

        // ---- maxDepth: negative is refused, not promoted to unbounded -----
        throwsAs(IllegalArgumentException.class, "Files.walk(dir, -1)",
                 () -> { try { Files.walk(dir, -1); }
                         catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IllegalArgumentException.class, "Files.find(dir, -1, ...)",
                 () -> { try { Files.find(dir, -1, (p, a) -> true); }
                         catch (IOException e) { throw new RuntimeException(e); } });

        // ---- the DirectoryStream filter is actually applied ---------------
        List<String> globbed = new ArrayList<>();
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir, "*.txt")) {
            for (Path q : ds) globbed.add(q.getFileName().toString());
        }
        check(globbed.size() == 1 && globbed.get(0).equals("a.txt"),
              "newDirectoryStream(dir, \"*.txt\") must filter; saw " + globbed);

        List<String> none = new ArrayList<>();
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir, q -> false)) {
            for (Path q : ds) none.add(q.getFileName().toString());
        }
        check(none.isEmpty(), "a reject-all filter must yield nothing; saw " + none);

        // ---- DirectoryStream is single-use, and says so ------------------
        DirectoryStream<Path> ds1 = Files.newDirectoryStream(dir);
        ds1.iterator();
        throwsAs(IllegalStateException.class, "second iterator()", () -> ds1.iterator());
        ds1.close();
        ds1.close(); // idempotent, must not throw
        throwsAs(IllegalStateException.class, "iterator() after close", () -> ds1.iterator());

        // ---- java.io after close: -1 and 0 are NOT the answers ------------
        Path rf = Files.write(base.resolve("r.txt"), "abc".getBytes());
        FileInputStream fis = new FileInputStream(rf.toFile());
        check(fis.read() == 'a', "open FileInputStream must still read");
        fis.close();
        fis.close(); // idempotent
        throwsAs(IOException.class, "FileInputStream.read() after close",
                 () -> { try { fis.read(); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "FileInputStream.read(byte[]) after close",
                 () -> { try { fis.read(new byte[4]); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "FileInputStream.available() after close",
                 () -> { try { fis.available(); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "FileInputStream.skip(1) after close",
                 () -> { try { fis.skip(1); } catch (IOException e) { throw new RuntimeException(e); } });

        Path wf = base.resolve("w.txt");
        FileOutputStream fos = new FileOutputStream(wf.toFile());
        fos.write("hi".getBytes());
        fos.close();
        fos.close(); // idempotent
        fos.flush();  // MEASURED: flush after close does NOT throw
        throwsAs(IOException.class, "FileOutputStream.write(int) after close",
                 () -> { try { fos.write(65); } catch (IOException e) { throw new RuntimeException(e); } });
        throwsAs(IOException.class, "FileOutputStream.write(byte[]) after close",
                 () -> { try { fos.write("x".getBytes()); } catch (IOException e) { throw new RuntimeException(e); } });
        // The bytes written BEFORE the close must still be on disk: this is what
        // makes the two rows above a real check and not just a new exception.
        check(Files.size(wf) == 2, "bytes written before close must survive; size="
                                   + Files.size(wf));

        // ---- PrintStream: the flag, not an exception ---------------------
        PrintStream ps = new PrintStream(new ByteArrayOutputStream(), true);
        check(!ps.checkError(), "fresh PrintStream must not be in error");
        ps.println("ok");
        check(!ps.checkError(), "a good write must not set the error flag");
        ps.close();
        check(!ps.checkError(), "a clean close must LEAVE the error flag false");
        ps.println("after");                 // must not throw
        check(ps.checkError(), "a write AFTER close must set the error flag");

        System.out.println("RJdkIoFabricated: " + checks + " checks, " + failures + " failures");
        if (failures != 0) throw new AssertionError(failures + " failures");
    }
}
```

Two notes for whoever lands it. `Files.newDirectoryStream(dir, "*.txt")` needs a
working `PathMatcher`; if CratonVM's glob support is the weak link, that check
will fail for a reason unrelated to this record — split it out rather than
deleting it. And the `PrintStream` block asserts [N7](#8-nominations), which is
**not** fixed by this lane, so it is expected to fail on CratonVM until that
nomination lands; either land N7 with the vector or comment that block with a
pointer to it.
