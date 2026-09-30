# Platform Support Matrix

Most of CratonVM is platform-neutral Rust. The divergent surface is the
syscall-touching I/O and process code. This page records which features are
**Full**, **Partial**, or **Stub** on each host OS.

Status legend:

- **Full** — fully implemented against the native OS API.
- **Partial** — works for common cases; specific limitations noted.
- **Stub** — registered as a safe no-op or `UnsupportedOperationException`; it
  can be called but does not perform the operation.

## I/O and NIO

| Feature | Linux | Windows | macOS |
|---------|-------|---------|-------|
| File I/O (`FileInputStream`, `FileOutputStream`, `RandomAccessFile`) | Full | Full | Full |
| `FileChannel.map` (mmap) | Full | Full | Full |
| `FileChannel.transferTo` | Full (`sendfile` + `copy_file_range` fast path) | Partial (userspace loop) | Partial (userspace loop) |
| `FileChannel.lock` (blocking) | Partial (non-blocking only) | Partial (non-blocking only) | Partial (non-blocking only) |
| `FileChannel.tryLock` | Full | Full | Full |
| `AsynchronousFileChannel` | Full | Full | Full |
| `Pipe.open` | Full | Full | Full |
| `Selector` / `SelectionKey` | Full (`epoll`) | Full (`WSAPoll`) | Partial (`kqueue`, rarely exercised in CI) |
| `SocketChannel` / `ServerSocketChannel` (blocking + non-blocking) | Full | Full | Full |
| `AsynchronousSocketChannel` / `…ServerSocketChannel` | Partial (worker-pool backed; no native IOCP/epoll-port integration) | Partial (same) | Partial (same) |
| `DatagramChannel` (UDP) | Full | Full | Full |
| Multicast (join/leave/loopback) | Full | Full | Full |
| Source-specific multicast block (IGMPv3) | Stub (Java-side filter only) | Stub | Stub |
| `WatchService` | Full (`inotify`) | Full (`ReadDirectoryChangesW`) | Full (`FSEvents`) |
| `Process` / `ProcessBuilder` spawn | Full | Full | Partial (generic path; no `posix_spawn` fast path) |
| `SO_KEEPALIVE` setter | Stub (no-op) | Stub | Stub |
| Scatter-gather `readv`/`writev` | Stub (JDK falls back to a per-buffer loop) | Stub | Stub |

## Zip / JAR

Real-mode JAR and ZIP reads are supported on all three platforms, with the
decompression-bomb guard (per-entry inflated cap, default 512 MiB, plus a 1000:1
ratio cap). See [Sandboxing & Hardening](../security/sandboxing.md).

## TLS sockets

`SSLSocket` is partial and does not match a full JSSE matrix. There is no full
`SSLContext`/`SSLEngine` stack — terminate TLS in front of the VM. See
[Cryptography](../security/cryptography.md).

## NUMA awareness

- **Linux:** detected via `numactl` and `/sys/devices/system/node/`.
- **Windows:** stub — every host reports a single node.
- **macOS:** not applicable.

## GPU offload

CUDA-backed, available only on hosts with a CUDA driver, and **off by default in
every build**. See [GPU Offload](../gpu/overview.md).

## The JIT

The JIT targets **x86-64 only**. An AArch64 backend exists but is partial. On any
other architecture the JIT is automatically disabled and the interpreter runs
everything. The interpreter, classloader, GC, and JFR are platform-neutral.

## CI coverage by platform

- **Linux** is the primary CI target; the `epoll`/`inotify`/`sendfile`/
  `copy_file_range` paths run on every push.
- **Windows** is a principal development platform; the `WSAPoll`,
  `ReadDirectoryChangesW`, `CreateProcess`, `CreatePipe`, and Win32 handle-table
  paths run in CI.
- **macOS** kernel-specific paths (`kqueue`, `FSEvents`) run only when built on
  macOS, which CI does not currently include — treat macOS as **best-effort**.
