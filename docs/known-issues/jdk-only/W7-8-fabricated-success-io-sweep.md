# W7-8 — fabricated success across the `java.io` / `java.nio.file` native surface

Status: **swept and fixed in source, NOT built and NOT run.** No `cargo` command
of any kind was executed for this record and no CratonVM binary exists carrying
it. Every claim below is a claim about *source* and about the *JDK 25 oracle*,
never about observed behaviour.

Scope: `native-io/src/lib.rs` and
`native-builtins/src/phases_late/nio_file.rs` only. Findings outside those two
files are recorded under [Out-of-file patch (not applied)](#out-of-file-patch-not-applied).

Oracle: **JDK 25 source**, `lib/src.zip` from Eclipse Adoptium
25.0.3.9-hotspot on this host — the javadoc *and* the method bodies, so a
verdict can cite what the class says and what it does. Every row below quotes
the sentence it rests on. `javap` was not needed; the sources are better
evidence.

This is the same species as
W2-7-fabricated-success-where-the-spec-mandates-failure.md
(read that first), swept through the one subsystem where the JDK specifies
failures most densely and where a wrong answer is quietest: a `-1` from a read,
a `0` from a transfer, a `void` write that returns.

---

## 1. The two instruments, and what each one caught

**Instrument A — a clamp on an argument the spec range-checks is the same
defect as a swallowed `Err`.** Inherited from the `StringBuilder` lane, and it
is the higher-yield of the two here. It found `FileChannel.truncate(-1)`
emptying the file, `FileChannel.write(buf, -1)` overwriting the front of it,
`Buffer.position(int)` parking silently at the limit, and
`writeUTF`'s `.min(65535)`.

Its sharpest use is not finding clamps but **telling two neighbouring contracts
apart**. `java.io.BufferedWriter` has both halves of the distinction inside one
class:

| overload | negative `len` | region past the end |
|---|---|---|
| `write(String, int, int)` | **no exception** — "the implementation in this class does not throw such an exception in these cases but instead simply writes no characters" | `IndexOutOfBoundsException` |
| `write(char[], int, int)` | `IndexOutOfBoundsException` | `IndexOutOfBoundsException` |

One `.min(len)` had been standing in for both. A sweep that "hardens all the
clamps" would have made the first overload throw where the JDK is documented not
to — trading one divergence for another, which is exactly the mistake W2-7 #1
and #3 record. The two are now separate helpers in `native-io`
(`writer_string_region` / `buffered_writer_string_region`) precisely so the
difference cannot be re-collapsed by the next person.

**Instrument B — the error path.** `unwrap_or(0)`, `let _ =`, `Err(_) =>` and
`match` arms whose failure branch returns a plausible value. One question per
hit: **can the caller tell this from a real success?** That question, not the
grep, is what does the work:

* `Files.delete` discarding an error → **already fixed**, and the code says so.
* `File.delete()` returning `false` → **correct**, that is the specified answer.
* `FileChannel.read` returning `-1` on a closed channel → **fabricated**, because
  `-1` is the value every copy loop in the world stops on.

Same shape, three different verdicts. Counting `let _ =` occurrences would have
scored all three identically.

**Sizing.** The campaign README records that its grep-derived counts were
systematically wrong and always in the same direction, so nothing here is sized
from a count. The two files hold roughly 740 hits across the two patterns; the
grep was used only to find candidates. **77 rows were adjudicated one at a
time** — 59 FABRICATED (covering ~72 call sites, since three rows group a
repeated body), 13 SPEC-DEGRADATION, 5 UNMEASURED — and all 77 are listed
below. Everything else in those files was not examined, and this record makes
no claim about it.

---

## 2. Reachability, stated once so the table does not have to repeat it

A verdict is worthless without knowing whether the code runs. Three tiers:

| tier | meaning | where |
|---|---|---|
| **DEFAULT** | registered unconditionally in an ordinary build | `register_phase57_file_channel`, the `java/io/BufferedWriter` block in `nio_file.rs`, `java/io/StringReader`, `register_async_file_channel` |
| **FEATURE** | only under `--features synthetic-jdk` | `register_nio_natives` (the whole synthetic `Buffer` family), the `OutputStreamWriter`/`BufferedWriter` natives in `native-io` |
| **FLAG** | only under an opt-in env var | both synthetic `RandomAccessFile` implementations (`CRATONVM_SYNTHETIC_RAF=1`; real-RAF is the default, and both crates must skip together), `java/io/FileWriter` (`!real_filewriter_enabled()`) |

FLAG/FEATURE rows are still fixed rather than merely recorded — they are cheap,
mechanical, and in-file — but nothing in this record should be read as
predicting a change in a default-build measurement except via the DEFAULT rows.

**`java.io` refuses a directory open; `java.nio.file` must not.** Nothing here
touches `reject_directory_open` or moves a check into the fd table. That split
stays per-call-site.

---

## 3. Census

Verdicts: **FABRICATED** (fixed here) · **SPEC-DEGRADATION** (correct as-is,
with the sentence that licenses it) · **UNMEASURED** (cannot be settled without
running).

### 3.1 FABRICATED — `java.nio.channels.FileChannel` (DEFAULT)

`register_phase57_file_channel`, `native-builtins/src/phases_late/nio_file.rs`.
`close()` writes `-1` into slot 0, so `fd_id < 0` *is* "closed"; every row below
answered something else.

| # | site | JDK 25 says | CratonVM answered | verdict |
|---|---|---|---|---|
| 1 | `read(ByteBuffer)`, closed | "@throws ClosedChannelException If this channel is closed" | `-1` | FABRICATED |
| 2 | `read(ByteBuffer,long)`, closed | same | `-1` | FABRICATED |
| 3 | `read(ByteBuffer,long)`, `position < 0` | "@throws IllegalArgumentException If the position is negative or the buffer is read-only" | read from offset 0, reported as if from `position` | FABRICATED |
| 4 | `write(ByteBuffer,long)`, `position < 0` | "@throws IllegalArgumentException If the position is negative" | **overwrote the front of the file**, returned the byte count | FABRICATED |
| 5 | `position(long)`, `newPosition < 0` | "@throws IllegalArgumentException If the new position is negative" | seek to 0, returned `this` | FABRICATED |
| 6 | `position(long)`, closed | "@throws ClosedChannelException If this channel is closed" | returned `this` | FABRICATED |
| 7 | `truncate(long)`, `size < 0` | "@throws IllegalArgumentException If the new size is negative" | **truncated the file to empty**, returned `this` | FABRICATED |
| 8 | `truncate(long)`, closed | "@throws ClosedChannelException If this channel is closed" | returned `this` | FABRICATED |
| 9 | `transferTo`, `position < 0` or `count < 0` | "@throws IllegalArgumentException If the preconditions on the parameters do not hold"; both "must be non-negative" | `0` | FABRICATED |
| 10 | `transferTo`, closed | "@throws ClosedChannelException If either this channel or the target channel is closed" | `0` | FABRICATED |
| 11 | `transferTo`, source `pread` failed | "@throws IOException If some other I/O error occurs" | `unwrap_or(0)` → loop breaks → short transfer | FABRICATED |
| 12 | `transferTo`, `target.write` threw | propagates the target's exception | **counted the whole chunk as transferred** and discarded the exception | FABRICATED |
| 13 | `transferFrom`, `position < 0` or `count < 0` | as row 9 | `0`; and `position.max(0)` wrote the source over the front of the destination | FABRICATED |
| 14 | `transferFrom`, closed | as row 10 | `0` | FABRICATED |
| 15 | `transferFrom`, destination `pwrite` failed | "@throws IOException If some other I/O error occurs" | `unwrap_or(0)`, loop kept consuming the source → silent partial copy | FABRICATED |
| 16 | `transferFrom`, `src.read` threw | propagates | treated as end-of-source | FABRICATED |
| 17 | `force(boolean)`, closed | "@throws ClosedChannelException If this channel is closed" | returned normally | FABRICATED |
| 18 | `read`/`write`, null `ByteBuffer` | `NullPointerException` | `-1` (read) / `0` (write) | FABRICATED |
| 19 | `position()`, `size()`, `write(ByteBuffer)`, closed | `ClosedChannelException` | bare `IOException("Channel closed")` | FABRICATED (type only) |

Row 19 is the mildest and worth stating separately: the failure *was* reported,
with the wrong type. `catch (ClosedChannelException)` — the reopen/retry idiom —
does not match a supertype instance, so a recovery branch keyed on it never ran.
`ClosedChannelException extends IOException`, so tightening the type cannot
break a handler that was catching the old one.

### 3.2 FABRICATED — `java.io.BufferedWriter` (DEFAULT)

Registered on the **real** `java/io/BufferedWriter`, so in real-JDK mode these
shadow *every* `BufferedWriter` in the process, not just the fd-backed one. In
the default build `bw_synthetic_fd` is compiled to always return `None`, which
makes the delegating arm the live one and `out == null` the only way to reach
the fallback.

| # | site | JDK 25 says | CratonVM answered | verdict |
|---|---|---|---|---|
| 20 | `write(String,int,int)` delegate | `Writer.write`: "@throws IOException If an I/O error occurs" | `let _ = invoke_virtual(out, "write", …)` — delegate's exception discarded | FABRICATED |
| 21 | `write(int)` delegate | same | same | FABRICATED |
| 22 | `write(char[],int,int)` delegate | same | same | FABRICATED |
| 23 | `newLine()` delegate | same | same | FABRICATED |
| 24 | `flush()` delegate | `Writer.flush`: "@throws IOException If an I/O error occurs" | same | FABRICATED |
| 25 | `close()` delegate | `close()` is `try (Writer w = out) { flushBuffer(); }` — the flush propagates | both `flush` and `close` discarded | FABRICATED |
| 26 | write/`newLine`/`flush` after close | `ensureOpen()`: "if (out == null) throw new IOException("Stream closed")" | `Ok(None)` — **characters silently discarded** | FABRICATED |
| 27 | `write(String,int,int)` bounds | "@throws IndexOutOfBoundsException If off is negative, or off + len is greater than the length of the given string" | `.min(text.chars().count())` — silent truncation | FABRICATED |
| 28 | `write(char[],int,int)` bounds | "@throws IndexOutOfBoundsException If off is negative, or len is negative, or off + len is negative or greater than the length of the given array" (body: `Objects.checkFromIndexSize`) | `.min(cap)` — silent truncation | FABRICATED |
| 29 | fd-path writes (FEATURE) | as row 20 | `let _ = fd_table().write_string(..)` | FABRICATED |

Row 27's units were wrong as well as its bound: `off`/`len` are `String.length()`
indices — UTF-16 code units, what `s.getChars(b, b + d, …)` counts — and the
code measured in code *points*. Corrected while the check was being written,
because a bound checked in one unit and a slice taken in another is how this
defect grows back.

### 3.3 FABRICATED — `native-io/src/lib.rs`

| # | site | tier | JDK 25 says | CratonVM answered | verdict |
|---|---|---|---|---|---|
| 30 | `Buffer.position(int)` | FEATURE | "if (newPosition > limit \| newPosition < 0) throw createPositionException(newPosition)"; "@throws IllegalArgumentException If the preconditions on newPosition do not hold" | `clamp(0, lim)`, returned `this` | FABRICATED |
| 31 | `Buffer.limit(int)` | FEATURE | "if (newLimit > capacity \| newLimit < 0) throw createLimitException(newLimit)" | `clamp(0, cap)`, returned `this` | FABRICATED |
| 32 | `OutputStreamWriter.write(String,int,int)` | FEATURE | `Writer`'s unweakened bounds contract (`str.getChars(off, (off + len), cbuf, 0)`) | clamped both ends; also indexed a Rust `String` by BYTE where Java counts UTF-16 units, so `&text[off..end]` could panic | FABRICATED |
| 33 | `BufferedWriter.write(String,int,int)` | FEATURE | the weakened contract of §3.2 row 27 | same clamp | FABRICATED |
| 34 | `OutputStreamWriter.close()` | FEATURE | "Closes the stream, flushing it first … @throws IOException" | `let _ = flush(fd)` | FABRICATED |
| 35 | `BufferedWriter.close()` | FEATURE | same | same | FABRICATED |
| 36 | `FileWriter.write(String)` | FLAG | "@throws IOException If an I/O error occurs" | `let _ = write_string(..)` | FABRICATED |
| 37 | `FileWriter.write(String,int,int)` | FLAG | as row 32 (`FileWriter` does not redeclare it) | clamped both ends *and* discarded the write error | FABRICATED |
| 38 | `StringReader.skip(long)`, `n < 0` | **DEFAULT** | "Negative values of n cause the stream to skip backwards. Negative return values indicate a skip backwards." | `n.clamp(0, remaining)` → `0` | FABRICATED |
| 39 | `RandomAccessFile.read()` | FLAG | `-1` means "the end of the file has been reached"; "@throws IOException if an I/O error occurs" | `Err(_) => -1` | FABRICATED |
| 40 | `RandomAccessFile.read(byte[],int,int)` | FLAG | same | `Err(_) => -1` | FABRICATED |
| 41 | `RandomAccessFile.seek(long)` | FLAG | "@throws IOException if pos is less than 0 or if an I/O error occurs" | `pos.max(0)`, and `let _ =` on the seek itself | FABRICATED |
| 42 | `RandomAccessFile.write(int)` | FLAG | "@throws IOException if an I/O error occurs" | `let _ =` | FABRICATED |
| 43 | `RandomAccessFile.write(byte[],int,int)` | FLAG | same | `let _ =` | FABRICATED |
| 44 | `RandomAccessFile.writeInt(int)` | FLAG | `DataOutput`: "@throws IOException if an I/O error occurs" | `let _ =` | FABRICATED |
| 45 | `RandomAccessFile.writeLong(long)` | FLAG | same | `let _ =` | FABRICATED |
| 46 | `AsynchronousFileChannel.truncate(long)`, `size < 0` | **DEFAULT** | "@throws IllegalArgumentException If the new size is negative" | `(*v).max(0)` → truncated to empty | FABRICATED |

Row 41 is worth pausing on because it is the counter-example to unifying
things: `RandomAccessFile.seek(-1)` is an **`IOException`** and
`FileChannel.position(-1)` is an **`IllegalArgumentException`**, for the same
input on the same file. The two classes genuinely differ, and a shared helper
would have had to pick one and be wrong half the time.

Row 46's fix had to go *after* the existing closed and `NonWritableChannel`
refusals, not before: the JDK checks those first, and a caller distinguishing
the three by type would otherwise see the wrong one.

### 3.4 FABRICATED — synthetic `RandomAccessFile` in `nio_file.rs` (FLAG)

`register_phase57_random_access_file`, skipped unless `CRATONVM_SYNTHETIC_RAF=1`.

| # | site | JDK 25 says | CratonVM answered | verdict |
|---|---|---|---|---|
| 47 | `read()`, `read([B)`, `read([BII)` — I/O error | `-1` is EOF; "@throws IOException If the first byte cannot be read for any reason other than end of file" | `Err(_) => -1` (3 sites) | FABRICATED |
| 48 | `seek(long)`, `pos < 0` | "@throws IOException if pos is less than 0" | `pos.max(0)` | FABRICATED |
| 49 | `setLength(long)`, negative | "@throws IOException If an I/O error occurs" | `*v as u64` wrapped a negative to ~16 EiB and asked the host to grow the file to it | FABRICATED |
| 50–58 | `writeInt`, `writeLong`, `writeShort`, `writeChar`, `writeByte`, `writeBoolean`, `writeFloat`, `writeDouble`, `writeBytes`, `writeChars` | `DataOutput`: "@throws IOException if an I/O error occurs" | `let _ =` on every one (11 call sites) | FABRICATED |
| 59 | `writeUTF(String)` | "If this number is larger than 65535, then a UTFDataFormatException is thrown" | `.min(65535)` — silently truncated, cutting at a byte index so the tail could land mid-sequence | FABRICATED |

The tell that rows 50–58 were accident rather than policy: the `write([B)V`
sibling a hundred lines above them in the same registration block **already**
propagated. A family where one member reports and eleven do not is not a design.

### 3.5 SPEC-DEGRADATION — adjudicated as CORRECT, left alone

This list matters as much as the fixes: it is what distinguishes a sweep from a
mass edit. Each row is a hit one of the two instruments produced and which the
oracle says to leave exactly as it is.

| site | the sentence that licenses it |
|---|---|
| `File.delete()` returning `false` | "@return true if and only if the file or directory is successfully deleted; false otherwise". The javadoc goes further and *names* the alternative: "Note that the java.nio.file.Files class defines the delete method to throw an IOException when a file cannot be deleted." Converting `File.delete` to a throw is a divergence, not a fix. |
| `File.mkdir()`, `renameTo()`, `setReadOnly()`, `setLastModified()` returning `false` | same boolean-return contract |
| `File.lastModified()` returning `0L` for a missing file | "@return … or 0L if the file does not exist or if an I/O error occurs" |
| `File.length()` returning `0L` | same shape |
| `BufferedWriter.write(String,int,int)` writing nothing for negative `len` | "@implSpec … the implementation in this class does not throw such an exception in these cases but instead simply writes no characters" — **preserved deliberately**; the new helper has this branch and its sibling does not |
| `RandomAccessFile.skipBytes(int)` returning `0` for `n <= 0`, and clamping to the file length | "If n is negative, no bytes are skipped"; the JDK body itself computes `if (pos + n > len) newpos = len` |
| `StringReader.skip` returning `0` at end of string, even for a negative `n` | "If the entire string has been read or skipped, then this method has no effect and always returns 0" — the guard is kept in front of the new backwards-skip |
| `Files.deleteIfExists` returning `false` for an absent file | that is the method's entire reason to exist; the code already restricts the swallow to `ErrorKind::NotFound` and reports everything else |
| `Writer.close()` / `FileChannel.close()` being a no-op when already closed | "Closing a previously closed stream has no effect"; `FileChannel.close` likewise. The *flush* inside `close` is what now propagates — the release of the descriptor stays unconditional and unchecked, so an error cannot strand a handle. |
| `p57_delete_path`'s `let _ = p57_delete_path_checked(path)` | a deliberate best-effort wrapper that exists *because* the checked sibling next to it is the one callers use; the pair is documented and was the subject of an earlier fix |
| `Files.delete` / `deleteIfExists` / `copy` / `move` typed-exception paths | already swept — `p57_no_such_file`, `p57_access_denied`, `p57_directory_not_empty`, `p57_file_already_exists` are exactly this species' fix, applied before this lane existed. Re-verified, not re-touched. |
| `FileChannel.transferTo`/`transferFrom` returning a **short** count | "@return The number of bytes, possibly zero, that were actually transferred" — a short transfer is legal. Only the paths that produced one by *discarding an error* were changed. |
| `FileChannel.force` on a non-file-backed fd (pipe/socket) doing nothing | there is nothing to sync; the existing comment already scopes it, and it is now guarded by the closed check rather than sharing one `if` with it |

### 3.6 UNMEASURED — recorded, not fixed

| site | why it cannot be settled from source |
|---|---|
| `FileChannel` natives lack the real-instance guard that `close()`/`isOpen()` carry | `close`/`isOpen` are declared above `FileChannel` and are reachable on a real `sun.nio.ch.FileChannelImpl` through a `FileChannel`-typed call site, which is why they sniff the class name. The rest are `abstract` on `FileChannel` and should always resolve to `FileChannelImpl`'s own override — *should*, on this VM's dispatch. If they do not, rows 1–2 turn a silent `-1` into a thrown `ClosedChannelException` on a healthy real channel. Loud is the right side to fail on, but this needs one run to confirm. **Check this first when the branch is built.** |
| `RandomAccessFile.writeUTF` writes plain UTF-8, not modified UTF-8 | NUL and supplementary characters encode differently. `readUTF` beside it is symmetric, so a CratonVM round trip works and only cross-VM/on-disk interop diverges. Out of species; a separate defect. |
| `DatagramChannel.setSoTimeout(int)`'s `.max(0)` (`native-io`, DEFAULT) | not a method `java.nio.channels.DatagramChannel` declares, so there is no javadoc to quote. It belongs to the net lane's surface, not this one. Left untouched. |
| `WatchService.poll(long, TimeUnit)` timeout `.max(0)` (`native-io`) | the javadoc specifies no exception for a negative timeout, and "do not wait" is a defensible reading. No sentence either way. |
| `Files.isSameFile` approximating with `Path.equals` | a documented approximation already carrying its own comment, not a swallowed failure |

---

## 4. What a caller sees change

One line per fix, from the caller's side. A fix whose observable cannot be
stated is one that is not understood.

| fix | observable |
|---|---|
| FileChannel read-after-close (rows 1–2) | `while ((n = ch.read(buf)) != -1)` over a channel closed by another thread now **throws `ClosedChannelException`** instead of exiting cleanly; the file it was copying no longer comes out silently truncated |
| `truncate(-1)` (row 7) | the file **keeps its contents** and `IllegalArgumentException` is thrown, instead of the file being emptied |
| `write(buf, -1)` (row 4) | the **first bytes of the file are no longer overwritten**; `IllegalArgumentException` |
| `position(-1)`, `read(buf, -1)`, `transferTo/From(-1, …)` (rows 3, 5, 9, 13) | `IllegalArgumentException` naming the offset, instead of silently operating at offset 0 |
| closed-channel `position`/`truncate`/`transfer*`/`force` (rows 6, 8, 10, 14, 17) | `ClosedChannelException`; in particular `force()` no longer tells a caller its data is durable when the channel is gone |
| `transferTo` when the target throws (row 12) | the target's own `IOException` reaches the caller, instead of a return value equal to the bytes requested |
| `transfer*` on an I/O error (rows 11, 15, 16) | `IOException` instead of a short count indistinguishable from a legal short transfer |
| type-only rows (19) | `catch (ClosedChannelException)` reopen branches start matching |
| BufferedWriter delegation (rows 20–25) | a full disk under `try (BufferedWriter w = …) { … }` now **throws on the write or on the close** instead of exiting cleanly with a short file |
| BufferedWriter after close (row 26) | `w.write(x)` after `w.close()` throws `IOException("Stream closed")` instead of accepting and discarding `x` |
| BufferedWriter bounds (rows 27–28) | `w.write("ab", 0, 5)` throws `StringIndexOutOfBoundsException` instead of writing `"ab"`; `w.write(cbuf, -1, 4)` throws instead of writing nothing. Non-ASCII text is windowed by UTF-16 index, so the characters written match the ones asked for |
| `Buffer.position/limit` (rows 30–31) | `buf.position(limit + 1)` throws `IllegalArgumentException` instead of returning a buffer parked at `limit`, which was read as a short read *from the channel* |
| Writer `close()` flush (rows 25, 34–35) | the last buffered characters failing to reach disk is now an exception out of `close()`; the descriptor is still released either way |
| `StringReader.skip(-n)` (row 38) | a lookahead parser that skips forward and backs up **actually rewinds**, and gets a negative return, instead of re-reading the same region |
| RAF reads (rows 39–40, 47) | an I/O error is an `IOException` rather than an end-of-file the loop stops on |
| RAF writes (rows 42–45, 50–58) | a failed record append throws instead of returning; the family can say "no" at all for the first time |
| `RAF.seek(-1)` / `setLength(-1)` (rows 41, 48, 49) | `IOException` instead of a seek to the start of the file, or a request to grow it to 16 exabytes |
| `writeUTF` over 64 KiB (row 59) | `UTFDataFormatException` instead of a truncated, possibly mid-sequence record that `readUTF` would read back as if whole |
| `AsynchronousFileChannel.truncate(-1)` (row 46) | the file keeps its contents; `IllegalArgumentException` |

---

## 5. Mechanism notes for whoever builds this

* Two new typed builders in `nio_file.rs`, both modelled on the existing
  `p57_file_already_exists` (real class + real `<init>`, `IOException` fallback
  if the image does not declare it): `p57_closed_channel` and
  `raf_utf_data_format`. `ClosedChannelException` carries no payload — the JDK
  class declares no fields and its constructor is the no-arg one — so unlike
  `p57_no_such_file` there is no `file` slot to populate.
* `bw_stream_closed()` reproduces `BufferedWriter.ensureOpen()`'s exception
  verbatim, message included.
* Error returns inside `transferTo`/`transferFrom` unpin the outer root
  (`unpin_native_roots` releases "from `base` onward", so releasing the
  target/src pin also releases the per-chunk pin) before returning. The
  pre-existing `try_alloc_concurrent_synthetic(…)?` in those loops does **not**
  do this; it is untouched and is not this species.
* `native-io` gained `writer_string_region` (strict) and
  `buffered_writer_string_region` (`BufferedWriter`'s weakened variant), sharing
  one `writer_region_out_of_bounds`. They raise
  `StringIndexOutOfBoundsException`, not the bare supertype, because the real
  path is `String.getChars` → `checkBoundsBeginEnd`, and
  `catch (StringIndexOutOfBoundsException)` does not match a supertype instance
  while `catch (IndexOutOfBoundsException)` matches both.
* **`cfg` arms.** No `#[cfg(unix)]`, `#[cfg(windows)]` or other platform arm was
  edited anywhere in this change. Everything touched is platform-independent
  Rust. Three `#[cfg(feature = "synthetic-jdk")]`-gated *blocks* were edited
  (§3.3 rows 30–35), which the driving host can compile with
  `--features synthetic-jdk` but which this lane did not compile at all.
* Nothing was built, checked, tested, clippy'd or formatted, and the VM was not
  run. `cargo fmt` in particular was not run — the tree is deliberately not
  fmt-clean.

---

## 6. Out-of-file patch (not applied)

One item only. It is a *documentation* change to another lane's file, so it is
recorded rather than applied.

**Add to `W2-7-fabricated-success-where-the-spec-mandates-failure.md`'s
inventory table** (that record is owned by another lane and was read, not
edited). Rows 6–8 continue its numbering:

```
| 6 | `FileChannel.read(ByteBuffer)` on a CLOSED channel returns `-1` | `ClosedChannelException` | `native-builtins/src/phases_late/nio_file.rs::register_phase57_file_channel` | FIXED (W7-8) |
| 7 | `FileChannel.truncate(-1)` empties the file; `write(buf, -1)` overwrites its front | `IllegalArgumentException` | same | FIXED (W7-8) |
| 8 | `BufferedWriter` discards its delegate's `IOException` and accepts writes after `close()` | propagate; `IOException("Stream closed")` | `nio_file.rs`, the `java/io/BufferedWriter` block | FIXED (W7-8) |
```

and to its **"How to find the next one"** section, the two filters this lane
used, which are more discriminating than the three spellings listed there:

```
A `min`/`clamp`/`saturating_*` on an argument the spec range-checks is the same
defect as a swallowed `Err`. Then, for every error-path hit: is the value it
answers on failure distinguishable, BY THE CALLER, from a real success? `-1`
from a read, `0` from a transfer, `false` from `File.delete`, and a `void`
write that returns are four different answers to that question, and only three
of them are defects.
```
