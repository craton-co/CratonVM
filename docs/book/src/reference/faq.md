# FAQ

### What is CratonVM, in one sentence?

A Java Virtual Machine written entirely in Rust, with a custom x86-64 JIT
compiler, that runs Java SE 8-25 bytecode with or without a JDK installed.

### Do I need a JDK installed to run it?

The foundational primary target of CratonVM is running authentic OpenJDK class
libraries directly, treating real JDK bytecode as authoritative. By default,
the `cratonvm` launcher runs in JDK-only mode against an authentic OpenJDK
installation (such as JDK 21 or 25). Compatible mode (`--compatible`) and
synthetic stubs provide an optimization layer for fast cold-start loading and
specialized high-throughput paths. In hermetic synthetic mode (`--synthetic-jdk`),
CratonVM can run completely standalone without any host JDK, `JAVA_HOME`, or
`rt.jar`. You compile `.java` to `.class` with standard `javac`, and CratonVM
executes the resulting bytecode. See [JDK Modes](../getting-started/jdk-modes.md).

### Is it production-ready?

CratonVM is research-grade software. It is not certified, has not had a formal
security audit, and must not run untrusted code in security-sensitive
environments. See the [Security Overview](../security/overview.md) and
[Known Limitations](../java-support/limitations.md).

### How fast is it?

With tiered compilation, SSA IR optimization, AVX2 SIMD auto-vectorization, and
On-Stack Replacement (OSR) active by default, compute-intensive kernels
(arithmetic, prime sieves, dense matrix multiplication) run within ~2–4x of
HotSpot C2. Microbenchmarks dominated by deep recursion or extreme allocation
churn are bounded by call-frame overhead and GC throughput. See
[Benchmarks](../performance/benchmarks.md).

### Why is my allocation-heavy program slow, or seemingly hung?

Two likely causes: (1) the heap is too small and the collector is thrashing;
(2) GC throughput on short-lived allocation is a known weak spot. Raise `-Xmx`
explicitly, especially inside containers where ergonomic defaults may not match
the actual workload. See [Memory & GC](../user-guide/memory-and-gc.md) and
[Containers](../user-guide/containers.md).

### How do I run a JAR?

`cratonvm --jar app.jar [args...]`. The main class comes from the JAR manifest;
`--classpath` is ignored in this mode. There is no main-class auto-detection from
a bare classpath; name the class explicitly otherwise.

### I get "native method not found" / `NoSuchMethodError`. Why?

Your program uses a standard-library method CratonVM has not implemented yet.
Audit exactly what is missing with `--XX:AuditMissingNatives` or dump it to JSON
with `--dump-missing-natives`. See [Debugging & Diagnostics](../user-guide/debugging.md).

### Does it do TLS / HTTPS?

Not as a full JSSE stack. There is no backed `SSLContext`/`SSLEngine`/
`HttpsURLConnection`. Terminate TLS in front of the VM (reverse proxy or
sidecar). The cryptographic primitives (digests, AES-GCM, HMAC, RSA, DSA,
ECDSA/Ed25519, PBKDF2, ML-KEM/ML-DSA) are available; see
[Cryptography](../security/cryptography.md).

### What platforms are supported?

Linux and Windows are the primary targets; macOS is best-effort. The JIT is
x86-64 only, and the interpreter runs everywhere. See the [Platform Support
Matrix](platform-support.md).

### Can I embed it in my own application?

Yes, from Rust via `cratonvm-embed`, or from any FFI language via the
`libcratonvm` C ABI, which also exposes the JNI Invocation API. Drive a VM from
the thread that created it; one VM per process is the tested configuration. See
[Embedding](../embedding/overview.md).

### How do I turn the JIT off?

`--nojit` or `CRATONVM_DISABLE_JIT=1`. This is the fastest way to tell whether a
wrong result or hang is an interpreter or JIT issue.

### How do I report a bug?

File on the GitHub issue tracker with a minimal Java reproduction, the exact
command line, expected-vs-actual output, and whether it reproduces under
`--nojit` / `--synthetic-jdk`. Report security issues privately per the
repository's `SECURITY.md`. See [Troubleshooting](../user-guide/troubleshooting.md).

### What license is it under?

Apache License 2.0.
