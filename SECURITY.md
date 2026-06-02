# Security Policy

> **WARNING: CratonVM is an experimental Java Virtual Machine implementation.
> It is NOT intended for production use and MUST NOT be used to run untrusted
> Java code in security-sensitive environments.**

## Cryptographic Implementation Status

CratonVM's `javax.crypto.*` / `java.security.*` natives are NOT considered
production-ready as of the 0.3.0 release. Specific limitations:

- **ML-KEM (JEP 496) and ML-DSA (JEP 497)**: post-quantum algorithm
  resolution now throws `NoSuchAlgorithmException`. Earlier prototypes
  returned zero-filled "keys" that decapsulated to constant values.
- **HKDF / PBKDF2WithHmacSHA***: throws `NoSuchAlgorithmException`.
  Earlier prototype used fixed salt/IKM constants and produced the same
  output across every process.
- **AES / AES-GCM**: routed through the `aes`/`aes-gcm` RustCrypto
  crates (constant-time, AES-NI capable). Previous in-tree implementation
  used T-table SBOX lookups (cache-timing oracle).
- **JCA provider chain**: 13 provider names are advertised; only `SUN`,
  `SunJCE`, and `SunRsaSign` are backed by real Service maps. Others —
  including the `SunEC` *provider object* — return null on
  `Provider.getService(...)` lookups by design. Note that EC/ECDSA and
  Ed25519 are nonetheless reachable through the `Signature`,
  `KeyPairGenerator`, and `KeyFactory` `getInstance(...)` natives, which
  short-circuit algorithm resolution rather than consulting the provider
  Service map. See `Provider.getInfo()` for each provider's actual
  coverage and `docs/CRYPTO_STATUS.md` for the per-algorithm matrix.
- **JCA `Signature` API**: RSA PKCS#1 v1.5 (SHA-1/256/384/512), ECDSA
  (`SHA256withECDSA`, `SHA384withECDSA`, P-256/P-384), and `Ed25519` are
  implemented. RSA-PSS, `NONEwithRSA`, and DSA are NOT backed —
  unrecognised algorithms fail closed (`sign()` yields an empty array,
  `verify()` returns `false`).
- **Signed-JAR verification**: handled by a *separate* implementation
  (`classloading/src/jar_signer.rs`) that verifies the PKCS#7 signer
  block, the `.SF` digest binding, the SignerInfo signature, and the
  X.509 chain to a trust anchor. It implements **RSA PKCS#1 v1.5
  (SHA-1/256/384/512) only**; ECDSA and DSA signer blocks are
  *recognised but fail closed* (`SigVerify::Unsupported` /
  `TrustError::NotImplemented`), so ECDSA/DSA-signed JARs are rejected.
  RSA-PSS is not handled. This differs from the JCA `Signature` API
  above, which the JAR signer does not share code with.
- **TLS (SunJSSE / SunJSSL)**: TLS endpoints are NOT supported. Use the
  process's external TLS terminator (nginx, Envoy) instead.

We recommend running CratonVM behind a process boundary that handles
key management and TLS termination via mature implementations. For
research and benchmarking, the in-tree crypto is adequate.

See [`docs/CRYPTO_STATUS.md`](docs/CRYPTO_STATUS.md) for a detailed
per-algorithm implementation matrix.

## Scope

CratonVM is a research and learning project. It has not undergone a security
audit and makes no guarantees about isolation, sandboxing, or resistance to
adversarial input.

## Known Limitations

- The bytecode verifier does not implement full type inference for pre-Java 7 class files
- Native method implementations may not enforce all JVM specification security constraints
- The JIT compiler uses executable memory mappings. These enforce **W^X (write
  xor execute)** on every platform — no page is ever simultaneously writable and
  executable. Pages are allocated read/write, code is emitted, then the region is
  flipped to read/execute: Windows allocates `PAGE_READWRITE` via `VirtualAlloc`
  then flips to `PAGE_EXECUTE_READ` with `VirtualProtect`; Linux/FreeBSD `mmap`
  `PROT_READ|PROT_WRITE` then `mprotect` to `PROT_READ|PROT_EXEC` (flushing the
  I-cache via `__clear_cache` on aarch64 first); macOS-arm64 uses `MAP_JIT` and
  `sys_icache_invalidate` before the RW→RX flip. See `jit/src/platform.rs`.
- **Partial Security Manager support (not a sandbox).** `java.lang.SecurityManager`,
  `java.security.AccessController`, `AccessControlContext`, and `Policy` have native
  implementations (`native-builtins/src/security_manager.rs`). `System.setSecurityManager`
  installs a process-wide singleton; `checkPermission` and the convenience checks
  (`checkRead`/`checkWrite`/`checkConnect`/`checkExec`/`checkDelete`/`checkExit`/
  `checkPropertyAccess`/`checkCreateClassLoader`) route through a policy evaluator, and
  `AccessController.doPrivileged` genuinely runs the action while tracking the caller's
  code source on a per-thread privileged-frame stack. Enforcement is **policy-gated and
  off by default**: with no `java.policy` loaded the evaluator is allow-all (matching the
  JDK default for a programmatically-installed SecurityManager without a policy); it only
  denies once a policy is parsed via `load_policy_file` / `set_active_policy`. Several
  `checkAccess` variants are unconditional allow. `ProcessBuilder.start` / `Runtime.exec*`
  consult the installed SecurityManager via `checkExec(command[0])` before any spawn
  syscall. The Panama / FFI downcall path is **not** gated by the SecurityManager (a
  documented follow-up), so native access bypasses these checks. This is **not** a
  sandbox for untrusted code — stack-based access control is not enforced, no
  `java.policy` is loaded by default, and the SecurityManager surface is deprecated for
  removal (JEP 411).
- No sandboxing of loaded Java classes
- **Incomplete signed-JAR trust.** The signed-JAR verifier
  (`classloading/src/jar_signer.rs`) implements RSA PKCS#1 v1.5 only. ECDSA and
  DSA signer blocks, RSA-PSS, and full RFC 5280 certification-path validation are
  **NOT** implemented. Signed-jar trust currently **fails closed**: a signed jar
  whose signature cannot be verified by the supported path is rejected, but the
  VM cannot cryptographically *establish trust* in the general case. Do not rely
  on jar signatures as a trust boundary.
- **Known JIT crash (under investigation).** A JIT-related native crash is
  tracked in [`docs/real-raf-segv-root-cause.md`](docs/real-raf-segv-root-cause.md).
  Two distinct root causes documented there have been fixed; the area remains
  under active investigation and is disclosed here as a known issue rather than
  hidden. Use `--nojit` to run the interpreter only if you need to avoid the JIT
  entirely.

## Hardening Measures

- **Checked arithmetic** is used throughout the GC (heap pointer calculations,
  object size computations) and JIT compiler (offset calculations, code buffer
  sizing) to prevent overflow-related memory corruption.
- All heap allocations in the GC are bounds-checked against the configured
  heap limit before writing.

## Configuration Hardening

When running CratonVM in any context where the input is not fully trusted:

1. **Always enable bytecode verification** — do not use `--noverify`, which
   disables structural and type checks on loaded class files.
2. **Limit heap size** — use `--Xmx` to cap memory consumption (e.g., `--Xmx 256m`).
   Without a limit the GC will attempt to grow the heap until the OS refuses.
3. **Restrict the classpath** — only include directories and JARs you control.
   CratonVM will load any `.class` file found on the classpath.
4. **Disable JIT for untrusted code** — the JIT compiler (`--nojit`) can be
   disabled to reduce the attack surface. The interpreter is simpler and has
   fewer unsafe code paths.
5. **Run with minimal OS privileges** — CratonVM does not drop privileges
   itself. Use OS-level sandboxing (containers, seccomp, AppArmor) for
   defense in depth.

## Unsafe Code Inventory

CratonVM uses `unsafe` Rust in the following subsystems:

| Subsystem | Purpose | Mitigations |
|-----------|---------|-------------|
| GC (arena, heap) | Pointer arithmetic for object layout | Checked arithmetic, bounds checks, alignment assertions |
| JIT (x64, aarch64) | Executable memory mapping and code emission | W^X via `mprotect`/`VirtualProtect`, buffer bounds checks |
| ObjectRef | Send/Sync for heap pointers | Single-threaded execution model; monitor protocol for future threading |
| FFI (native-api) | Calling native libraries via `libloading` | Library paths restricted to classpath |
| native-io | Raw syscalls / FFI for file, socket, NIO channel, and selector I/O | FD-table ownership, length/bounds checks on buffers |
| native-awt | Platform windowing/Java2D peers (Win32, Cocoa, X11) | Headless by default; peer handles validated before use |
| cuda-bridge | CUDA Driver API FFI for optional GPU offload | Opt-in Cargo feature; not built into the default CPU-only binary |
| types | `ObjectRef`/`Value` representation and transmutes | Layout assertions; constructed only by the VM runtime |
| jfr | Java Flight Recorder buffer encoding | Bounded ring buffers, length-checked writes |

`unsafe` blocks in the core subsystems (GC, JIT, and `ObjectRef`) are documented
with `// SAFETY:` comments describing the relied-upon invariant. Full
`// SAFETY:` coverage across all subsystems above is still in progress — the
peripheral subsystems (native-io, native-awt, cuda-bridge, jfr) are not yet
fully annotated.

## Reporting a Vulnerability

For **non-sensitive** issues, open a
[GitHub Issue](https://github.com/craton-co/cratonvm/issues) with the label `security`.

For **sensitive** issues (exploitable vulnerabilities, crashes on untrusted input),
please report privately either via
[GitHub Security Advisories](https://github.com/craton-co/cratonvm/security/advisories/new)
(preferred) or by emailing `security@craton.co`. Either channel keeps the issue
private and ensures it is not publicly visible until a fix is available.

In your report, please include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact

We will acknowledge receipt and aim to provide an initial response within 7 days.

## Supported Versions

| Version | Supported           |
|---------|---------------------|
| 0.3.x   | Yes (current)       |
| 0.2.x   | End-of-life         |
| 0.1.x   | End-of-life         |
