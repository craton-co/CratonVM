# Known Limitations

CratonVM is research-grade software with broad but incomplete coverage. This
page is the honest list of what is missing, partial, or behaves differently from
a certified JDK. Where a limitation has security implications, it is also
covered in the [Security](../security/overview.md) chapters.

## Standard library

- **TLS / SSL is not supported as a full JSSE stack.** `SSLContext`,
  `SSLEngine`, `KeyManagerFactory`, `TrustManagerFactory`, and
  `HttpsURLConnection` are not backed. Terminate TLS in front of the VM (a
  reverse proxy or service-mesh sidecar). See [Cryptography](../security/cryptography.md).
- **No `java.sql` / JDBC** — there is no database connectivity.
- **AWT / Swing / Java2D are headless.** They are implemented natively with
  in-memory `Graphics2D` rendering and EDT/EventQueue support for common
  invocation, mouse, key, window, and paint events, but no on-screen window
  backend is wired yet. **JavaFX is out of tree** and not a core module.
- **No JAR main-class auto-detection from the classpath** — name the class
  explicitly, or use `--jar` (which reads `Main-Class` from the manifest).

## Reflection & dynamic features

- **Reflection is broad but not exhaustive.** `Class.forName`, `Method.invoke`,
  `Field` access, dynamic proxies, and annotation reflection work; some edge
  cases (certain generic-signature, bridge-method, and nested-class corners) are
  unsupported or partial.

## JNI & native interop

- **JNI is substantial but partial.** The JNI Invocation API, a large function
  table (DefineClass-from-bytes, the `Call*Method` families and their `…V` /
  `…A` variants, global/local references, array-critical with GC pinning) are
  implemented. **Bare C-varargs `(...)` call forms** and **full foreign-thread
  attach** are still partial — see [Embedding](../embedding/overview.md).
- **Project Panama (`java.lang.foreign`)** downcalls/upcalls and library lookups
  are present and gated by `--enable-native-access`, but the foreign linker is
  still maturing.

## Cryptography

- **Best-effort, not constant-time everywhere.** Digests, HMAC, AES/AES-GCM are
  implemented with audited constant-time crates; RSA private-key operations use
  base blinding but are not fully constant-time, and ECDSA scalar paths are
  variable-time. Several algorithms (DH/ECDH key agreement, some KDFs) are
  not supported. The full per-algorithm matrix is in
  [Cryptography](../security/cryptography.md).

## Concurrency

- **`java.util.concurrent` is broad but not complete.** Some constructs (full
  ForkJoin parity, `ReentrantReadWriteLock`, `Phaser`) are still being brought
  to full parity.

## JIT & GC correctness items

- **The JIT targets x86-64 only.** On other architectures the interpreter runs
  everything. An AArch64 backend exists but is partial.
- **One moving-GC follow-up is tracked.** Under the moving collector there is a
  documented residual gap where a JIT worker's published GC-root snapshot can be
  stale relative to its live spill slots at a stop-the-world safepoint. In
  practice the JIT-active path forces the non-moving young sweep with selective
  promotion, so this has not been observed to corrupt the heap, but it is an
  open correctness item. The complete fix (a cross-thread stop-the-world JIT root
  scan) is in progress. See [Security Overview](../security/overview.md).

## Platform

- **macOS is best-effort.** Linux and Windows are the primary CI/development
  targets; some macOS-specific syscall paths (`kqueue`, `FSEvents`) run only when
  built on macOS. See the [Platform Support Matrix](../reference/platform-support.md).
- **Container cgroup limits are detected but not yet wired into heap sizing.**
  Set `-Xmx` explicitly in memory-constrained containers. See
  [Containers & cgroups](../user-guide/containers.md).

## Status & expectations

CratonVM is **not certified** and has **not undergone a formal security audit**.
It must not be used to run untrusted Java code in security-sensitive
environments. The [Roadmap](../contributing/roadmap.md) lists which of these
items are actively being worked on.
