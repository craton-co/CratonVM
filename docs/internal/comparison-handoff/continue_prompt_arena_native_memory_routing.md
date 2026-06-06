# Harden Unsafe-arena native-memory routing (silent-corruption R1 + FileDispatcher SIGSEGV R2)

**Severity:** high (R1 = silent data corruption; R2 = the same SIGSEGV the NIO socket fix already hit, still live on the file-channel path). Self-contained. Baseline commit: `d6cefc7` on `dev`.

## Background
`Unsafe.allocateMemory` in CratonVM does NOT return real OS pointers — it returns **synthetic handles** from an off-heap arena store (`native-builtins/src/lib.rs`, `unsafe_arena_allocate` → `ArenaStore`, base `0x10_0000_0000` = 2^36), backed by a `HashMap<base, Vec<u8>>`. `Unsafe.put/getByte` translate the handle to the backing `Vec`. JDK code like `sun.nio.ch.Util.getTemporaryDirectBuffer` does `addr = Unsafe.allocateMemory(size); NIO_ACCESS.newDirectByteBuffer(addr, size)`, so the resulting `DirectByteBuffer.address()` is one of these handles.

A recent fix (commit `d6cefc7`) added `NativeContext::copy_from_native_memory(&self, addr, &mut [u8])` / `copy_to_native_memory(&mut self, addr, &[u8])` (declared in `native-api/src/registry.rs`, overridden in `vm/src/vm/vm_exec.rs`) which routes an `addr` that `cratonvm_native_builtins::unsafe_arena_contains(addr)` reports as in-arena through `unsafe_arena_copy_out/copy_in`, else does a raw `copy_nonoverlapping`. `net_read0`/`net_write0` in `native-io/src/net.rs` now use these. That fixed the socket write SIGSEGV. Two issues remain.

## R1 — arena/real-pointer aliasing → silent corruption (highest value)
`unsafe_arena_contains(addr)` (`native-builtins/src/lib.rs`) is `ArenaStore::contains` = `locate(addr).is_some()`, i.e. **range membership** over each live block `[base, base+len)`. Arena bases start at `0x10_0000_0000` (64 GiB) and climb. Windows x64 user address space is 128 TiB, so a **real** OS pointer can be ≥ 64 GiB.

The dangerous case: a *real* `DirectByteBuffer` allocated via `ByteBuffer.allocateDirect` stores a **real** pointer from `dbb_allocate` (`native-io/src/direct_buffer.rs`), NOT an arena handle. If that real pointer numerically falls inside a live arena block's range, `copy_from/to_native_memory` mis-routes it to the arena → reads/writes the wrong bytes (silent corruption, not a crash). It is reachable, e.g. `SocketChannel.write(directBuffer)` → `IOUtil.write` (buffer already direct) → `Net.write0`/`SocketDispatcher.write0` → `net_write0` with a real pointer.

**Fix options (pick the robust one):**
1. Make arena handles **provably disjoint** from real pointers — e.g. OR a reserved high tag bit into the handle in `ArenaStore::allocate` (`lib.rs` ~`next_addr`), strip it in `locate`/`copy_*`/`get/put_byte`, so `contains` can test the tag bit instead of (or in addition to) range membership. Verify `Unsafe.get/putByte`, `copyMemory`, `freeMemory`, `reallocateMemory`, and `DirectByteBuffer.address()` consumers all round-trip the tagged value.
2. OR have `copy_from/to_native_memory` first check whether `addr` is a known live **real** allocation (the `dbb_allocate` / `record_unsafe_alloc` tables) and prefer the raw path for those; only fall to the arena for un-tracked handles.
Confirm `dbb_allocate`'s real pointers can never be mistaken for handles after the fix.

## R2 — same SIGSEGV unpatched on the file-channel path
`sun/nio/ch/FileDispatcherImpl.read0/write0/pread0/pwrite0` (`native-io/src/nio_native.rs`, ~lines 124/153/173/197, registered ~774-778) still do raw `std::ptr::copy_nonoverlapping` on a guest `addr`. A `FileChannel.write(heapBuffer)` goes (real JDK) through `IOUtil.write` → `Util.getTemporaryDirectBuffer` (**arena handle**) → `FileDispatcherImpl.write0` → raw memcpy → the **identical SIGSEGV** the socket path had. Fix: route these through `ctx.copy_from/to_native_memory` exactly like `net_read0`/`net_write0` now do. Also **audit** `native-io/src/socket_channel.rs` (~530/562) and `native-io/src/async_socket.rs` (~416/1028): per review they only handle genuine `BufferAccess::Direct` (real-pointer) buffers and route heap buffers through a non-memcpy branch, so they may be self-consistent — confirm they never see an arena handle, and convert if they can.

## Plan
1. Implement R1 (handle tagging or real-alloc-preference) in `native-builtins/src/lib.rs` arena + `vm/src/vm/vm_exec.rs` overrides; keep `unsafe_arena_contains` exact.
2. Implement R2: `FileDispatcherImpl.read0/write0/pread0/pwrite0` → `ctx.copy_*`. Audit the two channel files.
3. Add a Rust unit test for R1 (allocate an arena block, then assert a synthesized real-looking pointer in the same numeric range is NOT mis-routed).

## Verification
- `FileChannel` write+read of a `HeapByteBuffer` (forces a temp direct buffer) does not SIGSEGV and round-trips bytes. (See also `continue_prompt_nio_regression_coverage.md` for a probe.)
- A direct buffer from `allocateDirect` used with `SocketChannel.write` (gate `CRATONVM_REAL_NET_SOCKETS=1`) writes correct bytes (no corruption).
- `target/release/cratonvm.exe` socket round-trip still PASSES: `CRATONVM_REAL_NET_SOCKETS=1 cratonvm.exe -cp C:/tmp/audit SockProbe` → PASS.
- Regression pool stays 13/14.

## Key files
`native-builtins/src/lib.rs` (ArenaStore: `allocate`/`locate`/`contains`/`copy_out`/`copy_in`, `unsafe_arena_*`). `vm/src/vm/vm_exec.rs` (`copy_from_native_memory`/`copy_to_native_memory` overrides). `native-api/src/registry.rs` (trait defaults). `native-io/src/net.rs` (`net_read0`/`net_write0` — reference usage). `native-io/src/nio_native.rs` (FileDispatcherImpl). `native-io/src/direct_buffer.rs` (`dbb_allocate`, real-pointer alloc). Memory: `reference_server_socket_gap`.

## Build/test gotchas (Windows)
Before each rebuild: `taskkill //F //IM cratonvm.exe`, `taskkill //F //IM cargo.exe`, `taskkill //F //IM rustc.exe`; `rm -f target/release/cratonvm.exe` to force a relink; verify the exe mtime advanced. Run ONE build at a time. See memory `reference_windows_exe_lock_build_trap`.
