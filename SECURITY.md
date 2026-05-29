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
  `SunJCE`, and `SunRsaSign` are backed by real Service maps. Others
  return null on `Provider.getService(...)` lookups by design — see
  `Provider.getInfo()` for each provider's actual coverage.
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
- The JIT compiler uses executable memory mappings (`mmap`/`VirtualAlloc` with `RWX` permissions)
- No Security Manager implementation
- No sandboxing of loaded Java classes

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

All `unsafe` blocks carry `// SAFETY:` comments documenting the relied-upon invariant.

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
