# Platform Support Matrix

Audience: developers who need to know which syscall-touching features work
on which host OS. The bulk of CratonVM is platform-neutral Rust; the
divergent surface lives in [`native-io/`](../native-io/src/) and a few spots
in [`native-builtins/`](../native-builtins/src/).

Status legend:

- **Full** — fully implemented against the native OS API.
- **Partial** — works for common cases; specific limitations noted.
- **Stub** — registered with a no-op or `UnsupportedOperationException`
  return; safe to call but does not perform the operation.

## I/O and NIO

| Feature | Linux | Windows | macOS | Source |
|---|---|---|---|---|
| File I/O (`FileInputStream`, `FileOutputStream`, `RandomAccessFile`) | Full | Full | Full | [`lib.rs`](../native-io/src/lib.rs), [`random_access_file.rs`](../native-io/src/random_access_file.rs) |
| `FileChannel.map` (mmap) | Full (`memmap2`) | Full (`memmap2`) | Full (`memmap2`) | [`file_channel.rs`](../native-io/src/file_channel.rs) |
| `FileChannel.transferTo` | Full (`sendfile` + `copy_file_range` fast path) | Partial (userspace loop) | Partial (userspace loop) | [`file_channel.rs:553`](../native-io/src/file_channel.rs) |
| `FileChannel.lock` (blocking) | Partial — non-blocking only today (`TODO(round-8)` at [`lib.rs:10522`](../native-io/src/lib.rs)) | Partial — non-blocking only | Partial — non-blocking only | [`lib.rs:10522`](../native-io/src/lib.rs) |
| `FileChannel.tryLock` | Full | Full | Full | [`lib.rs`](../native-io/src/lib.rs) |
| `AsynchronousFileChannel` | Full | Full | Full | [`async_socket.rs`](../native-io/src/async_socket.rs), [`lib.rs`](../native-io/src/lib.rs) |
| `Pipe.open` | Full (`libc::pipe`) | Full (`CreatePipe`) | Full (`libc::pipe`) | [`pipe.rs`](../native-io/src/pipe.rs) |
| `Selector` / `SelectionKey` | Full (`epoll`) | Full (`WSAPoll`) | Partial (`kqueue` path is target-gated; CI rarely exercises it) | [`nio_selector.rs`](../native-io/src/nio_selector.rs) |
| `SocketChannel` / `ServerSocketChannel` (blocking + non-blocking) | Full | Full | Full | [`socket_channel.rs`](../native-io/src/socket_channel.rs) |
| `AsynchronousSocketChannel` / `…ServerSocketChannel` | Partial — connect/read/write via crossbeam-backed worker pool; IOCP / `EPollPort` / `KQueuePort` are stub facades ([`async_socket.rs:1083`](../native-io/src/async_socket.rs)) | Partial — same; no real IOCP integration | Partial — same | [`async_socket.rs`](../native-io/src/async_socket.rs) |
| `DatagramChannel` (UDP) | Full | Full | Full | [`datagram.rs`](../native-io/src/datagram.rs) |
| Multicast (join/leave/loopback) | Full | Full | Full | [`datagram.rs`](../native-io/src/datagram.rs), [`net.rs`](../native-io/src/net.rs) |
| Source-specific multicast block (IGMPv3) | Stub — Java-side filter only, no `IP_BLOCK_SOURCE` setsockopt ([`datagram.rs:514`](../native-io/src/datagram.rs)) | Stub | Stub | [`datagram.rs:514`](../native-io/src/datagram.rs) |
| TLS sockets (`SSLSocket`) | Partial — always compiled in [`native-builtins/`](../native-builtins/src/), not feature-gated; does not match HotSpot's full JSSE matrix | Partial | Partial | `native-builtins` |
| `AnonymousFileChannel` (memory-only `FileChannel`) | Full | Full | Full | [`lib.rs`](../native-io/src/lib.rs) |
| `WatchService` | Full (`inotify` via `notify` crate) | Full (`ReadDirectoryChangesW`) | Full (`FSEvents`) | [`watch.rs`](../native-io/src/watch.rs) |
| `Process` / `ProcessBuilder` spawn | Full (Linux-specific `native_unix_fork_and_exec` path) | Full (`CreateProcess`) | Partial — uses the generic `std::process` path; no `posix_spawn` fast path | [`process.rs`](../native-io/src/process.rs) |
| `SO_KEEPALIVE` setter | Stub (no-op without the `socket2` crate; documented at [`net.rs:644`](../native-io/src/net.rs)) | Stub | Stub | [`net.rs:644`](../native-io/src/net.rs) |
| `readv0` / `writev0` (scatter-gather) | Stub (returns `IOException("unsupported")`; JDK falls back to per-buffer loop) | Stub | Stub | [`nio_native.rs:289`](../native-io/src/nio_native.rs) |
| `setDirect0` (direct-buffer hint) | Stub (returns `-1`; JDK treats as hint) | Stub | Stub | [`nio_native.rs:299`](../native-io/src/nio_native.rs) |

## Zip / Jar

- Real-mode JAR and ZIP reads via the `zip` 2.x crate
  ([`zip_real_jar.rs`](../native-io/src/zip_real_jar.rs)). Decompression-bomb
  guard: per-entry inflated cap defaults to 512 MiB, 1000:1 compression ratio
  cap. Same behaviour on all three platforms.

## NUMA awareness

- **Linux**: detected via `numactl` paths and `/sys/devices/system/node/`.
- **Windows**: stub — every host reports as a single node regardless of
  physical topology ([`gc/src/numa.rs:334`](../gc/src/numa.rs) TODO).
- **macOS**: not applicable.

## GPU offload (`gpu` / `gpu-driver` Cargo features)

Opt-in and off by default in every build — a plain `cargo build` never links
CUDA. Two `cratonvm-cli` features layer on top of each other:

- `gpu` — exposes the `--gpu*` CLI flags and links `cuda-bridge` in its stub
  backend (device probing always returns `DeviceError::NoDriver`; the CLI
  surface exists but nothing offloads).
- `gpu-driver` — implies `gpu`, plus the real CUDA Driver API bindings via
  the `cudarc` 0.13 crate (`driver`, `cuda-12060` features), dynamically
  loading `nvcuda.dll` (Windows) / `libcuda.so` (Linux). This is the feature
  that actually offloads work to a GPU.

Internally `cratonvm-cli`'s `gpu` feature maps to `cratonvm-vm`'s
`gpu-offload` feature (`vm/Cargo.toml`), which pulls in `cuda-bridge` and
`jit-cuda`.

| Feature | Linux | Windows | macOS | Source |
|---|---|---|---|---|
| GPU offload (CUDA, NVIDIA-only) | Full — same driver-API path as Windows; not exercised as heavily as the Windows dev box | Full — validated on real hardware: Windows 11 + RTX 2060 (sm_75) | Not supported — CUDA is NVIDIA-only and NVIDIA ships no CUDA driver for macOS | [`cuda-bridge/`](../cuda-bridge/src/), [`jit-cuda/`](../jit-cuda/src/), [`vm-cli/src/main.rs`](../vm-cli/src/main.rs) |

No AMD/ROCm or Intel/oneAPI backend exists or is planned for Phase 1/2; "GPU
offload" in CratonVM documentation always means CUDA. See
[`docs/gpu/annotations.md`](gpu/annotations.md) for the annotation surface and
[`docs/gpu/cuda-oxide-evaluation.md`](gpu/cuda-oxide-evaluation.md) for why
the `cuda-oxide` crate is not on the critical path.

## Runtime flags and environment variables affecting platform behaviour

| Variable / Flag | Effect |
|---|---|
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | Overrides the 512 MiB per-entry inflated cap in [`zip_real_jar.rs`](../native-io/src/zip_real_jar.rs). Read once on first jar/zip access. |
| `CRATONVM_SUREFIRE_IPC_DBG` | Verbose diagnostic logging for Surefire's IPC channel (read in [`socket_channel.rs:52`](../native-io/src/socket_channel.rs)). |
| `CRATONVM_DBG_JETTY` | Extra trace output for Jetty bring-up paths in [`lib.rs`](../native-io/src/lib.rs). |
| `CRATONVM_JAVA_HOME` | Override `JAVA_HOME` when a launcher (Maven, Gradle) points at a CratonVM shim tree but boot modules must come from a real JDK. |
| `JAVA_HOME` | Standard JDK installation path. |
| `set_path_confine_to_cwd(true)` (API) | Confines canonical path resolution to the process CWD. **Off by default** — CratonVM is not a sandbox. Multi-tenant embedders MUST enable this at startup. See [`lib.rs:155`](../native-io/src/lib.rs). |
| `add_sandbox_root(path)` (API) | Adds trusted directories beyond CWD (e.g. `$JBOSS_HOME`). Used by `SharedVm::new` for classpath / `--java-home` / `-D*.home` roots. |
| `--XX:-UseContainerSupport` | Disables cgroup v1/v2 auto-sizing of heap and thread pools inside Docker/Kubernetes. Linux-only; ignored elsewhere. |

## Bytecode / VM features unaffected by platform

The interpreter ([`vm/src/runtime/interpreter.rs`](../vm/src/runtime/interpreter.rs)),
classloader, GC ([`gc/src/`](../gc/src/)), and JFR
([`jfr/src/`](../jfr/src/)) crates are platform-neutral. The JIT
([`jit/src/`](../jit/src/)) compiles on x86-64. On AArch64 a separate, much
smaller backend (`jit/src/aarch64_backend.rs`) exists but is **off by
default**: set `CRATONVM_JIT_ARM64=1` (or `CRATONVM_JIT=arm64`) to enable it.
It compiles only leaf methods -- arithmetic, locals, conversions, compares,
branches and switches, with no call, field, array, allocation, monitor or
exception -- and has not been proven on hardware; see
[`jit/aarch64-parity.md`](jit/aarch64-parity.md). On every other architecture,
and on AArch64 without that switch, the JIT is disabled (interpreter-only).

## Test coverage by platform

- **Linux** is the primary CI target; the `epoll`/`inotify`/`sendfile`/
  `copy_file_range`/`posix_spawn`-equivalent paths are exercised on every
  push.
- **Windows** is the principal development platform; the `WSAPoll`,
  `ReadDirectoryChangesW`, `CreateProcess`, `CreatePipe`, and Win32
  handle-table paths run in CI.
- **macOS** kernel-specific paths (`kqueue`, `FSEvents`) are only run when
  the test binary is built on Darwin. CI does not currently include a
  macOS runner; treat macOS as best-effort.
- **GPU offload** is not part of CI (no CUDA-capable CI runner). It was
  manually validated on real hardware — Windows 11 + RTX 2060 (sm_75).

## Further reading

- Crate-specific module docs: every `native-io/src/*.rs` file opens with a
  `//!` block describing its registry shape and platform backing.
- Embedding guide: [`docs/EMBEDDING.md`](EMBEDDING.md).
- GC tuning: [`docs/gc-tuning.md`](gc-tuning.md).
- Top-level configuration: [`docs/CONFIG.md`](CONFIG.md).
