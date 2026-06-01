# System.setOut redirect + PrintStream user-stream routing + unbuffered FOS (2026-06-01)

Three independent, verified VM correctness fixes, plus the deep-dive findings on
the remaining DaCapo `luindex` blockers (kept for follow-up).

## Fixes landed

### 1. `System.setOut` / `System.setErr` were silently ignored
`getstatic java/lang/System.out`/`err` had a bootstrap intercept that ALWAYS
returned the canonical synthetic fd-backed stream (`ensure_system_streams()`),
disregarding any user stream installed via `System.setOut(...)` (which the
`setOut0`/`setErr0` natives store into the `System.out`/`err` **static field**).
So every program that redirects stdout/stderr (logging frameworks, test
harnesses, DaCapo's `TeePrintStream`) had its redirect dropped — output went to
the real console and the redirected sink stayed empty.

Fix (`interpreter.rs`, Getstatic): read the static field first; a non-null value
(the user override) wins, else fall back to the canonical synthetic stream
(boot phase / no override). The static field is a **GC root** the collector
updates on object motion — unlike the `native-builtins` override *map*, whose
raw `ObjectRef` went stale when GC moved the user stream ("stale pointer …
falling back to CP class PrintStream").

Repro (`SetOut2`): `System.out == ps` after `setOut(ps)` is now `true`; HotSpot
parity.

### 2. `PrintStream` natives ignored the receiver's underlying stream
The blanket `println`/`print`/`write`/`flush` natives resolved every PrintStream
to a fixed fd (1/2) and wrote straight to it — so output to a *user* PrintStream
(`new PrintStream(fos)`, or a `TeeOutputStream`) bypassed the real stream chain.

Fix (`native-builtins/lib.rs`): `route_write_through_out` — when the receiver's
`FilterOutputStream.out` field is non-null (a real wrapper stream), write the
bytes through `out.write([B,0,len)` via `invoke_virtual` (and `out.flush()` for
flush); only the canonical synthetic streams (null `out`) take the fd fast-path.
GC-safety: `new_array` can trigger a compacting GC; `this` is pinned for the
native call but a pre-allocation `out` ref is not, so `out` is **re-resolved
after** the allocation (a pre-alloc `out` dangled → the avrora SEGV during
bring-up).

Repro (`TeeTest`): a `FilterOutputStream` subclass overriding `write(int)` now
tees to both screen + file, byte-for-byte matching HotSpot.

### 3. `FileOutputStream` writes were userspace-buffered (non-Java semantics)
`fd_table` wrapped `FileWrite` in a `BufWriter`, so FOS writes sat in userspace
until an explicit `flush()`/`close()`. Java's `FileOutputStream.write` is
**unbuffered** — bytes hit the OS immediately and are visible to a concurrent
reader on a separate handle. CratonVM violated this: a second `open` of the same
path read an empty/partial file.

Fix (`fd_table.rs`): flush the `BufWriter` after every `FileWrite` write. Apps
that want batching use `BufferedOutputStream` (Java-side), which hands us
already-coalesced chunks — matching HotSpot, whose FOS issues a `write()`
syscall per call.

Repro (`Visib2`): write-then-read across two handles with no flush now returns
the bytes (HotSpot parity).

## Verification
`SetOut2`/`TeeTest`/`Visib2` match HotSpot. Regression pool clean on
cratonvm-cpu: commons-math **3204/3204**; BC math/math-raw/util/asn1/crypto-prng
all PASS; **DaCapo avrora PASS**. No regressions.

## DaCapo luindex — why it is still blocked (deep chain, for follow-up)
With these fixes plus a (reverted) experiment routing RAF through real bytecode,
luindex was traced end-to-end. It needs a *chain* of further deep fixes:

1. **RAF dispatch-bypass (real, NOT fixed here).** `java.io.RandomAccessFile`
   construction is intercepted by high-level natives
   (`phases_late::register_phase57_random_access_file` AND
   `native-io::register_io_extras_natives`) that win over real bytecode via the
   invoke path's `native_methods.find(class_name,…)` (interpreter.rs ~11034,
   which beats bytecode even when the class declares its own). The registered
   `<init>` shadows the real JDK ctor, so `new FileDescriptor(); open0(...)`
   never runs → `this.fd` is null, writes are dropped (luindex `FSDirectory.sync`
   → `new RandomAccessFile(f,"rw").getFD().sync()` NPE / no index commit).
   - Removing those high-level natives DOES make RAF fully functional (real ctor
     + the complete `native-io::random_access_file` platform natives
     open0/read0/write0/seek0/…), and luindex then **indexes fully**. BUT it
     regresses: the real RAF ctor's `FileCleanable.register(fd)` /
     Cleaner / PhantomReference path SEGVs under sustained load (avrora and
     default-size luindex, rc=139). So the RAF removal was **reverted**; the
     correct fix is real-bytecode RAF **plus** fixing the FileCleanable/Cleaner
     SEGV. Tracked, not done.

2. **`BufferedReader.readLine()` returns empty (real, NOT fixed).** With RAF
   working, luindex `small` reached DaCapo's `FileDigest` validation, which does
   `new BufferedReader(new FileReader(stdout.log)).readLine()`. On CratonVM
   `FileReader.read(char[])` and `BufferedReader.read()` work, but
   `BufferedReader.readLine()` returns **0 lines** on a non-empty file (HotSpot:
   correct) — a real-bytecode `readLine`/`implReadLine` bug (the synthetic-jdk
   BufferedReader natives are correctly gated off in real-JDK mode, so this is
   the JDK `readLine` bytecode itself). Minimal repro: `RdTest3`.

`luindex` cannot pass until both #1 (RAF + FileCleanable) and #2 (readLine) are
fixed. `sunflow` (AWT/Java2D/ImageIO native surface) and `fop`
(ClassLoader-modelled-as-Object / classloader isolation + SEGV) remain the
large, separate efforts documented in
`docs/dacapo-luindex-sunflow-fop-investigation.md`.
