# native-io audit — 2026-07-26

Scope: `native-io/src/**` (19 files, ~50 kLoC). Base: `arch/wave1-integration-20260726`
merged into this worktree at `473bb3f93af619b9da2972816dfa4389e3169da4`.

The hunt was for **silent-wrong-behaviour** defects — the class the prior art in this
area belongs to (`setDoOutput` wrong slot, `ServerSocket.bind()` no-op,
`DirectByteBuffer.put(byte)` lost writes, ByteBuffer mark/reset aliasing,
`MockWebServer.close()` AssertionError). Everything below either corrupts data,
loses data, or hangs — none of it raises a clean exception.

---

## 1. What is actually live on the default (real-JDK) path

`register_io_natives` **is** called on the default real-JDK path
(`vm/src/vm/vm_init.rs:1608`, the non-`synthetic-jdk` arm). Almost every registration
inside it is tagged `NativeKind::Bridge`, and Bridge natives are never suppressed —
only `SyntheticStub`-tagged natives on real-protected classes yield to real bytecode
(`vm/src/vm/vm_exec.rs:11655`, `vm/src/runtime/interpreter.rs:31339`). So a Bridge
registration in this crate **overrides the real JDK's own bytecode**.

### Live (default build)

| Area | Entry points |
|---|---|
| `java/io/File` metadata | `exists`, `isFile`, `isDirectory`, `length`, `delete`, `mkdir(s)`, `list`, `canRead/Write`, `createNewFile`, `renameTo` |
| `java/io/FileInputStream` / `FileOutputStream` | `open0`, `read`/`readBytes`, `write`/`writeBytes`, `available`, `skip`, `close` |
| `java/io/FileDescriptor` | `close0` |
| `sun/nio/cs/StreamDecoder` / `StreamEncoder` | `forInputStreamReader`, `read()`, `read([CII)`, `ready`, `close`, `getEncoding`; encoder `write` family — **backs every `InputStreamReader` / `Files.newBufferedReader` / `Channels.newReader`** |
| `java/nio/channels/Pipe` + `sun/nio/ch/{Source,Sink}ChannelImpl` | `open`, `source`, `sink`, `read(ByteBuffer)`, `read([BII)`, `write(ByteBuffer)`, `write([BII)`, `isOpen`, `close`, `configureBlocking` |
| `java/nio/channels/SocketChannel` / `ServerSocketChannel` | `open`, `connect`, `finishConnect`, `accept`, `read`/`write` (scalar + vectored), `configureBlocking`, options |
| `java/nio/channels/AsynchronousSocketChannel` / `…ServerSocketChannel` | Future **and** handler forms of `connect`/`read`/`write`/`accept` |
| `java/nio/channels/DatagramChannel`, `MembershipKey` | `send`/`receive`/`bind`/`join` |
| `sun/nio/ch/*` | `IOUtil`, `SocketDispatcher`, `UnixDispatcher`, `NativeSocketAddress`, `FileKey`, `EPoll`, `EventFD`, `Util`, `WindowsSelectorImpl` |
| `sun/nio/fs/UnixNativeDispatcher`, `java/nio/file/*` | `Files` surface, `WatchService`/`WatchKey`/`WatchEvent` |
| `java/nio/DirectByteBuffer`, `java/nio/Bits`, `jdk/internal/ref/Cleaner` | direct-buffer alloc/free/cleaner |
| `java/lang/ProcessHandleImpl`, process pipes | `process.rs` surface |
| `java/io/RandomAccessFile` `open0`/`read0`/`readBytes0`/`write0`/… | real-JDK RAF path (default) |

### Synthetic-jdk only — **compiled out of a default build**

The single most important finding for effort-allocation: **`register_nio_natives`
is gated at its call site** (`native-io/src/lib.rs:5554`):

```rust
#[cfg(feature = "synthetic-jdk")]
register_nio_natives(registry);
```

That one gate takes out the entire `native_bb_*` / `native_cb_*` / `native_tb_*`
family — roughly `lib.rs:6336`–`13500`, i.e. every synthetic `ByteBuffer`,
`CharBuffer`, `IntBuffer`, `LongBuffer`, `FloatBuffer`, `DoubleBuffer`,
`ShortBuffer` accessor. Also gated: the synthetic `InputStreamReader` /
`OutputStreamWriter` / `BufferedReader` / `BufferedWriter` / `Reader` / `Writer`
char-stream overrides, `CharArrayWriter`, `LineNumberReader`, synthetic
`FileChannel.lock()`/`tryLock()` and `FileLock`, and the synthetic
`DatagramChannel` family in `nio_native.rs:1280`+.

A sweep of that gated region turned up a long list of genuine JDK-contract
violations — relative `get()`/`put()` swallowing `BufferUnderflow`/`BufferOverflow`
instead of throwing *and* not advancing position; multi-byte accessors hardcoding
big-endian while `order(ByteOrder)` flips a `bigEndian` field nobody reads;
`slice()`/`duplicate()`/`asReadOnlyBuffer()` **copying** rather than sharing the
backing store; `ByteBuffer.wrap` copying the array and never validating
`offset`/`length` so `limit > capacity` is reachable; absolute accessors bounded
against `capacity` instead of `limit`; `position(int)`/`limit(int)` clamping where
the JDK throws. **None of it runs by default.** Recorded in §5 as a landmine list —
do not spend time there unless `register_nio_natives` is ever un-gated, at which
point every item becomes live simultaneously.

Note also: the synthetic `RandomAccessFile` write natives (`lib.rs:11062`+) discard
both the error and the short-write count from `FdTable::rw_write` (which is
`write(2)`, not `write_all`). They register only under `if !real_raf_enabled()`, and
the default is real-JDK RAF — so this is opt-in dead code today. **The comment at
`lib.rs:10834` says "Default (unset) = synthetic", which is inverted relative to
`real_raf_enabled()` at `lib.rs:10817`.** Stale comment, worth correcting.

---

## 2. Defects found and FIXED

All five are on the default real-JDK path. Each has `#[cfg(test)]` coverage that
fails before the change.

### D1 — `StreamDecoder` silently drops decoded characters (data loss)
`native-io/src/stream_decoder.rs`, `decode_into` (the `ncopy` truncation) and
`native_sd_read`.

`decode_into` ended with `let ncopy = chars.len().min(len);` and wrote only
`chars[..ncopy]`. The surplus was **discarded** — not written to the destination,
and not recoverable from `rest` (which holds *undecoded bytes* only). The doc
comment asserted this was unreachable because total bytes are kept `<= len`, but
the progress-forcing branch a few lines above deliberately breaks that invariant:

```rust
let mut want = len.saturating_sub(bytes.len());
if want == 0 && !bytes.is_empty() { want = 4; }
```

`Reader.read()` calls `decode_into(..., len = 1)`, so the invariant is broken the
moment a multi-byte character appears.

**Failure scenario.** `new InputStreamReader(in, "UTF-8")` over `"éabcd"`
(`C3 A9 61 62 63 64`), read one char at a time:

- call 1: `len=1`, `want=1` → reads `C3`, incomplete prefix → `carry=[C3]`, returns 0
- call 2: carry fills `len` → `want=4` → reads `A9 61 62 63` →
  `chars = ['é','a','b','c']`, `ncopy = 1` → `'é'` delivered, **`'a','b','c'` gone**,
  and `rest` is empty so the carry cannot hold them either
- result: `"éabcd"` reads back as `"éd"`

Silent — no exception, no short-read signal. Hits every char-at-a-time tokenizer
(`StreamTokenizer`, hand-rolled config/JSON/SQL scanners) over any non-ASCII input.

**Fix.** Stash the surplus in `SdState::pending`, the read-ahead queue that already
exists for exactly this purpose and that `native_sd_read_chars` already drains.

### D2 — `StreamDecoder.read()` bypassed the read-ahead queue
Same file, `native_sd_read`.

`read(char[],int,int)` parks its surplus in `SdState::pending`; `read()` went
straight to `decode_into` and never looked at it. Interleaving the two on one
reader — a `BufferedReader` wrapper plus a direct `isr.read()`, or D1's fix —
skipped every queued character and returned the ones *after* them, then lost them
at `close()`. Out-of-order delivery followed by loss.

**Fix.** Drain one char from `pending` before pulling fresh bytes. This is also
what makes D1's stashed surplus reachable.

### D3 — `StreamDecoder.ready()` ignored decoded read-ahead
Same file, `native_sd_ready`.

Checked `carry` (incomplete *bytes*) but not `pending` (fully decoded *chars*). A
reader whose surplus sat in `pending` while the underlying stream had
`available() == 0` reported `ready() == false` with a character immediately
deliverable. Latent before D1/D2; load-bearing after.

**Fix.** `!carry.is_empty() || !pending.is_empty()`.

### D4 — `Pipe` channels ignored the ByteBuffer `offset` field (wrong-region I/O)
`native-io/src/pipe.rs`, `buffer_view` + `source_read_buffer` / `sink_write_buffer`.

`buffer_view` returned `(arr, position, limit)` and callers indexed the backing
array by the raw `position`. The real-JDK `ByteBuffer` also has an `offset` field:
`HeapByteBuffer.slice()` returns
`new HeapByteBuffer(hb, -1, 0, rem, rem, pos + offset)` — it resets `position` to 0
and folds the parent position into `offset`. So for **every sliced buffer** the
pipe read/write touched the wrong region of the backing array.

**Failure scenario.** `pipe.source().read(buf.slice())` writes the bytes at
`arr[0..n]` instead of `arr[offset..offset+n]`: the caller's slice reads back as
zeros while the bytes clobber whatever else lives at the head of the shared array.
The sink direction sends the *wrong bytes* on the wire. No exception either way.

`socket_channel.rs::buffer_access` (lines 705–746) already handled `offset`
correctly — `pipe.rs` was the divergent copy, which is the "same operation, two
implementations, one of them wrong" shape.

**Fix.** `buffer_view` now returns a `HeapBufferView { arr, base, position, limit }`
with `index() = base + position` and `remaining() = limit - position`, and both
callers clamp the transfer to what is addressable behind `index()` (a `limit` that
overshoots the backing array previously produced an out-of-range transfer).

### D5 — missing GC blocking regions around genuinely blocking kernel I/O
`native-io/src/pipe.rs` (`source_read_buffer`, `source_read_bytes`,
`sink_write_buffer`, `sink_write_bytes`) and
`native-io/src/socket_channel.rs` (`sc_read_scattering`).

This is the documented **"STW blocking-region missing native I/O family"** class.

- `pipe.rs` used `libc::read`/`write` (no `O_NONBLOCK`) and `ReadFile`/`WriteFile`
  with `OVERLAPPED = NULL` — fully synchronous by design (the module doc says so).
  A `Pipe.SourceChannel.read()` on an empty pipe parks in the kernel indefinitely.
  The Windows `CreatePipe` default buffer is 4 KiB, so the sink direction blocks too.
- `sc_read_scattering` called `try_read_nb` with **no bracket at all**, unlike every
  sibling in the same file (`sc_read` ~1799, `sc_write` ~1967,
  `sc_write_gathering` ~2109). A blocking-mode `SocketChannel` leaves its
  `TcpStream` in OS-blocking mode, so a scattering `read(ByteBuffer[])` — the shape
  Jetty/Netty-style reactors use for header+body reads — parks indefinitely.

**Failure scenario.** A concurrent stop-the-world pause waits forever on a mutator
that never reaches a safepoint. Whole-VM hang, no exception, no diagnostic.

**Fix.** Bracket each call in `begin_blocking_region()` / `end_blocking_region()`,
using `end_blocking_region_refs` where a Java ref is used after the region (a pause
inside the region can relocate it). `sc_read_scattering` additionally pins its
destination buffers and reloads them through `read_native_pin`, matching
`sc_write_gathering`.

**Audit result for task 3 generally:** `native-io` never manipulates
`in_blocked_region` directly — there is no raw store anywhere in the crate; every
site goes through the `NativeContext` enter/exit helpers. The sibling's
`resume_virtual_continuation` / `check_post_block_gc()` bug shape does **not**
recur here. The gap was purely *missing* brackets, not incorrect ones. All
begin/end pairs in `net.rs`, `lib.rs`, `datagram.rs`, `process.rs`,
`nio_selector.rs` and `socket_channel.rs` were walked and are balanced across every
early-return arm.

### D6 — async `write` handler form never advanced the source buffer
`native-io/src/async_socket.rs`, `aio_asc_write` → `Job::Write` → `drain_completions`.

`AsynchronousByteChannel.write` is specified to update the buffer's position by the
bytes written. The handler form delivered a bare `CompletionKind::IntCount(n)` and
never touched the buffer. `Job::Write` even *carried* it (`bb_obj: ObjectRef`) and
then destructured it away with `bb_obj: _` — and carried it **unrooted** across a
worker-thread blocking write, so a moving collection could relocate it.

This is the **same defect already found and fixed for the sibling Future form** on
2026-07-17 (`FutureOutcome::Count`, `async_socket.rs:494`+, whose comment records
a 35-byte STOMP CONNECT frame physically resent 67 times). The handler form was
missed.

**Failure scenario.** `while (buf.hasRemaining()) channel.write(buf, att, handler)`
re-armed from `completed()` — exactly Tomcat's `WsRemoteEndpointImplBase` /
`Nio2SocketWrapper` write loop — sees `hasRemaining()` still true after every
"successful" completion and resubmits the identical slice. The frame is
retransmitted in a tight loop; the peer rejects the duplicate and closes, which
presents as a premature/spurious close rather than a write bug.

**Fix.** `Job::Write` now holds the buffer as a global root (`bb_gref`), and a new
`CompletionKind::WriteCount { n, buffer_gref }` advances `position` by `n` **before**
invoking `completed()` and then releases the root. Handler-less and error exits park
the root on a `pending_root_releases` queue drained by `drain_completions` — same
idiom as the existing `PendingFieldReset`. Every `Job::Write` exit path reaches
exactly one of the three release routes.

### D7 — `sun/nio/ch/Net` `read0`/`write0` did not retry EINTR
`native-io/src/net.rs`, `net_read0` and `net_write0`.

Only `WouldBlock` was special-cased. `ErrorKind::Interrupted` fell through to
`net_err`, which has no `Interrupted` arm, producing
`SocketException: read0: Interrupted system call`. An interrupted read/write has
consumed/transferred **nothing**, so this is a spurious failure.
`socket_channel.rs::try_read_nb` / `try_write_nb` already retry, with a comment
saying it is required; this is the `NioSocketImpl` path (plain
`Socket.getInputStream().read()`) that was missed.

**Failure scenario.** A signal delivered while parked in the kernel — SIGCHLD from a
`process.rs`-spawned child without `SA_RESTART`, a profiler or JVMTI signal — aborts
a connection mid-request, randomly and unreproducibly, under load.

**Fix.** Retry loop on `ErrorKind::Interrupted || raw_os_error() == Some(4)`,
inside the existing blocking region.

---

## 3. A hypothesis that was WRONG (recorded so nobody re-derives it)

`decode_into` (and `native_sd_read`) pin `this_pin` then `out_pin` but only ever
call `unpin_native_roots(this_pin)`. That reads like a per-call pinned-root leak.

**It is not.** `unpin_native_roots(base)` **truncates** the thread's pin stack to
`base` (`vm/src/vm/vm_exec.rs:4010`, `native_pin_roots.truncate(base)`), so
releasing the first-taken pin releases every pin taken after it. The edits adding
explicit `out_pin` releases were made and then reverted; a clarifying comment was
left at the `bytes.is_empty()` exit instead. `native_sd_read_chars` calling both
`unpin_native_roots(this_pin)` and `unpin_native_roots(out_pin)` is a redundant
no-op, not evidence of a leak.

---

## 4. Hot-path cost (task 4)

**`std::env::var` on the I/O hot path: zero findings.** There is no `env::var`,
`env::var_os` or `env::vars` call anywhere in `native-io/src`. Every `CRATONVM_*`
flag goes through the cached `io_flags()` helper (`lib.rs:46`, a `&'static` borrow
of a once-parsed struct); all ~28 call sites are plain field reads. Nothing bypasses
it. The sibling's three-uncached-reads finding does not have an analogue here.

Real per-call costs that do exist, in rough priority order (none fixed — all are
performance-only, and the brief prioritised one real defect with a repro over a
broad sweep):

1. **Per-byte `Value`-boxed element loop in a live native.**
   `stream_decoder.rs:433-437` (`native_sd_read_chars`) copies up to
   `READ_AHEAD_CHARS = 4096` chars out of the scratch array one
   `get_array_element` at a time. `ctx.read_char_array_into` is the bulk intrinsic
   and is already used at `stream_encoder.rs:721`. This is the single best
   remaining perf win in the crate — it is on every `Reader.read(char[])`.
2. **Per-call `String`/`Vec` clones in the decoder.** `stream_decoder.rs:542`
   clones both the charset name and the carry on **every** `read`; `:722` clones
   the name again. `stream_encoder.rs:271-285` returns `name.to_string()` per write.
3. **Scratch `vec![0u8; len]` + full extra memcpy per read/write** —
   `lib.rs:1468`/`1509`/`1869`/`1894`/`1954`, `random_access_file.rs:333`/`400`,
   `socket_channel.rs:762`/`786`/`1784`. Structural (the bulk intrinsics take a
   Rust slice), but it is a second copy of every byte the process moves.
4. **`args.to_vec()` per read** — `lib.rs:1469`, purely to have a `&mut [Value]`
   for `end_blocking_region_refs`. A 2-element stack array would do.
5. **3 allocations per `java.io.File` metadata call** — `read_file_path`
   (`lib.rs:781`) `String`, `validate_path` (`lib.rs:381`/`421`) `to_string`, then
   `normalize_for_os` (`lib.rs:546`) does `replace('/', "\\")` on Windows
   **unconditionally**, even when the path contains no `/`. Fires on `exists`,
   `isFile`, `isDirectory`, `length`, `list`, `canRead`… — i.e. classloader and
   resource-scanner loops. Guarding the `replace` with a `contains('/')` test is a
   two-line win.
6. `random_access_file.rs:87` locks the global `sync_modes()` map on every
   `write0` even for plain `"r"`/`"rw"` handles that never have an entry.

---

## 5. Found, NOT fixed — with rationale

### 5a. Synthetic-jdk-only (compiled out by default) — landmine list
Live only if `register_nio_natives` (`lib.rs:5554`) is ever un-gated. Line numbers
are pre-existing.

- Relative `get()`/`put()` on Char/Int/Long/Float/Double/Short buffers return 0 /
  no-op instead of throwing `BufferUnderflowException`/`BufferOverflowException`,
  **and do not advance position** — `native_cb_get` 12850, `native_tb_get_int`
  13052, `_long` 13145, `_float` 13238, `_double` 13331, `_short` 13424, and the
  mirrored putters 12883/13085/13178/13271/13364/13457. A
  `try { while(true) sink(buf.get()); } catch (BufferUnderflowException e) {}`
  drain loop becomes an infinite loop emitting zeros. `native_bb_get` (7086) /
  `native_bb_put` (7166) get this right — same-operation divergence.
- All `ByteBuffer` multi-byte accessors hardcode big-endian
  (`from_be_bytes`/`to_be_bytes`) and never read the `bigEndian` field —
  7284/7304/7326/7350/7376/7396/7420/7440 plus the float/double/char delegates
  7464–7520. `direct_buffer.rs:589` seeds that field and
  `native-builtins/src/servlet.rs:4707` implements the `order()` setter, so the
  state exists and is authoritative; `order(LITTLE_ENDIAN)` silently produces
  byte-reversed wire format.
- `CharBuffer.charAt(int)` has **no bounds check at all** (`native_cb_char_at`
  12961) — `limit` is destructured away, so it reads past the limit and
  `charAt(-1)` wraps a `usize`. `CharBuffer` is a `CharSequence`, so this leaks
  stale characters into regex/`append` rather than throwing.
- `slice()`/`duplicate()`/`asReadOnlyBuffer()` **copy** instead of sharing —
  `native_bb_slice` 7602, `native_bb_duplicate` 7582, and the
  `tb_abstract_view_fns!` macro 12623–12692. Writes through a slice vanish from
  the parent. `asReadOnlyBuffer()` is also fully writable
  (`native_bb_is_read_only` 7578 always returns 0).
- `ByteBuffer.wrap` copies the array (so `bb.array() != arr`) and
  `wrap(byte[],int,int)` never validates `offset`/`length` against the array —
  6859/6876 — so `limit > capacity` is reachable and the `Direct` arm's
  `copy_to_native_memory` would write past the allocation.
- Absolute accessors bound against `capacity` where the JDK uses `limit`
  (7105/7186/7315/7365, 12874/12914/13076/13110/13169/13203/13448); `position(int)`
  6911 and `limit(int)` 6937 **clamp** where the JDK throws
  `IllegalArgumentException` — fail-slow instead of fail-fast.
- `bb_state` (6104) never reads `offset` while `bb_storage_view` (6154) does — the
  same `offset` divergence as D4, in the typed-buffer family.
- Mark handling was checked and is **correct** (6923/6958/7036/7048/7058/7080/7009).

### 5b. Encoding-boundary gaps in live code — specified, not fixed
`stream_decoder.rs`. Real and on the default path, but each needs its own
correctness design plus a charset-matrix test; bundling them into this change would
have made it unreviewable.

- **`split_complete_prefix` UTF-16 surrogate split** (`:802-809`): splits on an even
  byte boundary, but a supplementary character is a 4-byte surrogate *pair*. A
  refill landing between the high and low surrogate yields `U+FFFD U+FFFD`,
  non-deterministically by buffer alignment. Correct split is
  `bytes.len() & !1`, minus 2 more when the last complete unit is a high surrogate.
- **`split_complete_prefix` catch-all arm** (`:807`): `_ => bytes.len()` is
  commented "single-byte charsets" but is not restricted to them.
  `normalize_supported` accepts `Shift_JIS`, `EUC-JP`, `ISO-2022-JP`, `Big5`,
  `EUC-KR`, `GBK`, `GB2312`, `GB18030` (via `encoding_rs`), so a 2-byte CJK
  character or a 3-byte ISO-2022-JP escape straddling a refill is split and
  lossy-decoded on both sides — intermittent mojibake on any file larger than one
  refill.
- **UTF-16 BOM re-detected per refill** (`:702` → `charset.rs:876`): chunk 1 of a
  UTF-16LE-BOM file strips the BOM and decodes LE; chunks 2+ have no BOM and
  default to BE, so everything after the first refill is byte-swapped. The encoder
  already solves this with a `bom_written` latch (`stream_encoder.rs:271-286`); the
  decoder needs the symmetric once-only flag.
- **`native_sd_read_chars` `or_insert_with` hardcodes `"UTF-8"`** (`:442-447`),
  discarding the `cs`-field-derived name that `decode_into` resolved for
  `Channels.newReader`-built decoders.
- **`native_se_write_string`** (`stream_encoder.rs:745-750`) round-trips the Java
  `String` through a Rust `String` before recovering UTF-16 units, so an unpaired
  surrogate cannot survive and the `leftoverChar` carry never sees a real high
  surrogate. The `write([CII)V` path is correct; only the `String` overload is
  affected.

### 5c. `available()` structurally returns 0 for subprocess pipes — cross-crate
`lib.rs:1536`, `process.rs:1747`, `nio_native.rs:329` all do
`…available(fd).unwrap_or(0)`. The backing `FdTable::available`
(`native-api/src/fd_table.rs:778-802`) has arms only for `FileRead` and `Stdin`;
`ChildStdoutPipe` / `ChildStderrPipe` / `ChildMergedPipe` / `FileReadWrite` fall to
`_ => Err(NotFound)`, which the three call sites convert to `0`.

`Process.getInputStream()` is a real `FileInputStream` over exactly those entries
(`process.rs:1789-1833`), so
`while (proc.getInputStream().available() > 0) { … }` — commons-exec, Surefire's
forked-JVM readers, Ant/Maven `StreamPumper` variants — never enters the loop and
reads the child as empty. Silent truncation. **The fix belongs in `native-api`'s
`FdTable::available`, not here** — see §6.

---

## 6. Cross-owner requests

### R1 — `native-api`: `FdTable::available` must handle pipe and read-write fds
**File** `native-api/src/fd_table.rs`, fn `available` (~778-802).
**Ask** Add arms for `ChildStdoutPipe`, `ChildStderrPipe`, `ChildMergedPipe` and
`FileReadWrite`. For the pipe kinds, `FIONREAD` on Unix / `PeekNamedPipe` on
Windows; for `FileReadWrite`, `len - seek_pos` as the `FileRead` arm already does.
**Rationale** Three `native-io` call sites (`lib.rs:1536`, `process.rs:1747`,
`nio_native.rs:329`) currently turn the resulting `Err(NotFound)` into `0`, which
makes the standard `available() > 0` subprocess drain loop read a child process as
producing no output. `native-io` cannot fix this without duplicating fd-kind
knowledge that belongs to the table. See §5c.

### R2 — `native-api`: `available()` should distinguish "closed" from "nothing now"
**File** same fn.
**Ask** Keep a distinguishable error for a genuinely closed/unknown fd so the
`native-io` callers can throw `IOException` per
`FileInputStream.available()`'s contract instead of returning `0`.
**Rationale** Today a closed descriptor and an idle one are both `0`, so
`available()`-gated readers spin instead of terminating. Depends on R1.

### R3 — `vm`: stale inverted comment on the RAF mode default
**File** `native-io/src/lib.rs:10834` — *this one is in my files and I can take it*,
noted here only because it pairs with the flag semantics owned elsewhere: the
comment says "Default (unset) = synthetic" while `real_raf_enabled()`
(`lib.rs:10817`) makes the default **real**. Left untouched this pass to keep the
diff focused; flagging so it is not read as truth.

---

## 7. Test coverage added

All new tests fail before the corresponding change.

| Test | File | Guards |
|---|---|---|
| `audit_read_single_char_does_not_drop_surplus_after_multibyte` | `stream_decoder.rs` | D1 — `"éabcd"` must read back whole |
| `audit_read_single_char_handles_consecutive_multibyte` | `stream_decoder.rs` | D1 — `"中文ok"`, carry + surplus together |
| `audit_read_single_char_drains_pending_before_refilling` | `stream_decoder.rs` | D2 — interleaved bulk/single reads stay in order |
| `audit_ready_accounts_for_decoded_read_ahead` | `stream_decoder.rs` | D3 |
| `audit_buffer_view_honours_slice_offset` | `pipe.rs` | D4 — `index() == offset + position` |
| `audit_buffer_view_plain_allocate_unchanged` | `pipe.rs` | D4 — no regression for `allocate()` |
| `audit_buffer_view_missing_offset_field_is_zero` | `pipe.rs` | D4 — synthetic layout has no `offset` |
| `audit_source_read_writes_at_slice_offset_and_brackets_gc` | `pipe.rs` | D4 + D5 — bytes land at `offset`, exactly one blocking region |
| `audit_sink_write_reads_from_slice_offset_and_brackets_gc` | `pipe.rs` | D4 + D5 — correct bytes on the wire |
| `audit_buffer_view_clamps_limit_past_array_end` | `pipe.rs` | D4 — overshooting `limit` clamps |
| `audit_handler_write_completion_advances_source_buffer` | `async_socket.rs` | D6 — position advances, root released |
| `audit_parked_root_release_advances_then_frees` | `async_socket.rs` | D6 — worker-parked release path |
| `audit_zero_gref_is_not_queued` | `async_socket.rs` | D6 — sentinel handling |

`native-io/src/test_support.rs` gained three capabilities to make these possible:
`script_input_stream` (makes the mock's `invoke_virtual("read","([BII)I")` behave
like a real `InputStream`, filling the caller's array and returning a genuine count,
so the decoder's refill loop is exercised end to end rather than against a fixed
scripted return); real `add_global_root`/`resolve_global_root`/`remove_global_root`
bookkeeping (the trait defaults are inert stubs returning handle 0, which would have
made D6's whole code path a silent no-op under test); and a `global_root_count()`
accessor for leak assertions.

---

## 8. Constraints honoured

- **No new env-var gates, no default-off landings.** Every fix in §2 is
  unconditional. No `CRATONVM_*` variable was added, and no capability was placed
  behind one.
- **No build or test run** (nine concurrent cargo builds OOM the host). Each edited
  file was syntax-checked with `rustfmt --check --skip-children`, which parses
  without writing; all six parse clean. **The changes are unbuilt and untested —
  they need a `cargo test -p cratonvm-native-io` before they are trusted.**
- **Line endings preserved.** All six edited files verified still 100% CRLF
  (0 bare LF).
- **File ownership respected.** Only `native-io/src/**` and this document were
  written. Everything outside is in §6 as a request.
