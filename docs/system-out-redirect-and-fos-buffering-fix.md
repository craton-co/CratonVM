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

### Follow-up: `BufferedReader.readLine` root cause (the synthetic Reader stack)
Deeper investigation (2026-06-01, second pass) pinned #2 precisely, and it is
the **same synthetic-native-shadows-real-bytecode pattern as RAF — but spread
across the entire `java.io` Reader stack**:

- `servlet.rs::register_r3_resource_loading` (always registered) installs a
  blanket `BufferedReader.readLine()` native for a synthetic byte[]-backed r3
  resource reader (`BufferedReader.field0→InputStreamReader.field0→byte[]`,
  pos@1, count@3). Via `native_methods.find(class_name,…)` it beats real
  bytecode, and for a *real* `BufferedReader` the layout doesn't match →
  `r3_get_input_stream` returns None → the native returns **null**. That is
  why every real `readLine()` returns 0 lines (`read()` is unaffected — it has
  its own dispatch).
- Removing that native does NOT fix it: real `readLine` bytecode then throws
  `IOException: Stream closed` because the underlying Reader stack is *also*
  synthetic — `Reader.read`→`native_sr_read` (native-io lib.rs ~5871, ungated),
  `InputStreamReader.read`→`native_isr_read` (~3886), `FileReader`/`StringReader`
  are synthetic-native-backed, and the real `BufferedReader`/`ensureOpen`
  bytecode sees a null `in`. `read()` works only because it routes through the
  synthetic `Reader.read` natives; `readLine()` (real bytecode using
  `in`/`cb`/`fill`) does not mesh with them.

So a correct `readLine` requires making the whole Reader I/O stack real-bytecode
(`FileReader`→`InputStreamReader`→`StreamDecoder`→`FileInputStream`, plus
`BufferedReader` itself), exactly analogous to the RAF migration — and it will
hit the same class of cascading issues (Cleaner/PhantomReference, GC under
load). It is a subsystem migration, not a localized fix. The blanket
`readLine` native is left in place (returns null for real readers — the
pre-existing behavior) rather than throwing, to avoid regressing apps that
currently tolerate the null.

### Net assessment of luindex/sunflow/fop
All three are blocked by **subsystem-level** work, not bug fixes:
- **luindex**: real-bytecode I/O stack (RAF *and* Reader) + Cleaner/
  PhantomReference support that survives GC under load. Two independent deep
  migrations.
- **sunflow**: AWT/Java2D/ImageIO native surface (Toolkit, Disposer, image
  codecs, rasterizers).
- **fop**: ClassLoader-modelled-as-`Object` / classloader-isolation + the
  resulting SEGV.

The tractable underlying bugs in these chains have been fixed and committed
(SHA-384 long decode; System.setOut redirect; PrintStream user-stream routing;
unbuffered FOS). The remaining work is large and regression-prone (the RAF
real-bytecode experiment already SEGV'd avrora), so it should be scoped and
undertaken deliberately rather than as a quick patch.

### Follow-up: why real-bytecode RAF SEGVs avrora (the JIT re-entrancy UB)
A second attempt re-applied the RAF real-bytecode change and diagnosed the
avrora SEGV to the bottom:

1. **It is NOT the cleaner stale-address bug** — that was real and is now fixed
   (defer cleaner/finalizer dispatch under a JIT thread-borrow + GC-relocate the
   deferred queues; commit "fix(gc): defer cleaner/finalizer …"). Applying that
   fix did not stop the avrora SEGV.
2. **A debug build pinned the actual fault**: `jit_thread_mut: aliasing &mut
   JvmThread borrow detected (a prior JitThreadGuard is still live)` at
   `jit/helpers.rs:214`. The backtrace is pure nested `jit_invoke_dispatch`
   (no cleaner/GC frames): a JIT method's `jit_invoke_dispatch` holds the
   `&mut JvmThread` (slow path, `helpers.rs:2031`) across `bail_to_interpreter`,
   whose interpreter execution calls another JIT method whose code re-enters
   `jit_invoke_dispatch` → `jit_thread_mut` → a second live `&mut JvmThread` to
   the same thread. With the debug assert neutered the build runs on to a hard
   SEGV, i.e. the aliasing is genuine UB, not a benign over-assert: the inner
   call mutates `thread` (frame push/realloc) while the outer holds derived
   state, so under sustained load the outer dereferences moved/freed memory.
3. Real-bytecode RAF exposes this latent bug because the real RAF ctor +
   `FileDescriptor`/`Cleaner`/`FileCleanable` machinery adds more JIT-compiled
   methods to the nesting, deepening the `jit_invoke_dispatch` chains until the
   aliasing turns fatal. Synthetic-native RAF avoided it by never running that
   bytecode.

**Conclusion**: real-bytecode RAF is blocked on a JIT re-entrancy redesign —
JIT helpers obtain `&mut JvmThread` from a thread-local raw pointer
(`jit_thread_mut`), and nested interpreter↔JIT calls create overlapping `&mut`
borrows to the same thread. Making this sound (e.g. a borrow-token / single-owner
discipline threaded through `bail_to_interpreter`, or re-deriving all
thread-internal references after every nested call) is a JIT-subsystem effort,
not a localized fix, and masking it (JIT skip-list for RAF-path methods) is
disallowed by the no-mask rule. The RAF change was reverted to keep avrora green;
the cleaner/finalizer GC-safety hardening (independently correct) was kept.
