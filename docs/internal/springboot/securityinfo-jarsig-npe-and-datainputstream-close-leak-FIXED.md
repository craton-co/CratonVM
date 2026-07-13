# `SecurityInfoTests`/`NestedJarFileTests`: `Providers.<clinit>` NPE + `DataInputStream.close()` no-op — FIXED (2 of 3 bugs); PKCS7 verification failure OPEN

**Status:** 2 root causes FIXED. A third, distinct, deeper crypto-correctness bug remains OPEN.

Investigated while following up on the `zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`
residuals list, which named `SecurityInfoTests`/`NestedJarFileTests` (signed-jar cases) as an
unexplored cluster: jar-signature verification failing, plus a `bcprov-jdk18on-1.78.1.jar` file
handle left open past test teardown.

These turned out to be **three separate root causes**, not one:

1. `sun.security.jca.Providers.<clinit>` no-op'd → `NullPointerException` in
   `Providers.startJarVerification()`/`stopJarVerification()` — **FIXED**.
2. `java.io.DataInputStream.close()` registered as an unconditional no-op → file-handle leak —
   **FIXED**.
3. Real `sun.security.util.SignatureFileVerifier`/`PKCS7.verify()` rejects the real BouncyCastle
   RSA signature over `META-INF/BC2048KE` (`SecurityException: cannot verify signature block
   file`) — **OPEN**, not investigated further (see below).

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot apps\spring-boot `
  -Exe <cratonvm exe> -Category all -Start 14 -Count 2 -RunName repro
```
(`Start 14`/`Count 2` picks `loader/spring-boot-loader`'s `NestedJarFileTests` and
`SecurityInfoTests` from the default class list — use `-RefreshLists -ListOnly` to find the
current index if the list has changed.) Or target the classes directly by name via `-ClassList`.

Before the fixes: `SecurityInfoTests` 1/3 pass, `NestedJarFileTests`'s `verifySignedJar` fails —
both with a `NullPointerException` from `Providers` AND a suppressed `AssertionError` (open
`FileDataBlock` path) from `AssertFileChannelDataBlocksClosedExtension`.

After fixes 1+2: `SecurityInfoTests` 2/3 pass (only `getWhenJarIsSigned` still fails, now with
*only* the `SecurityException` — no suppressed leak assertion). `NestedJarFileTests`: same pattern
— `verifySignedJar` still fails on the `SecurityException` alone, no leak. The other 4
`NestedJarFileTests` failures (`getCommentAlignsWithJdkJar`, `getEntryWhenMultiReleaseEntryReturnsEntry`,
`versionedStreamStreamsEntries`, `createOpensJar`) are unrelated pre-existing bugs (ZipEntry comment
parsing, multi-release entry resolution, directory-entry counting) — out of scope here, not
investigated.

## Bug 1 — `Providers.<clinit>` no-op breaks jar-signature verification (FIXED)

### Root cause

`native-builtins/src/jca/cipher.rs::register_cipher_clinit_shim` no-ops `sun/security/jca/Providers
.<clinit>` (and `ProviderList.<clinit>`) as part of a chain of `<clinit>` bypasses built for
`javax.crypto.Cipher`/`KeyGenerator` bring-up (`Security`/`Provider`/`JceSecurity`/`Debug`/etc. —
see the extensive comments already in that file for why each one is no-op'd). The real
`Providers.<clinit>` sets two static fields this bypass never anticipated a second consumer for:

- `providerList` ← `ProviderList.fromSecurityProperties()`
- `threadLists` ← `new ThreadLocal<>()`

Both stay **null**. `sun.security.util.SignatureFileVerifier.<init>` (real jar-signature
verification — reached via `java.util.jar.JarEntry.getCertificates()`/`JarInputStream` /
`JarVerifier`, which Spring Boot loader's `SecurityInfo.load()` drives directly) always wraps its
body in:
```java
Object obj = null;
try {
    obj = Providers.startJarVerification();
    this.cf = CertificateFactory.getInstance("X.509");
} finally {
    Providers.stopJarVerification(obj);
}
```
`startJarVerification()` NPEs dereferencing the null `providerList` (`getSystemProviderList()
.getJarList(...)`); the `finally` block's `stopJarVerification(null)` → `endThreadProviderList(null)`
NPEs calling `.remove()` on the null `threadLists` `ThreadLocal` itself. Since this is a plain
`try/finally` (not try-with-resources), the finally-block's NPE **replaces** the try-block's NPE
(nothing recorded as suppressed) — surfacing as exactly one:
```
java.lang.NullPointerException: Cannot invoke "java.lang.ThreadLocal.remove()" because
"sun.security.jca.Providers.threadLists" is null
    sun.security.jca.Providers.endThreadProviderList(Providers.java:249)
    sun.security.jca.Providers.stopJarVerification(Providers.java:129)
    sun.security.util.SignatureFileVerifier.<init>(SignatureFileVerifier.java:115)
```
Confirmed against the real JDK 25 source (`java.base/sun/security/jca/Providers.java` from
`$JAVA_HOME/lib/src.zip`) — this is not speculative.

### Fix

`native-builtins/src/jca/cipher.rs` — instead of resurrecting `ProviderList
.fromSecurityProperties()`/`threadLists` bring-up (the exact chain the original no-op was written
to dodge), bypass the two entry points `SignatureFileVerifier` actually calls directly:
```rust
r.register("sun/security/jca/Providers", "startJarVerification",
    "()Ljava/lang/Object;", |_ctx, _args| Ok(Some(Value::Object(None))));
r.register("sun/security/jca/Providers", "stopJarVerification",
    "(Ljava/lang/Object;)V", clinit_noop);
```
The real purpose of this thread-local provider swap (stop jar verification from recursively
loading providers out of the jar being verified) is moot under CratonVM: `CertificateFactory`/
`Signature`/`MessageDigest` dispatch is native, not routed through a loaded `ProviderList` at all.

### Verification

Standalone repro (`ZcDebugRepro.java`, outside Spring Boot: `ZipContent.open()` +
`openRawZipData().asInputStream()` wrapped in `JarInputStream`, iterate `getNextJarEntry()`) against
the real `bcprov-jdk18on-1.78.1.jar` confirmed the NPE disappeared and execution proceeded into
real PKCS7 parsing (reaching bug 3 below). `SecurityInfoTests`/`NestedJarFileTests`: NPE gone in
both, replaced by the `getWhenJarIsSigned`/`verifySignedJar`-only `SecurityException` (bug 3).

## Bug 2 — `DataInputStream.close()` no-op → `FileDataBlock` leak (FIXED)

### Root cause

**Not** related to `ZipInputStream`/`InflaterInputStream`/`ZipInflaterInputStream` despite that
being the first (wrong) hypothesis — see "Dead ends" below. The actual leak:
`native-io/src/lib.rs::register_data_stream_natives` registers
`DataInputStream.close()` as `native_noop_void`. `DataInputStream` declares **no bytecode of its
own** for `close()` — real JDK inherits `FilterInputStream.close()` → `in.close()`. Registering a
native directly on `DataInputStream` pre-empts that inherited real bytecode: the interpreter's
dispatch prefers a class's own registered native over walking the hierarchy to find an ancestor's
real implementation once the class itself has no declared method for it — so the no-op ran
instead, **unconditionally**, for every `DataInputStream`, in both real-JDK and synthetic-jdk
modes (constructors — and therefore every `new DataInputStream(...)` — always resolve through
whatever native is registered for `<init>`, bypassing the "prefer real bytecode" check entirely,
so this isn't even mode-dependent).

Spring Boot loader's `SecurityInfo.load()` → `JarEntriesStream.matches()` wraps **every jar
entry's content** in `new DataInputStream(getInputStream(...))` to compare bytes:
```java
try (DataInputStream expected = new DataInputStream(getInputStream(size, streamSupplier))) {
    assertSameContent(expected);
}
```
`getInputStream(...)` opens one `ZipContent.Entry.openContent()` reference per entry (a
`FileDataBlock` slice sharing the parent `ZipContent`'s `FileAccess`/refcount). Since `expected
.close()` (guaranteed by try-with-resources, confirmed separately — see below) never reached past
the `DataInputStream` no-op, that reference was **never released**, for **every non-directory
entry** in the jar. Confirmed via an instrumented `FileDataBlock` (temporarily swapped into
`apps/spring-boot/loader/spring-boot-loader/build/classes/java/main`, stack-traced every
open/close call): 9 opens (one `ZipContent.open` + one `openRawZipData` + one per file entry) vs.
only 2 matching closes (both from the *outer* `entries.close()`/`ZipContent.close()`, not from any
per-entry `DataInputStream`) — leaving the real jar's `FileDataBlock` refcount stuck above zero,
tripping `AssertFileChannelDataBlocksClosedExtension`.

`native-builtins/src/classloader.rs` has a **second, independent** registration for `dis, "close"`
(also a no-op) — dead in practice under real-JDK boot, since `native-io::register_io_natives` is
called later in `vm/src/vm/vm_init.rs`'s bootstrap sequence and wins. Fixed both, for consistency /
in case registration order ever changes — but `native-io`'s copy is the one that actually matters.

`BufferedInputStream.close()` (also no-op'd in `classloader.rs`) was investigated and **left
alone**: unlike `DataInputStream`, `BufferedInputStream` DOES declare its own `close()` in real
bytecode (`bufUpdater.compareAndSet(...) ... input.close()`), so the interpreter correctly prefers
that real implementation regardless of the registered no-op — this native is not reached in
practice. `native-io/src/lib.rs`'s own `register_buffered_stream_natives` has an explicit
"Wave2 H2" comment already documenting a deliberate policy of leaving `BufferedInputStream` to run
on real bytecode; no evidence surfaced here to override that.

### Fix

`native-io/src/lib.rs` — added `native_dis_close`, mirroring the already-correct
`native_dos_close` (`DataOutputStream`'s side, which flushes+closes the underlying `OutputStream`
via `invoke_virtual_declared`) instead of the no-op:
```rust
fn native_dis_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Value::Object(Some(inner)) = ctx.get_field(this, DIS_FIELD_IN) {
        ctx.invoke_virtual_declared("java/io/InputStream", inner, "close", "()V", &[])?;
    }
    Ok(None)
}
```
Registered in place of `native_noop_void` for `dis, "close", "()V"`.

`native-builtins/src/classloader.rs`'s dead-in-practice copy fixed the same way, for consistency.
`InflaterInputStream.close()` (`native-builtins/src/phases_late.rs`) was also fixed from a blanket
no-op to a real propagating implementation while chasing the (wrong) initial hypothesis — kept,
since it's still correct and matters under `--synthetic-jdk` (no real bytecode to fall back to
there).

### Verification

Minimal, Spring-Boot-independent repro (no jar/zip involved at all):
```java
static class TracingIS extends ByteArrayInputStream {
    static int closedCount = 0;
    TracingIS(byte[] b) { super(b); }
    public void close() throws IOException { closedCount++; super.close(); }
}
TracingIS is = new TracingIS(data);
try (DataInputStream expected = new DataInputStream(is)) { expected.read(new byte[4096]); }
// closedCount was 0 before the fix (even with a PLAIN explicit expected.close(), not just
// try-with-resources — ruling out a try-with-resources bytecode bug specifically), 1 after.
```
Also verified with `BufferedInputStream`/`InflaterInputStream` double-wrapped under a
`DataInputStream` (matching `JarEntriesStream`'s exact wrapping shape for DEFLATED entries) — same
before/after result. Re-ran the instrumented `FileDataBlock` against the real
`SecurityInfoTests`/`NestedJarFileTests`: no more open-path assertions in either class.
`cargo test -p cratonvm-native-builtins --lib`: 2976 passed, 6 pre-existing unrelated failures
(JCA Ed25519, jspecify type-use annotations ×3, ByteBuffer address test, xerces whitespace —
matches prior sessions' documented baseline). `cargo test -p cratonvm-native-io --lib`: 346
passed, 0 failed.

### Dead ends (for future readers hitting the same trail)

- **`ZipInputStream`/`InflaterInputStream`'s own native reimplementation is dead code under
  real-JDK boot.** `native-builtins/src/phases_late.rs::register_p58_gzip_streams` registers a
  full synthetic `ZipInputStream` (`<init>`/`getNextEntry`/`read`/`close`, eager in-memory
  drain-and-reparse via the `zip` crate). This is **only reached under `--synthetic-jdk`** — real-
  JDK mode (the default whenever a JDK is detected) runs the genuine `java.util.zip.ZipInputStream`
  /`InflaterInputStream` bytecode instead, confirmed via direct instrumentation (`eprintln!` in the
  native never fired for the real Spring Boot test run, nor for a minimal standalone
  `new ZipInputStream(...).close()`/`getNextEntry()` repro). The interpreter's `check_override`
  gate in `vm/src/vm/vm_exec.rs` (`invoke_or_native`) prefers real bytecode for any concrete,
  non-abstract method unless the (class, method) pair is explicitly allowlisted there — neither
  `ZipInputStream` nor `InflaterInputStream` is.
- Real bytecode's `close()` propagation through `ZipInputStream`/`InflaterInputStream` was
  independently verified correct (a `TracingInputStream` wrapped by a real `ZipInputStream`, and
  separately by a real 3-arg-constructed `InflaterInputStream` sharing a reused `Inflater` across
  iterations — matching `JarEntriesStream`'s own `ZipInflaterInputStream` usage exactly — both
  correctly cascaded `close()` down to the tracing stream, both before and after touching the
  `InflaterInputStream.close()` native).
- Not a JIT miscompilation: the `DataInputStream` leak reproduced identically at N=1 call
  (interpreter only, no JIT warmup) and with `--nojit`.
- Not a try-with-resources bytecode bug: a **plain, explicit** `expected.close();` (no
  try-with-resources at all) exhibited the identical leak.
- Not a field-layout mismatch: reflectively read `FilterInputStream.in` off the live
  `DataInputStream` object both before and after the failed `close()` call — correctly held the
  wrapped stream reference throughout. The bug is purely about native-vs-inherited-bytecode
  dispatch precedence, not data corruption.

## Bug 3 — real PKCS7 signature verification fails (OPEN, not investigated further)

After bugs 1+2, `SecurityInfoTests.getWhenJarIsSigned` and `NestedJarFileTests.verifySignedJar`
both still fail with:
```
java.lang.SecurityException: cannot verify signature block file META-INF/BC2048KE
    sun.security.util.SignatureFileVerifier.processImpl(SignatureFileVerifier.java:308)
    sun.security.util.SignatureFileVerifier.process(SignatureFileVerifier.java:281)
    java.util.jar.JarVerifier.processEntry(JarVerifier.java:323)
```
This comes from real JDK bytecode: `SignerInfo[] infos = block.verify(sfBytes);` (`block` is a
`sun.security.pkcs.PKCS7` parsed from the real `META-INF/BC2048KE.RSA`-equivalent signature block
in `bcprov-jdk18on-1.78.1.jar`) returning `null` — i.e. the actual cryptographic RSA-signature
verification over the `.SF` file's bytes is failing against BouncyCastle's real signing
certificate. This is a **distinct, deeper bug** in CratonVM's crypto primitives (RSA signature
verification, digest computation, or certificate/public-key parsing along that real-bytecode path)
— not characterized further here. Candidates for a follow-up investigation: `native-builtins/src/
jca/signature.rs` (real `Signature.verify()` dispatch), `native-builtins/src/jca/key_factory.rs`
(X.509 `PublicKey` extraction), or the digest primitives feeding `MessageDigest` during PKCS7
`SignerInfo` verification.

## Files changed

- `native-builtins/src/jca/cipher.rs` — `Providers.startJarVerification`/`stopJarVerification`
  natives (bug 1 fix).
- `native-io/src/lib.rs` — `native_dis_close` (bug 2 fix, the registration that actually wins at
  boot).
- `native-builtins/src/classloader.rs` — `DataInputStream.close()` fixed for consistency (dead in
  practice); `BufferedInputStream.close()` investigated and left as documented-intentional no-op.
- `native-builtins/src/phases_late.rs` — `InflaterInputStream.close()`/`ZipInputStream.close()`
  fixed for `--synthetic-jdk` mode correctness (both confirmed dead code under real-JDK boot, kept
  regardless — see "Dead ends" above).
