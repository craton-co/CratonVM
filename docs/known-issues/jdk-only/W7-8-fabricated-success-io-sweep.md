# W7-8 — fabricated success across the `java.io` / `java.nio.file` native surface

Status: **swept and fixed in source, NOT built and NOT run.** No `cargo` command
of any kind was executed for this record and no CratonVM binary exists carrying
it. Every claim below is a claim about *source* and about the *JDK 25 oracle*,
never about observed behaviour.

> ## §10 — B8, 2026-08-12: a NEW fabricated-success row, found by running
>
> This sweep is about `java.io` natives that report success and do nothing. It
> has a member it never found, and it is on the most ordinary call in the
> package. In `--synthetic-jdk`
> (`/c/craton/synjdk-target/release/cratonvm.exe`), an `OutputStreamWriter`
> accepts a write, accepts an explicit `flush()`, accepts `close()`, throws
> nothing, and leaves a **zero-byte file**:
>
> ```
>                     HotSpot   --synthetic-jdk   --jdk-only
> osw_flushed_len          5           0              5
> osw_closeonly_len        5           0              5
> raw_fos_len              5           5              5      <-- FileOutputStream is fine
> bos_len                  5           5              5      <-- BufferedOutputStream is fine
> ```
>
> Three of five `Writer` write overloads drop their bytes silently
> (`write(String)`, `write(char[],int,int)`, `write(int)`); `write(String,int,int)`
> works; `append(CharSequence)` throws `NoSuchMethodError`.
>
> **The shape is this record's §7 shape, one level up.** §7's lesson was that a
> fix can land in a registrar nothing calls. Here both registrars are called,
> and that is the defect: `java/io/OutputStreamWriter` is registered from
> `native-io/src/lib.rs:6656-6686` **and** `native-builtins/src/lib.rs:9478-9525`,
> `register()` is last-write-wins, and the two disagree about slot 0.
> `native_osw_init` (native-io) wins `<init>` and stores an **`Int` fd** there;
> `osw_wrapped_output` (native-builtins, `logging_shims.rs:12`), which the three
> overloads only *it* registers must call, requires a `Value::Object` in slot 0
> and answers `None` otherwise. `write_bytes_from_output_stream_writer` then
> ends with a bare `if let Some(out) = … { … }` and **no `else`** — so `None`
> is `Ok(None)`, and the failure is laundered into success.
>
> That trailing `if let` with no `else` is the exact idiom this record exists to
> sweep for, so it belongs in the §2 tier table. Full transcript, per-overload
> breakdown and the slot-convention analysis are in
> `W7-50-synthetic-jdk-strict-six.md` §12; the NOMINATION is filed there rather
> than duplicated here.
>
> **Scope:** `--jdk-only` and `--real-jdk` are green on this probe — the block
> is guarded by `drops_real_layout_synthetic()`, and that guard is doing its
> job. This is a `--synthetic-jdk`-only row. **Scheduling: none** — no suite
> arm runs `--synthetic-jdk` at any `SUITE=` value.

> **2026-08-12 — read §7 before §3.1, and §8 before either.** The §3.6 residuals
> were re-opened and four of five settled from source. The one that mattered most
> was not on that list: **§3.1's entire fix set landed in a registrar that neither
> shipping mode ever calls.** `register_phase57_file_channel` is reachable only
> from `register_synthetic_overrides`, while `register_phase57_nio_file` — which
> `vm_init` calls directly in both shipping arms — carried its own unhardened
> copies of seven of the same triples. Rows 1, 2 and 19 were therefore LIVE in
> `--real-jdk` and `--jdk-only`. §7.1 has the chain; §6 item 2 had the decision to
> take.
>
> **§8 (same day, later): the re-aim is APPLIED, and so are §6 items 3–6.** The
> seven shared triples now point at one body each, registered by BOTH registrars,
> so rows 1, 2 and 19 are live-fixed in all three configurations. Still not built
> and still not run.
>
> **§9 (same day, later still) is the largest thing in this record and it is NOT
> fixed.** The §2 tier table names `register_async_file_channel` as the owner of
> the `AsynchronousFileChannel` surface. It owns eleven triples; the class has
> two more, and a registrar in a **third** file this record never opened serves
> both of them. `force(Z)V` is a **silent no-op on every channel this VM
> produces** — a durability barrier that returns before it reads its own first
> argument — and `lock()` mints a real `java/util/concurrent/FutureTask` at the
> wrong width so `get()` **parks forever**. §7.1's lesson recurs with the
> polarity reversed: it is not enough to fix the winning registrar; you have to
> ask what the winner does not register. §9.6 carries the patches, out-of-file.

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

> **Reachability correction, 2026-08-12 (§7.1), and its repair (§8).** "DEFAULT"
> in this heading was wrong for the seven triples both registrars serve — rows
> 1, 2, 5, 6, 17 (partly) and 19 — because `register_phase57_file_channel` is
> reached only from `register_synthetic_overrides`. Those seven are now single
> bodies (`p57_fc_size`, `p57_fc_position`, `p57_fc_position_set`, `p57_fc_read`,
> `p57_fc_write`, `p57_fc_close`, `p57_fc_is_open`) registered by both
> registrars, so the heading is true again for them. Rows 3, 4, 7–16 and 18
> concern triples only this registrar registers and are **synthetic-JDK only** —
> in a shipping mode real `FileChannelImpl` bytecode serves them. That is not a
> gap to close by registering more triples on the shipping path: doing so would
> put a native in front of methods nothing currently intercepts.

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

**Every row of this table was re-opened on 2026-08-12 and four of the five are
now settled. Read §7 before working from any of them.** The table is kept as
written so the corrections can be read against it.

| site | why it cannot be settled from source |
|---|---|
| `FileChannel` natives lack the real-instance guard that `close()`/`isOpen()` carry | `close`/`isOpen` are declared above `FileChannel` and are reachable on a real `sun.nio.ch.FileChannelImpl` through a `FileChannel`-typed call site, which is why they sniff the class name. The rest are `abstract` on `FileChannel` and should always resolve to `FileChannelImpl`'s own override — *should*, on this VM's dispatch. If they do not, rows 1–2 turn a silent `-1` into a thrown `ClosedChannelException` on a healthy real channel. Loud is the right side to fail on, but this needs one run to confirm. **Check this first when the branch is built.** — **SUPERSEDED, §7.1: the guard exists, and the real finding underneath it is worse.** |
| `RandomAccessFile.writeUTF` writes plain UTF-8, not modified UTF-8 | NUL and supplementary characters encode differently. `readUTF` beside it is symmetric, so a CratonVM round trip works and only cross-VM/on-disk interop diverges. Out of species; a separate defect. — **CONFIRMED and half-fixed, §7.2. "readUTF beside it is symmetric" is false in `native-io`.** |
| `DatagramChannel.setSoTimeout(int)`'s `.max(0)` (`native-io`, DEFAULT) | not a method `java.nio.channels.DatagramChannel` declares, so there is no javadoc to quote. It belongs to the net lane's surface, not this one. Left untouched. — **WRONG FRAMING, FIXED, §7.3.** |
| `WatchService.poll(long, TimeUnit)` timeout `.max(0)` (`native-io`) | the javadoc specifies no exception for a negative timeout, and "do not wait" is a defensible reading. No sentence either way. — **There IS a sentence, §7.4. The `.max(0)` was right; a sibling in the same crate was not.** |
| `Files.isSameFile` approximating with `Path.equals` | a documented approximation already carrying its own comment, not a swallowed failure — **the comment misstates the JDK, §7.5.** |

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

**Item 1 was the only one when this record was written; §7 added five more, all
in `native-builtins/src/phases_late/nio_file.rs`, and they are listed after it.**
Item 1 is a *documentation* change to another lane's file, so it is recorded
rather than applied.

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

### Items 2–6 — all in `native-builtins/src/phases_late/nio_file.rs`

Added 2026-08-12 by the pass recorded in §7. Anchor each on the quoted text, not
the line number.

> **ALL FIVE APPLIED, 2026-08-12 — see §8.** Item 2 was applied by *sharing*
> rather than by moving or delegating, and item 5's call site grew a screen the
> patch text did not have. The text below is kept as written so the differences
> can be read against it.

**2. Re-aim §3.1 at the registrar that actually ships (§7.1).** This is the
biggest of the six and it is not a patch so much as a decision. `close()`,
`isOpen()`, `size()`, `position()`, `position(long)`, `read(ByteBuffer)` and
`write(ByteBuffer)` are registered TWICE, and in `--real-jdk` / `--jdk-only`
only `register_phase57_nio_file`'s copies exist. Either move §3.1's hardened
bodies into that registrar, or have it delegate. Do **not** simply delete the
duplicate registrations: in `--synthetic-jdk` mode the file-channel registrar is
the one that wins, so deleting either side changes a different mode than the one
being fixed. Prove whichever way with a `--dump-native-registry` diff.

**3. The two contradictory ordering comments (§7.1).** `nio_file.rs:6002-6007`
says `register_phase57_nio_file` wins *"in every build and every mode"* because
*"even in the synthetic arm `vm_init.rs` calls THIS registrar again
afterwards"*; `:15322-15334` says its own copy is *"the LOSING copy of this
triple"*. In the synthetic arm the second is the winner
(`register_phase57_natives` calls `register_phase57_nio_file` at line 21 and
`register_phase57_file_channel` at line 36, and `vm_init.rs:1837-1905` calls
neither again). Correct both comments together or they will re-diverge.

**4. `RandomAccessFile.writeUTF` / `readUTF` — converge on one codec (§7.2).**
`native-io` now exports the pair, so this is a two-line change plus the length
check that already exists:

```
-        let bytes = s.as_bytes();
+        let bytes = cratonvm_native_io::encode_modified_utf8(&s);
```

and in `readUTF`:

```
-        let s = String::from_utf8_lossy(&str_buf).to_string();
+        let s = cratonvm_native_io::decode_modified_utf8(&str_buf)
+            .map_err(|e| raf_utf_data_format(ctx, &e))?;
```

`raf_utf_data_format` is already in that file (`nio_file.rs:9022`) and is the
right type: `DataInput.readUTF` specifies `UTFDataFormatException` for bytes that
are not a valid modified UTF-8 encoding, which is exactly what
`from_utf8_lossy` was silently replacing with `U+FFFD`. Note the existing
`if bytes.len() > 65535` check must stay where it is and must measure the
ENCODED buffer — it already does.

While there: `writeChars` at `:12296` does `for ch in s.chars()` and writes
`ch as u16`, which truncates every supplementary character to one wrong code
unit. `DataOutput.writeChars` writes UTF-16 code units, i.e. `s.encode_utf16()`.
Same wire format, same file, one line.

**5. `Files.isSameFile` — use the identity, not the spelling (§7.5).** Replace
the `Path.equals` body at `nio_file.rs:1267-1286` with `p57_read_path` on both
arguments and

```rust
match cratonvm_native_io::file_channel::paths_name_the_same_file(
    std::path::Path::new(&p1),
    std::path::Path::new(&p2),
) {
    Ok(same) => Ok(Some(Value::Int(i32::from(same)))),
    Err(e) => Err(p57_io_error(&e)),
}
```

and correct the comment above it, which currently states as fact that the
default provider's check *is* path equality. It is the fast path only. Keep the
VFS/jar-encoded case in mind: `paths_name_the_same_file` touches the host
filesystem, so a `vfs_decode`-able path must keep taking the equality answer —
that screen is not in the helper and must be at this call site.

**6. The option-list scan (§7.6).** Three sites, one species, and landing them
is what unlocks two more assertions in `regression-suite/src/RNioNoFollow.java`
— the vector that currently asserts nothing about `NOFOLLOW_LINKS` on Windows:

* `Files.copy(InputStream, Path, CopyOption[])` (`:19572`) — refuse every option
  but `REPLACE_EXISTING` with `UnsupportedOperationException`. Synthetic-only, so
  this one changes no shipping measurement; it is here so the three do not
  diverge.
* `fsp_new_input_stream` (`:8624`) — refuse `APPEND` and `WRITE` with
  `UnsupportedOperationException`. **Shipping.**
* `fsp_new_output_stream` — refuse `READ` with `IllegalArgumentException`
  (note: a *different* type from its sibling, and the JDK means it).
  **Shipping.**

`fsp_scan_open_options` already walks the array for `nofollow`; these are
additional verdicts from the same walk, not a second one.

---

## 7. §3.6 re-opened, 2026-08-12 — four of five settled

Same standing as everything above: **nothing was built, checked, tested or run.**
The JDK 25 oracle is the same one, and it is on this host —
`C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot\lib\src.zip`. Every quote below
is from a file extracted out of it, with the line number in that file.

The five rows were re-opened together because they had one thing in common: each
was recorded as *unsettleable from source*, and four of them turned out to be
settleable from source by asking a different question than the one the row asked.

### 7.1 The real-instance guard EXISTS — and the finding under it is that this record fixed the losing registrar

**The row's own question is closed.** The class-name screen it says is missing
was moved INSIDE the accessors by the wave that followed this record.
`native-api/src/synthetic_file_channel.rs` now owns the private slot map, and
`private_base` opens with

```rust
let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
if class_name.as_deref() != Some(CLASS) {
    return None;
}
```

so `fd_value` answers `Value::Object(None)` and `set_fd_value` drops the write
for **every** receiver that is not literally `java/nio/channels/FileChannel`.
The module says why in its own words — *"the screen moved INSIDE the accessors …
Every call site inherits it, and none can forget it"*. So the "if they do not,
rows 1–2 turn a silent `-1` into a thrown `ClosedChannelException` on a healthy
real channel" hazard is gone in the other direction: on a real
`sun.nio.ch.FileChannelImpl` the accessors are guaranteed to answer "not ours",
and the surrounding bodies take their not-open arm with certainty rather than by
luck.

**But the bodies that take that arm are not this record's.** Reachability, from
the tree:

* `register_phase57_file_channel` — where **every fix in §3.1 landed** — is
  called from exactly one place, `register_phase57_natives`
  (`native-builtins/src/phases_late/nio_file.rs:36`), which is called from
  exactly one place, `native-builtins/src/lib.rs:23938`, which is inside
  `register_synthetic_overrides`. `vm/src/vm/vm_init.rs` reaches
  `register_synthetic_overrides` only through `register_builtins`, and calls
  that only inside `#[cfg(feature = "synthetic-jdk")] { if config.use_synthetic_jdk { … } }`
  (`vm_init.rs:1835-1839`).
* `register_phase57_nio_file` is called **directly** by `vm_init.rs` in the two
  arms that ship — `:2217` (feature-enabled binary running real-JDK) and
  `:2763` (the default build) — and registers its own bodies for
  `size()J`, `position()J`, `position(J)Ljava/nio/channels/FileChannel;`,
  `close()V`, `isOpen()Z`, `write(Ljava/nio/ByteBuffer;)I` and
  `read(Ljava/nio/ByteBuffer;)I` (`nio_file.rs:5924-6104`).

So in **both shipping runtime modes**, seven FileChannel triples are served by
`register_phase57_nio_file`'s unhardened bodies and the hardened ones are never
registered at all. Concretely, rows 1, 2 and 19 of §3.1 are LIVE today:
`nio_file.rs:6075` still answers `-1` for a `read` it cannot service, and
`:6039` still raises a bare `IOException("Channel closed")` rather than
`ClosedChannelException`. Rows 3–18 concern six triples
(`truncate`, `read`/`write` with an explicit position, `transferTo`,
`transferFrom`, `force`) that **only** the synthetic registrar registers, so in
a shipping mode real `FileChannelImpl` bytecode runs and there is nothing to
fix there.

**A doc row this corrects.** `nio_file.rs:6002-6007` asserts that
`register_phase57_nio_file`'s `isOpen` is *"THE WINNING REGISTRATION for this
triple, in every build and every mode"* because *"even in the synthetic arm
`vm_init.rs` calls THIS registrar again afterwards"*. It does not: the synthetic
arm is `vm_init.rs:1837-1905` and contains no call to it, while inside
`register_phase57_natives` the order is `register_phase57_nio_file` at line 21
and `register_phase57_file_channel` at line 36 — so in **synthetic** mode the
`isOpen` at `:15334` that calls itself *"the LOSING copy"* is the one that wins.
The two comments contradict each other and both are half right, which is exactly
the shape §3 of the directory README warns about. Neither file is this lane's;
see [Out-of-file patch](#6-out-of-file-patch-not-applied).

**What this does NOT settle**, and no source read can: whether a native
registered on `java/nio/channels/FileChannel` intercepts a call whose receiver is
a real `sun.nio.ch.FileChannelImpl`. `close()V` clearly does — the body at
`nio_file.rs:5965` was written for that case and says so. `read(ByteBuffer)` is
*declared* abstract on `FileChannel` and *overridden* by `FileChannelImpl`, which
is a different dispatch shape. That is the one run this row still needs, and it
is now a much sharper question than "is there a guard".

### 7.2 `writeUTF` — confirmed, and "readUTF beside it is symmetric" is false

The row is right that `RandomAccessFile.writeUTF` writes plain UTF-8:
`nio_file.rs:12259` is `let bytes = s.as_bytes();` and the length prefix at
`:12273` is that buffer's length. Its neighbour `readUTF` at `:12112` is
`String::from_utf8_lossy`, so within that file the round trip does close.

The row's consolation — *"a CratonVM round trip works"* — does not survive
contact with the second implementation. `native-io/src/lib.rs` registers the
same class's `readUTF()Ljava/lang/String;` and, until this pass, bound it to
**`native_raf_read_line`**, with the word `simplified` for a comment. `readLine`
scans to the next `\n`/`\r`; `readUTF` reads a 2-byte big-endian length and then
exactly that many bytes. So that binding consumed the length prefix as text,
stopped at whichever payload byte happened to be `0x0A`, and left the file
position mid-record — corrupting every later read on the handle, not only its
own. `writeUTF` had **no** registration in that file at all, which is why no
round trip existed to fail.

The two encodings differ on exactly two inputs, and it is worth writing them
down because "UTF-8" is not one thing here: `U+0000` is `C0 80` in modified
UTF-8 and `00` in plain, and a supplementary character is a **six-byte surrogate
pair** in modified UTF-8 and one four-byte sequence in plain. Both the count in
the prefix and the payload are wrong, and `readUTF` cannot tell.

**Fixed in this pass, in `native-io` only.** `encode_modified_utf8` /
`decode_modified_utf8` (already correct, already unit-tested, already used by
`DataOutputStream.writeUTF`) are now `pub`, and `native-io` grew
`native_raf_read_utf` / `native_raf_write_utf` over them plus `raf_read_exact`
for the `readFully` half of the contract. Both are **FLAG** tier
(`CRATONVM_SYNTHETIC_RAF=1`), so this changes no shipping measurement; what it
changes is that the wire format now has one spelling per crate and the export
exists for the `nio_file.rs` twin to converge onto. That convergence is the
out-of-file patch below.

### 7.3 `DatagramChannel.setSoTimeout` — the row asked the wrong class. FIXED

*"Not a method `java.nio.channels.DatagramChannel` declares, so there is no
javadoc to quote"* is true and irrelevant. The comment sitting directly above
the registration already said which class to quote: this body is reachable only
as `channel.socket().setSoTimeout(..)`, i.e. from a caller using the
**DatagramSocket** surface. JDK 25 answers it twice:

* `java/net/DatagramSocket.java:687` — `@throws IllegalArgumentException if
  {@code timeout} is negative`
* `sun/nio/ch/DatagramSocketAdaptor.java:231-236`, which is what `socket()`
  actually returns:

  ```java
  public void setSoTimeout(int timeout) throws SocketException {
      if (isClosed()) throw new SocketException("Socket is closed");
      if (timeout < 0) throw new IllegalArgumentException("timeout < 0");
      this.timeout = timeout;
  }
  ```

`.max(0)` mapped `setSoTimeout(-1)` to `setSoTimeout(0)`, and the very next line
of the body reads `0` as **no timeout**. So the one input a caller uses to say
"bound this receive" was turned into "never bound it" — the sharpest possible
instance of this record's own instrument, because an unbounded receive is
indistinguishable from a healthy configuration until it hangs.

Fixed in `native-io/src/lib.rs`'s `register_datagram_channel`, which is
**DEFAULT** tier (`register_io_natives` is called in all three `vm_init` arms).
The message is HotSpot's. The `isClosed()` refusal that precedes it upstream was
deliberately NOT added: `dc_fd` answering `None` means "closed OR never bound",
so raising `SocketException` on it would refuse a channel the JDK accepts. The
consequence is stated rather than guessed — on a closed channel with a negative
timeout this now answers `IllegalArgumentException` where HotSpot answers
`SocketException`, which is one wrong exception type instead of a silent
success.

Covered by `regression-suite/src/RJdkNet.java::negativeSoTimeout`, which asserts
the refusal on all four surfaces (`DatagramChannel.socket()`, `DatagramSocket`,
`Socket`, `ServerSocket`), the acceptance of `0` and of a positive value, and
the closed-beats-negative ordering on the two that run real JDK bytecode. The
positive half is not decoration: without it a VM that threw on every timeout
would satisfy all four refusals.

### 7.4 `WatchService.poll` — there is a sentence, and the `.max(0)` was the side that had it right

The javadoc genuinely says nothing (`java/nio/file/WatchService.java:155-172`
names only `ClosedWatchServiceException` and `InterruptedException`). But the
implementation is the sentence: `sun.nio.fs.AbstractWatchService.poll(long,
TimeUnit)` hands the value to `LinkedBlockingDeque.poll(timeout, unit)`, whose
loop opens `if (nanos <= 0L) return null;`. A negative wait is a wait that has
already expired.

So `watch_timeout_millis`'s `if timeout <= 0 { return 0; }` — the `.max(0)` this
row flagged — is **correct and stays**. What the row did not look at is the
*other* WatchService surface in the same crate: `native-io/src/watch.rs`'s
`poll_with_timeout` classified `timeout_ns == i64::MIN || timeout_ns < 0` as
"block indefinitely", so on that surface every negative timeout was an
unbounded wait. Two surfaces in one crate disagreeing about the same argument,
with one of them turning a caller's expired deadline into a hang.

Fixed by splitting the classification into a pure `watch_wait_for(i64) ->
WatchWait`: `i64::MIN` (this file's own `take()` sentinel, written by
`take_blocking`) is the only value that means Forever; every other
non-positive value is Now. Asserted on the classifier rather than by timing,
deliberately — the pre-fix behaviour of the negative case is *never returns*, so
a test that told the two apart by waiting would hang on a red tree instead of
failing it, and any bound that avoided the hang would be a fixed wall-clock
bound. `wp3_8_take_still_blocks_after_the_negative_timeout_fix` is the guard
against fixing it backwards by collapsing the sentinel in with the rest, which
would silently turn every `WatchService.take()` in the process into a busy
`poll()`.

### 7.5 `Files.isSameFile` — the approximation is fine, the comment justifying it is not

*"A documented approximation already carrying its own comment"* is what the row
says. The comment is
`nio_file.rs:1262-1266`: *"The default provider's same-file check is path
equality (real-path resolution for symlinks omitted); approximate with
`Path.equals`."*

Path equality is the JDK's **fast path**, not its answer. Both default providers
return early on `file1.equals(obj2)` and otherwise read the identity of both
files — `st_dev`/`st_ino` on Unix, volume serial + file index via
`GetFileInformationByHandle` on Windows — and compare that. So the JDK answers
`true`, and CratonVM answers `false`, for every pair naming one file by two
spellings: a hard link, a symlink and its target, `dir/x` and `dir/sub/../x`, an
absolute and a relative path to the same file, and on Windows two spellings
differing only in case or in 8.3 shortening. `Files.isSameFile` is how a caller
asks *"am I about to copy this file onto itself"*, so a `false` there is the
answer that lets the destructive branch run — which puts it back inside this
record's species rather than outside it.

The identity half is landed in `native-io/src/file_channel.rs` as
`paths_name_the_same_file`, next to the fd-keyed `file_identity_triple` it
reuses the `GetFileInformationByHandle` binding from, with
`wp3_3_same_file_is_identity_not_path_equality` covering it (the `..` traversal
is the non-skippable row; the hard link is the one `canonicalize` cannot see and
is therefore what proves the identity read is doing the work). The **call site**
is another lane's file and is unchanged — see the out-of-file patch.

### 7.6 One row this pass added rather than closed

`Files.copy(InputStream, Path, CopyOption...)` in JDK 25 refuses **every** option
but `REPLACE_EXISTING` with `UnsupportedOperationException(opt + " not
supported")`. CratonVM's copy of it (`register_p71_files_bridge`,
`nio_file.rs:19572`) reads `REPLACE_EXISTING` and ignores the rest. That
registrar is reached only from `register_synthetic_overrides`, so **in both
shipping modes the real bytecode runs and the refusal is the JDK's own** — which
is why `regression-suite/src/RNioNoFollow.java` can now assert it.

That assertion matters out of proportion to its size, and the reason is
`RNioNoFollow`'s standing vacuity: `symlinkArms()` bails on Windows because
`Files.createSymbolicLink` needs a privilege, so **every arm that tests
`NOFOLLOW_LINKS` against a symlink has never executed on the primary platform**.
`Files.copy(in, target, NOFOLLOW_LINKS)` is a `NOFOLLOW_LINKS` assertion that
needs no link and no privilege, because it asks the other half of the same
question the defect was: *is the option list read at all*. The scanner that
produced this record's parent defect read `APPEND` and `CREATE_NEW` and nothing
else, so an unrecognised option was silently accepted — and that is precisely
what this refusal detects.

Two sibling refusals are the same species and are **not** asserted, because
they are served by shipping natives that do not implement them:

| call | JDK 25 | CratonVM |
|---|---|---|
| `Files.newInputStream(p, StandardOpenOption.WRITE)` | `UnsupportedOperationException` (`FileSystemProvider.newInputStream`: *"All OpenOption values except for APPEND and WRITE are allowed"*) | accepted — `fsp_new_input_stream` (`nio_file.rs:8624`) scans the option list for `nofollow` only |
| `Files.newOutputStream(p, StandardOpenOption.READ)` | `IllegalArgumentException("READ not allowed")` (`FileSystemProvider.newOutputStream`) | accepted — same shape in `fsp_new_output_stream` |

Both are in `register_phase57_nio_file`, i.e. live in both shipping modes.
Landing the option scan and promoting these two into `RNioNoFollow` is one
change and should be done as one; the vector text says so at the site.

---

## 8. §6 items 2–6 APPLIED, 2026-08-12

Same standing as everything above: **nothing was built, checked, tested or run.**
No `cargo` command of any kind was executed. Every claim here is a claim about
source and about the JDK 25 oracle.

### 8.1 Item 2 — the re-aim, and why it is neither a move nor a delegation

The patch text offered two shapes ("either move §3.1's hardened bodies into that
registrar, or have it delegate") and warned against a third (deleting a duplicate
registration). A fourth was taken, and the reason is what that warning is really
about:

> **Both registrars now register the SAME function pointer for each of the seven
> shared triples.** `p57_fc_size`, `p57_fc_position`, `p57_fc_position_set`,
> `p57_fc_read`, `p57_fc_write`, `p57_fc_close` and `p57_fc_is_open` are
> top-level `fn`s — `NativeCallback` is a plain `fn` pointer, so a named function
> registers as directly as a closure — and both `register_phase57_nio_file` and
> `register_phase57_file_channel` name them.

Moving the bodies would have left `--synthetic-jdk` running whatever the loser
registered. Delegating would have left one registrar authoritative and the other
a forwarding stub the next reader has to trace. Sharing makes **last-write-wins
stop being load-bearing for these seven triples in every mode** — the only
version of "it does not matter which registrar wins" that is actually true. It
also makes a future edit's failure mode benign: you cannot now change one mode's
FileChannel behaviour without changing the other's, which is exactly the property
whose absence produced this record's headline.

Neither registration was deleted, for the reason the patch text gives.

**What a `--dump-native-registry` diff should show.** The registry holds one slot
per `(class, method, descriptor)` and a duplicate updates it in place, so the
**row counts do not move in any of the three configurations**. Check that first:

| configuration | which registrar supplies the seven | rows before → after | callback |
|---|---|---|---|
| default build, `--real-jdk` | `register_phase57_nio_file` only (`vm_init.rs:2763`) | unchanged | now `p57_fc_*`; was seven distinct closures |
| default build, `--jdk-only` | same | unchanged | same |
| `--features synthetic-jdk`, `--synthetic-jdk` | both; `register_phase57_file_channel` runs last and wins | unchanged | `p57_fc_*` from **both**, so winner and loser are indistinguishable |

A dump printing only class/method/descriptor therefore shows **no diff at all**,
and that is the correct result — what changed is which code the slots point at,
not which slots exist. A "the fix did nothing" reading trips exactly here.

**The behavioural diff, per mode.** `Closed` means a synthetic
`java/nio/channels/FileChannel` whose `close()` has run (private slot holds `-1`).

| triple / condition | shipping modes, before | shipping modes, after | `--synthetic-jdk` |
|---|---|---|---|
| `size()J`, closed | `0` | `ClosedChannelException` | unchanged (already hardened) |
| `size()J`, I/O error | `unwrap_or(0)` | `IOException` | unchanged |
| `position()J`, closed | `0` | `ClosedChannelException` | unchanged |
| `position(J)`, negative | seek to 0, returns `this` | `IllegalArgumentException` | unchanged |
| `position(J)`, closed | returns `this` | `ClosedChannelException` | unchanged |
| `read(ByteBuffer)`, closed | **`-1`** (row 1) | `ClosedChannelException` | unchanged |
| `read(ByteBuffer)`, null buffer | `-1` | `NullPointerException` | unchanged |
| `write(ByteBuffer)`, closed | bare `IOException` (row 19) | `ClosedChannelException` | unchanged |
| `write(ByteBuffer)`, null buffer | `0` | `NullPointerException` | unchanged |
| `close()V`, `isOpen()Z` | already equivalent | unchanged | unchanged |

So **synthetic mode is behaviourally unchanged for all seven**, which is what the
patch text demanded, and the shipping modes gain §3.1's rows 1, 2, 5, 6 and 19.

### 8.2 The third fd state the shared bodies needed and §3.1's did not

§3.6's first row — *"if they do not, rows 1–2 turn a silent `-1` into a thrown
`ClosedChannelException` on a healthy real channel"* — is still unsettled from
source, and moving §3.1's bodies onto the shipping path is precisely what would
have cashed that risk. It is not cashed, and the mechanism was available all
along:

`synthetic_file_channel::fd_value` answers `Value::Object(None)`, **not an
`Int`**, for a receiver whose class is not literally
`java/nio/channels/FileChannel` — §7.1 quotes the screen. The fd slot therefore
has three states, not two, and the shared bodies match all three:

* `Int(v), v >= 0` — open synthetic channel.
* `Int(_)` negative — **closed**, and only a synthetic receiver can be, since
  `-1` is what this file's own `close()` writes. Takes the hardened arm.
* anything else — **foreign**. Keeps each site's pre-W7-8 answer *verbatim*
  (`0` for `size`/`position`, `this` for `position(J)`, `-1` for `read`).

§3.1's bodies collapsed the second and third with `.as_int().unwrap_or(-1)`,
which is safe where they ran (synthetic mode has no real `FileChannelImpl`) and
would not have been on the shipping path. `write` is the one site where `Foreign`
shares the hardened arm, and that is sound rather than inconsistent: it already
raised a bare `IOException` for a foreign receiver, so tightening to
`ClosedChannelException` — a subclass — cannot turn a success into a failure.

**What this still does not settle**, and no source read can: whether a native on
`FileChannel` intercepts a receiver that is a real `sun.nio.ch.FileChannelImpl`.
`close()V` demonstrably does, and has carried a class-name screen for it since
the H2 file-lock bug. For `read`/`write`/`size`/`position` there is a strong
*behavioural* argument that it does not — the shipping bodies answered `-1`/`0`
for a foreign receiver, so interception would mean every real `FileChannel.read`
in the process reports end-of-file and every `size()` reports 0, which H2's
MVStore alone would not survive — but that is an inference from the tree working,
not a measurement, and it is the shape this campaign calls a reach-versus-defect
confusion. The `Foreign` arm means the answer no longer changes anything.

### 8.3 Item 3 — the two contradictory ordering comments

Both rewritten together, and both now say what is true **per mode** rather than
asserting a single winner:

* the `isOpen` comment claiming *"THE WINNING REGISTRATION for this triple, in
  every build and every mode"* because *"even in the synthetic arm `vm_init.rs`
  calls THIS registrar again afterwards"* — it does not, and the claim is gone;
* the one calling its own copy *"the LOSING copy … nothing here is reachable"* —
  true of the two shipping modes, false of `--synthetic-jdk`, where this
  registrar runs last and wins.

They are replaced by one banner above the shared bodies giving each registrar's
call chain with its `vm_init.rs` line numbers, and a per-mode table at the
`register_phase57_nio_file` site. A third comment in the same file (*"vm_init.rs:1788
and :2273"*) had rotted by ~480 lines and was corrected, with a note not to trust
either pair without re-reading.

### 8.4 Items 4–6

**`writeUTF` / `readUTF` / `writeChars` (item 4).** Applied as written:
`encode_modified_utf8` / `decode_modified_utf8`, `raf_utf_data_format` carrying
the decoder's message, the `> 65535` check left where it was and still measuring
the *encoded* buffer, and `writeChars` walking `s.encode_utf16()`. One consequence
the patch text did not state: `readUTF` can now **fail**. `from_utf8_lossy`
answered `U+FFFD` for exactly the bytes `DataInput.readUTF` specifies
`UTFDataFormatException` for, so a record written by any other JVM containing a
NUL or a supplementary character used to come back silently corrupted. All three
are FLAG tier (`CRATONVM_SYNTHETIC_RAF=1`) *and* synthetic-only, so no shipping
measurement moves; what moves is that the wire format now has one spelling in the
tree instead of one per crate.

**`Files.isSameFile` (item 5).** Applied, with the `vfs_decode` screen at the
call site as required. The screen is load-bearing, not belt-and-braces:
`paths_name_the_same_file` reads the identity of both files, so a jar/jrt-encoded
path — which names no host file — would come back `NotFound` and turn a
legitimate `false` into an exception. Inside an archive there are no links and no
`..`, so equality *is* the whole answer there rather than a fast path, and the
comment says so.

One deviation from the patch text's snippet, worth four extra lines: it routed
every error through `p57_io_error`, i.e. a bare `IOException`. HotSpot throws
`NoSuchFileException` for an absent path, so `NotFound` now goes to
`p57_no_such_file` naming whichever path is missing — reachable only for two
*different* spellings, because equal ones never touch the disk.

**The option-list scan (item 6).** All three sites, each from the walk the
scanner already performed:

* `fsp_new_input_stream` — `APPEND`/`WRITE` → `UnsupportedOperationException`,
  message `'APPEND' not allowed`, raised **before** the VFS branch and before any
  descriptor is reserved, because the JDK's check is the method's first
  statement. **Shipping.**
* `fsp_new_output_stream` — `READ` → `IllegalArgumentException("READ not
  allowed")`, before the open, so a refusal creates and truncates nothing.
  **Shipping.**
* `Files.copy(InputStream, Path, CopyOption[])` — every option but
  `REPLACE_EXISTING` → `UnsupportedOperationException(opt + " not supported")`,
  raised **before the source stream is drained**: an unsupported option must not
  consume the caller's stream on its way to throwing. Synthetic-only.

`P57OpenFlags` gained `read` and `write`. The two exception types differ
**deliberately** and the code says so at both sites — the asymmetry is the JDK's,
and a caller distinguishing them by `catch` clause sees it. One knowing
deviation: where both `APPEND` and `WRITE` are present the JDK names whichever
comes first in the caller's array; this scan has collapsed the order, so it names
`APPEND`. Only the type and the fact of the refusal are load-bearing.

**The half-fix hazard, stated plainly**, because `run.sh` fails on any `CK`-line
difference from HotSpot: landing these three refusals *without* the matching
`RNioNoFollow` assertions is safe — the suite sees no new output. Landing the
assertions without the refusals reddens it. The Java is in §8.5 and is **not**
applied; `regression-suite/` is another lane's.

### 8.5 The fixture Java these refusals unlock — NOT applied

For `regression-suite/src/RNioNoFollow.java`, whose `symlinkArms()` bails on
Windows so that every `NOFOLLOW_LINKS` arm in it has never executed on the
primary platform. None of this needs a link or a privilege:

```java
    static void optionListIsRead() throws Exception {
        Path f = Files.createTempFile("rnio-opt", ".tmp");
        try {
            String in = "none";
            try { Files.newInputStream(f, StandardOpenOption.WRITE).close(); }
            catch (UnsupportedOperationException e) { in = "UnsupportedOperationException"; }
            catch (Exception e) { in = e.getClass().getSimpleName(); }
            System.out.println("CK RNioNoFollow newInputStream.WRITE=" + in);

            String out = "none";
            try { Files.newOutputStream(f, StandardOpenOption.READ).close(); }
            catch (IllegalArgumentException e) { out = "IllegalArgumentException"; }
            catch (Exception e) { out = e.getClass().getSimpleName(); }
            System.out.println("CK RNioNoFollow newOutputStream.READ=" + out);

            String cp = "none";
            try (InputStream src = new ByteArrayInputStream(new byte[] { 1, 2, 3 })) {
                Files.copy(src, f, LinkOption.NOFOLLOW_LINKS);
            } catch (UnsupportedOperationException e) {
                cp = "UnsupportedOperationException";
            } catch (Exception e) {
                cp = e.getClass().getSimpleName();
            }
            System.out.println("CK RNioNoFollow copyStream.NOFOLLOW=" + cp);

            // Anti-vacuity: the LEGAL spellings must still work, or a VM that
            // refused every option would satisfy all three refusals above.
            try (InputStream ok = Files.newInputStream(f, StandardOpenOption.READ)) {
                System.out.println("CK RNioNoFollow newInputStream.READ=ok");
            }
            try (OutputStream ok = Files.newOutputStream(f, StandardOpenOption.WRITE)) {
                System.out.println("CK RNioNoFollow newOutputStream.WRITE=ok");
            }
            try (InputStream src = new ByteArrayInputStream(new byte[] { 1, 2, 3 })) {
                Files.copy(src, f, StandardCopyOption.REPLACE_EXISTING);
                System.out.println("CK RNioNoFollow copyStream.REPLACE=ok");
            }
        } finally {
            Files.deleteIfExists(f);
        }
    }
```

Expected on HotSpot 25, and on CratonVM only once the refusals land:
`UnsupportedOperationException`, `IllegalArgumentException`,
`UnsupportedOperationException`, then three `ok`s. Before the change the first
three print `none`. Imports: `java.io.ByteArrayInputStream`,
`java.io.InputStream`, `java.io.OutputStream`, `java.nio.file.LinkOption`,
`java.nio.file.StandardCopyOption`, `java.nio.file.StandardOpenOption`.

The last three rows are not decoration, and §7.6 already says why: a VM that
threw on every option would satisfy all three refusals. `copyStream.REPLACE=ok`
matters most — `REPLACE_EXISTING` is the one option the copy path is supposed to
accept, and it is what a too-eager refusal would break.

### 8.6 Coverage this pass does NOT have

**The `FileChannel` re-aim itself has no scheduled assertion.** The seven triples
are reachable from Java only through a `java/nio/channels/FileChannel` that
CratonVM minted through `newFileChannel`'s legacy fallback, and that fallback
runs only when constructing a real `FileChannelImpl` fails — which on a healthy
real-JDK image it does not. So a vector written against `--real-jdk` would
exercise the real JDK's own bytecode and prove nothing about this change. Stated
rather than papered over: **rows 1, 2 and 19 are fixed in source on the shipping
path and are unasserted**, and the vector that would assert them has to run under
`--synthetic-jdk`.

---

## 9. The `AsynchronousFileChannel` surface, 2026-08-12 — the live owner is a THIRD file, and both of its live triples are fabricated success

Same standing as everything above: **nothing was built, checked, tested or run.**
No `cargo` command of any kind was executed. Every CratonVM claim is a claim
about *source*; every JDK claim is `javap -p` or `src.zip` against Eclipse
Adoptium 25.0.3.9 on this host (`javap -version` = `25.0.3`).

§2's tier table names `register_async_file_channel` (`native-io/src/lib.rs`) as
the DEFAULT owner of the AFC surface, and §3.3 row 46 fixed
`AsynchronousFileChannel.truncate(long)` there. That is true of the eleven
triples that registrar names and **false of the class**. `javap -p
java.nio.channels.AsynchronousFileChannel` declares thirteen instance methods;
`register_async_file_channel` names eleven of the reachable ones, and the two it
does not name — `force(Z)V` and `lock()Ljava/util/concurrent/Future;` — are
served by a registrar in a **third** file that this record never opened. Both of
them are this record's species, and one of them is a hang.

This is the §7.1 lesson recurring with the polarity reversed. There, §3.1's
fixes had landed on the losing registrar. Here the record fixed the *winning*
registrar and never asked what the winner does **not** register — and
last-write-wins answers "then somebody else's body runs", not "then real
bytecode runs", because these methods are `abstract` on a class CratonVM itself
instantiates.

### 9.1 The three registrars and the per-triple winner

Ordering re-measured from `vm/src/vm/vm_init.rs` on this tree — **the line
numbers in §7.1 and §8.1 have drifted by ~70, so anchor on the identifiers**:

| arm | first | last |
|---|---|---|
| `#[cfg(feature = "synthetic-jdk")]` (`:1903`) + `if config.use_synthetic_jdk` (`:1905`) | `register_builtins` `:1907` | `register_io_natives` **`:1908`** |
| its `else` (feature build, real-JDK arm) | `register_essential_natives_with_shims` `:2028` | `register_io_natives` **`:2225`** |
| `#[cfg(not(feature = "synthetic-jdk"))]` (`:2540`) — the shipping `cratonvm-cli` | `register_essential_natives_with_shims` `:2566` | `register_io_natives` **`:2761`** |

`register_io_natives` is last in **all three**, so anything `native-io`
registers wins everywhere. The three registrars on this class:

* **A — `native-builtins/src/phases_late/net_channels.rs::register_p67_async_channels`.**
  Reached from `register_essential_natives_with_shims`
  (`native-builtins/src/lib.rs:7943`) — a SHIPPING registrar, unlike
  `register_p58_nio_channels` in the same file, which
  W7-88-net-channels-dead-registration.md measured as registering nothing at
  all. Also reached a second time in the synthetic arm via
  `register_synthetic_overrides` → `register_phase67_natives`
  (`lib.rs:24073`). Ambient kind `Bridge` (`net_channels.rs:1309`), so strict
  mode does not refuse it. Believes the layout
  `path_str=0, open=1, _unused=2` (`:1310`).
* **B — `native-io/src/lib.rs::register_async_file_channel`** (`:18816`), via
  `register_phase92_io_completeness` (`:18796`), via `register_io_natives`
  (`:6902`). Believes `AFC_FIELD_FD = 0`, `AFC_FIELD_PATH = 1`,
  `AFC_FIELD_OPEN = 2` (`:18057-18059`).
* **C — `native-io/src/nio_native.rs::register_t16_channel_overrides`**
  (`:1765`), called by `register_io_natives` at `:6908` — i.e. **after** B, with
  the comment *"Registered LAST so these overrides win"*. It re-registers
  `open`, `isOpen`, `size`, `close`. Its file header still declares A's layout
  (`nio_native.rs:1344`) but its bodies do not use it: `t16_afc_open` is
  `crate::native_afc_open(ctx, args)` verbatim (`:1427-1429`), the legacy
  3-slot body beside it is `#[cfg(any())]` — permanently dead — and the other
  three discriminate with `t16_afc_uses_real_handle` (`:1421`: `≥3` fields and
  slot 0 is an `Int`) before delegating to B. So **C wins those four triples and
  agrees with B**, and the object every AFC in this VM is born with is B's:
  slot 0 = `Int(fd)`, slot 1 = the path `String`, slot 2 = the open flag
  (`alloc_afc_channel`, `native-io/src/lib.rs:19145-19164`).

| triple | A | B | C | **winner** |
|---|:-:|:-:|:-:|---|
| `open(Path,[OpenOption])` | yes | yes | yes | C (= B's body) |
| `read(ByteBuffer,J)Future` / `write(ByteBuffer,J)Future` | yes | yes | — | B |
| `read/write(...,Object,CompletionHandler)V` | — | yes | — | B |
| `size()J` · `close()V` · `isOpen()Z` | yes | yes | yes | C (delegates to B) |
| `truncate(J)` | yes (no-op) | yes | — | B |
| `tryLock(JJZ)` | — | yes | — | B |
| **`force(Z)V`** | **yes** | — | — | **A** |
| **`lock()Ljava/util/concurrent/Future;`** | **yes** | — | — | **A** |

So of A's ten AFC registrations, eight are overwritten and **the only two that
dispatch are the two nobody audited.**

### 9.2 `force(boolean)` is a silent no-op on every channel this VM produces

`net_channels.rs:1487-1505`, the sole registrant:

```rust
r.register(afc, "force", "(Z)V", |ctx, args| {
    let this = obj_arg(args, 0)?;
    let metadata_only = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
    // Get the file path from field 0
    let path = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Ok(None),
    };
    if !path.is_empty() {
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
            if metadata_only { let _ = file.sync_data(); } else { let _ = file.sync_all(); }
        }
    }
    Ok(None)
});
```

Slot 0 on the object `alloc_afc_channel` built holds `Value::Int(fd)`. The
`match` therefore takes `_ =>` and **returns `Ok(None)` before reading its own
first argument.** `force(true)` on every `AsynchronousFileChannel` this VM
produces is a `void` method that returns having done nothing — the exact answer
§1's Instrument B is built to catch, and the worst possible one for this method,
because a durability barrier's caller records the data as committed on the next
line.

Four distinct defects are stacked in those nine lines, and they are worth
separating because a fix that repairs only the first still lies:

1. **Wrong slot.** Reads A's layout on B's object. This alone is the no-op.
2. **Inverted polarity.** `force(true)` means *content **and** metadata*
   (`src.zip`, `AsynchronousFileChannel.java:380-384`: *"If `true` then this
   method is required to force changes to both the file's content and metadata
   to be written to storage; otherwise, it need only force content changes"*).
   The body calls `sync_data()` when the flag is true and `sync_all()` when it
   is false. Its sibling on the synchronous class has it right —
   `nio_file.rs:15525-15531` is `if metadata { file.sync_all() } else { file.sync_data() }`.
   A one-line "read slot 1 instead of slot 0" fix would turn a no-op into a
   *wrong* fsync, which is the W7-20 laundering shape.
3. **Wrong handle.** It opens a **second** descriptor by path and fsyncs that
   one. CratonVM's own writes for this channel live behind
   `afc_files()` (`native-io/src/lib.rs:18104`), and on the synchronous side the
   equivalent code says why this matters: *"`clone_file` flushes any buffered
   writer for the fd"* (`nio_file.rs:15510`). Fsyncing an unrelated handle
   flushes nothing of the caller's. It also fails outright — silently — on a
   channel opened `READ`-only, because the second open asks for `.write(true)`.
4. **Swallowed error, and no closed check.** `let _ = file.sync_all()` discards
   the one failure the method exists to report
   (*"@throws IOException If some other I/O error occurs"*), and there is no
   open-flag test at all, so `force` on a **closed** channel also returns
   normally where `src.zip:386-387` says *"@throws ClosedChannelException If
   this channel is closed"*.

**Blast radius: the largest row in this record.** `force` is how a database
tells the OS its log is on disk. The mint site's own comment in the sibling
`write` body (`net_channels.rs:1439`) names H2's `FileAsync`, and H2's async
file store calls `channel.force(true)` on exactly this path. A silent no-op here
is not a wrong answer a test can see; it is a durability guarantee that is not
being made, and it surfaces as corruption after a crash, which no green suite
can distinguish from a healthy run.

**Scheduled, and unasserted.** `regression-suite/src/RJdkAsyncChannel.java` is in
`JDKONLY_CLASSES` (`run.sh:119`) and its `writeReadRoundTrip` calls
`ch.force(true); ch.force(false);` at `:143-144` — and then asserts only
`check(ch.isOpen(), "force() must not close the channel")`. **The defect path is
scheduled and the assertion beside it is satisfied by the no-op.** That is the
§6.5-of-README shape: a fixture that reaches the defect and measures something
else. The assertable half is the refusal — see the nomination.

### 9.3 `lock()` mints a real `java.base` class at the wrong width, and `Future.get()` never returns

`net_channels.rs:1506-1516`, again the sole registrant
(`grep -rn '"lock",' --include=*.rs` returns exactly one AFC hit):

```rust
let future = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/FutureTask", 2)?;
ctx.set_field(future, 0, Value::Object(None));
ctx.set_field(future, 1, Value::Int(1));
```

`java/util/concurrent/FutureTask` is a **real `java.base` class**, and
`try_alloc_concurrent_synthetic` resolves it and clamps the width up, so this is
a genuine `FutureTask` with the real layout. `javap -p
java.util.concurrent.FutureTask` on 25.0.3, statics excluded, superclass
`Object`:

| slot | field | what the mint writes | after `coerce_field_value_by_descriptor` |
|---:|---|---|---|
| 0 | `state` (`int`, volatile) | `Object(None)` | `Int(0)` — `gc/src/heap.rs:1657`, the `I` arm maps `Object(None)` to `Int(0)` |
| 1 | `callable` (`Callable`) | `Int(1)` | `Object(None)` — `heap.rs:1674`, the `L` arm degrades `Int` to null |
| 2 | `outcome` | — | zero |
| 3 | `runner` | — | zero |
| 4 | `waiters` | — | zero |

`FutureTask.NEW` is `0`. So the "done = true" the mint believes it is writing
lands on `callable` and is thrown away, and `state` is left at exactly the value
that means **not started**. Real `FutureTask.get()` is
`int s = state; if (s <= COMPLETING) s = awaitDone(false, 0L); return report(s);`
— `0 <= 1`, so it enters an untimed `awaitDone` and parks. Nothing can ever
complete it: `callable` is null, `runner` is null, and no thread holds a
reference to run it. **`ch.lock().get()` blocks forever**, and
`while (!f.isDone())` spins forever, because `isDone()` is `state != NEW`.

**Whether the native runs at all is the one thing source cannot settle, and both
answers are defects.** `lock()` is `final` on `AsynchronousFileChannel` and its
bytecode is `return lock(0L, Long.MAX_VALUE, false);`. If the registry wins —
which is what the census category `native-shadows-bytecode` counts, 226 rows on
an ordinary program — the hang above is what happens. If the bytecode wins, it
calls `lock(JJZ)Future`, which is `abstract` and registered nowhere, so it is
`AbstractMethodError`. There is no arm in which `lock()` works.

**Why this row belongs to this record and not to a concurrency lane.** It is
fabricated success *and* wrong-layout-on-a-real-class in one object: the native
returns a plausible `Future` — right static type, non-null, implements the
interface — that has never been connected to anything. §1's question answers
itself: the caller cannot tell it from a real success **until it asks**, and
then it does not come back.

### 9.4 The same mint, three more times — and the fix already exists in the same file

`try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/FutureTask", 2)`
appears four times, all in `net_channels.rs`:

| site | triple | live? | `state` ends as |
|---|---|---|---|
| `:1383` | `AsynchronousFileChannel.read(ByteBuffer,J)Future` | no — B wins | `Int(bytes_read)` |
| `:1464` | `AsynchronousFileChannel.write(ByteBuffer,J)Future` | no — B wins | `Int(bytes_written)` |
| **`:1511`** | **`AsynchronousFileChannel.lock()Future`** | **YES — sole registrant** | `NEW` → hangs |
| **`:1816`** | **`AsynchronousServerSocketChannel.accept()Future`** | **YES** — `native-io/src/async_socket.rs:3516` registers only `accept(Object,CompletionHandler)V` | `NEW`, and slot 1 is `Int(0)` so even the mint's own belief says "pending" |

The two dead ones are worth keeping in the table for what they would do if the
order changed: `state := Int(n)` for an arbitrary byte count walks straight
through `FutureTask`'s constant block — `NORMAL=2`, `EXCEPTIONAL=3`,
`CANCELLED=4`, `INTERRUPTING=5`, `INTERRUPTED=6` — so a 3-byte read would report
itself as having completed exceptionally and `get()` would throw
`ExecutionException` wrapping whatever `outcome` held. That is a *worse* failure
than the hang and it is one registration-order change away.

**And the correct helper is already in this codebase, already used, and its own
doc comment already contains the diagnosis.**
`native-builtins/src/phases_late/concurrent.rs:1363-1369`:

> *"A synthetic `FutureTask` does NOT work here: in real-JDK mode
> `FutureTask.get()` runs the real bytecode (reads the real `state` field, stuck
> NEW) → the websocket client's `fConnect.get(timeout)` TimeoutException."*

`aio_completed_future(ctx, result)` is four lines over
`CompletableFuture.completedFuture`. It was applied to
`AsynchronousSocketChannel.connect(SocketAddress)Future`
(`net_channels.rs:1621`) and to `asc.read`/`asc.write` (`:1690`, `:1762`) — and
to **none** of the four `FutureTask` sites twenty lines away in the same file.
`native-io`'s `wrap_completed_future` (`lib.rs:19543`) is the same fix arrived at
independently for B's `read`/`write`. This is the
"correct helper exists but only one callsite uses it" shape: the diagnosis was
written down and then applied at the callsite that produced the failing test,
not to the shape.

### 9.5 Two lower-ranked rows on the same surface, for completeness

Both in `native-io/src/lib.rs`, both this record's own file, both smaller than
anything in §9.2–9.4 and neither fixed here:

* **`native_afc_size` on a closed channel is a bare `IOException`, not
  `ClosedChannelException`** (`:19467-19477`). `native_afc_close` (`:19479`)
  clears `AFC_FIELD_OPEN` but leaves `AFC_FIELD_FD` holding the id it just
  removed from `afc_files()`, so `size()` finds a positive fd, `afc_file_size`
  fails to look it up, and the error is mapped to `IOException("size: …")`.
  Exactly §3.1 row 19's species — the failure *is* reported, with a type
  `catch (ClosedChannelException)` does not match.
* **`native_afc_read_handler` / `native_afc_write_handler` discard the
  application handler's exception** (`:19406`, `:19417`, `:19446`, `:19456` —
  four `let _ = ctx.invoke_virtual(handler, "completed"/"failed", …)`). Scored
  deliberately low rather than omitted: on HotSpot the handler runs on a group
  thread and a throw there reaches only the thread's uncaught handler, so the
  *observable* gap is narrow. What is not narrow is that the `let _ =` also eats
  a `MethodCallFailed` raised by the VM itself on the way into the handler, and
  those two are indistinguishable at the call site.

### 9.6 Out-of-file patch items 7–9 (NOT applied)

All three are in `native-builtins/src/phases_late/net_channels.rs`, which is
W7-88's file, not this one. Anchor on the quoted text, not the line number.

**7. `AsynchronousFileChannel.force(Z)V` — refuse when closed, sync the
channel's own handle, and get the polarity right.** The refusal is the part with
a scheduled witness, so it can land first and alone. Replace the whole body
registered at `r.register(afc, "force", "(Z)V", …)`:

```rust
    r.register(afc, "force", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Layout note: the channel is minted by `native-io`'s
        // `alloc_afc_channel` -- AFC_FIELD_FD=0, AFC_FIELD_PATH=1,
        // AFC_FIELD_OPEN=2. This body used to read slot 0 as a path String
        // under THIS file's `path_str=0` belief, take the `_ =>` arm on the
        // Int it actually found, and return Ok(None): `force(true)` was a
        // silent no-op on every channel the VM produces. The one registration
        // that decides the layout is the one that ALLOCATES.
        if !matches!(ctx.get_field(this, 2), Value::Int(1)) {
            return Err(cratonvm_native_api::RuntimeError::IOException {
                message: "AsynchronousFileChannel is closed".into(),
            }
            .into());
        }
        // `metaData == true` is "content AND metadata" (JDK 25
        // AsynchronousFileChannel.java:380-384), i.e. sync_all. The previous
        // body had this backwards.
        let metadata = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let handle_id = match ctx.get_field(this, 0) {
            Value::Int(v) if v > 0 => v as u32,
            _ => return Ok(None),
        };
        cratonvm_native_io::afc_sync_at(handle_id, metadata).map_err(|e| {
            cratonvm_native_api::RuntimeError::IOException {
                message: format!("AsynchronousFileChannel.force: {e}"),
            }
        })?;
        Ok(None)
    });
```

with, in `native-io/src/lib.rs` beside `afc_truncate_at` (`:18371`) and exported
the same way `native_afc_open` is:

```rust
/// `AsynchronousFileChannel.force(boolean)`: fsync THIS channel's handle.
/// Opening the path a second time (what the previous caller did) syncs a
/// descriptor that carries none of this channel's buffered writes.
pub fn afc_sync_at(id: u32, metadata: bool) -> io::Result<()> {
    let entry = afc_file_entry(id)?;
    let handle = entry.lock();
    if metadata {
        handle.file.sync_all()
    } else {
        handle.file.sync_data()
    }
}
```

The `ClosedChannelException` type is the one deviation to argue about: this
patch raises `IOException` to match `native_afc_truncate`'s existing refusal
(`native-io/src/lib.rs:19008-19013`) rather than introducing a typed builder
into a file that has none. If the two land together the typed one is better —
`ClosedChannelException extends IOException`, so tightening later cannot break a
handler, exactly as §3.1 row 19 argues.

**8. The three remaining `FutureTask` mints → `aio_completed_future`.** Same
edit at three sites; `use` is already in scope, the helper is `pub(crate)` in the
same module tree, and `asc.connect` twenty lines away is the worked example. For
`lock()` (`:1506-1516`) the whole closure body becomes

```rust
        |ctx, _args| aio_completed_future(ctx, Value::Object(None)),
```

**but this is a fabricated success even after the fix** — a `Future<FileLock>`
completing with `null` is not what `lock()` promises, and the honest shapes are
either (a) route it to `native_afc_try_lock`'s real OS-advisory-lock plumbing
(`native-io/src/lib.rs:18955`) and complete the future with the resulting
`sun/nio/ch/FileLockImpl`, or (b) delete the registration so the `final`
bytecode runs and the missing `lock(JJZ)Future` becomes a loud
`AbstractMethodError`. (a) is the right one and it is small, because the
plumbing already exists for `tryLock()`. **Do not land the one-line version on
its own**: it converts a hang into a `null` that a caller will dereference, and
that is trading a loud failure for a quiet one — the mistake §1 records as
W2-7 #1.

For `assc.accept()` (`:1811-1821`) and the two dead `read`/`write` mints
(`:1383`, `:1464`) the mechanical `aio_completed_future` swap is correct as-is —
`accept()` has no channel to hand back, so it should complete with `null` only
if the surrounding `open`/`bind` surface is also honest; otherwise the same
argument as `lock()` applies and deletion is better. Either way, **no
`java/util/concurrent/FutureTask` should be minted by field-index writes
anywhere in this tree**, and a ratchet on that string in `net_channels.rs` is
worth more than any of the individual fixes.

**9. Correct `nio_native.rs:1344`'s layout comment.** It states
`AsynchronousFileChannel = 3 fields (path_str=0, open=1, _unused=2)` in a file
whose only surviving AFC body delegates to the crate that uses
`fd=0, path=1, open=2`. The stale comment is what the `force` body was written
against, and it is still there for the next reader. One line, and it should say
which registrar allocates.

### 9.7 What is scheduled, stated per row

| finding | scheduled evidence today | after the nominations |
|---|---|---|
| `force` no-op | `RJdkAsyncChannel.writeReadRoundTrip` **calls it** (`:143-144`) and asserts only `isOpen()` — vacuous | the closed-channel refusal is assertable from Java; the fsync itself is not |
| `lock()` hang | **none.** No fixture in `regression-suite/src` mentions `AsynchronousFileChannel.lock`; `probes/` holds only `AsyncCloseProbe`, and `run.sh` reads a word list, not `probes/` | a fixture that calls `ch.lock()` and `get(1, SECONDS)` distinguishes all three states — and must be *timed*, because the pre-fix behaviour is "never returns" and an untimed vector hangs the suite instead of failing it (the `WatchService` lesson, §7.4) |
| `assc.accept()` hang | **none.** No fixture mentions `AsynchronousServerSocketChannel` at all | same shape |
| `size()` after close | `RJdkAsyncChannel.sizeTruncateClose` exists; whether it asserts the exception *type* was not read | — |

The `force` row is the one to take first, and the reason is that it is the only
one of the three whose defect path a green suite is already walking.
