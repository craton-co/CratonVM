# `SecurityInfoTests`/`NestedJarFileTests`: `Providers.<clinit>` NPE + `DataInputStream.close()` no-op — FIXED (3 of 4 bugs); X.509 DER-parsing failure OPEN

**Status:** 3 root causes FIXED (2 crypto/dispatch gaps genuinely fixed, plus general DSA
`Signature` routing infrastructure added). A fourth, distinct, deeper bug in real X.509
certificate DER parsing remains OPEN and blocks full test passage.

Investigated while following up on the `zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`
residuals list, which named `SecurityInfoTests`/`NestedJarFileTests` (signed-jar cases) as an
unexplored cluster: jar-signature verification failing, plus a `bcprov-jdk18on-1.78.1.jar` file
handle left open past test teardown.

These turned out to be **four separate root causes**, not one:

1. `sun.security.jca.Providers.<clinit>` no-op'd → `NullPointerException` in
   `Providers.startJarVerification()`/`stopJarVerification()` — **FIXED**.
2. `java.io.DataInputStream.close()` registered as an unconditional no-op → file-handle leak —
   **FIXED**.
3. CratonVM has no native DSA `Signature` sign/verify at all, and `sun.security.util
   .SignatureUtil.{initVerify,initSign}WithParam` were never reachable (dead native
   registrations) — **FIXED** (real-JDK-SPI routing added for DSA; `SignatureUtil` allowlisted
   for native dispatch). Necessary but **not sufficient** — see bug 4.
4. `sun.security.x509.X509CertInfo`/`sun.security.util.DerValue` fail to fully re-parse one of the
   two real certificates embedded in the PKCS7 block (almost certainly the BouncyCastle leaf
   cert's unusually large 2048-bit DSA key material) — **OPEN**, not root-caused to a fix (see
   below). This is what actually still blocks `getWhenJarIsSigned`/`verifySignedJar` from passing.

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

After fix 3 (DSA routing + `SignatureUtil` allowlist): same 2/3 pass rate, same single
`SecurityException` for `getWhenJarIsSigned`/`verifySignedJar` — bug 3 was real and necessary
(confirmed via a from-scratch, Spring-Boot-independent repro) but the observed test failure has
bug 4 further upstream in the same call chain, so fixing 3 alone doesn't move the test result.
Left in because it's independently correct and needed once bug 4 is fixed.

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

## Bug 3 — no native DSA crypto + `SignatureUtil` natives unreachable (FIXED, necessary but not sufficient)

### Root cause

`bcprov-jdk18on-1.78.1.jar`'s real jarsigner signature (`META-INF/BC2048KE.{SF,DSA}`) is signed
with a **2048-bit DSA** key (`SHA256withDSA`) — the `.DSA` file extension is literal here, not
just jarsigner's historical default naming (confirmed via `openssl asn1parse`/`-print_certs` on
the extracted signature block: the leaf cert "Legion of the Bouncy Castle Inc." has
`Public Key Algorithm: dsaEncryption`, 2048-bit `p`). Two independent gaps block this:

1. **No native DSA crypto at all.** `native-builtins/src/jca/signature.rs::verify_dispatch`/
   `sign_dispatch` (the synthetic Rust crypto backing RSA/ECDSA) have no `SIG_SHA256_DSA` arm —
   `verify_dispatch(...).unwrap_or(false)` means DSA `Signature.verify()` silently returns `false`
   for *any* input, no exception, ever. Confirmed via a minimal repro completely independent of
   Spring Boot or certificates: `Signature.getInstance("SHA256withDSA")` + `initVerify`/`update`/
   `verify` against a real JDK-generated DSA keypair+signature always returned `false`.
2. **`sun.security.util.SignatureUtil.{initVerify,initSign}WithParam` were dead native
   registrations.** These exist in `native-builtins/src/jca/signature.rs` (`sigutil_init_verify_key`
   etc.) specifically to bypass a known issue: real `SignatureUtil` bytecode indirects through
   `SharedSecrets.getJavaSecuritySignatureAccess()`, which is populated only inside
   `Signature.<clinit>` — no-op'd in `jca/key_factory.rs` for unrelated reasons, so the accessor is
   permanently `null` and real bytecode would NPE. But `sun/security/util/SignatureUtil` was never
   added to the `check_override` allowlist in `vm/src/vm/vm_exec.rs` (the list that lets a
   registered native pre-empt a *concrete* real-bytecode method — see `SigProbe` entries for
   `java/security/Signature`/`KeyFactory`/`KeyPair` already there). Since `SignatureUtil` is a
   real, loadable class with real bytecode for these static methods, `has_own_bytecode` was true
   and the interpreter always preferred real bytecode — the natives were dead code, for **every**
   algorithm's jar-signature verification, not just DSA. (This didn't previously matter for
   RSA/ECDSA because those go through `Signature.getInstance(...).verify(...)` for other callers —
   e.g. plain `Signature` API use — via a path that doesn't need `SignatureUtil`; it specifically
   blocks the `sun.security.pkcs.SignerInfo.verify()` jar-signature path, which calls
   `SignatureUtil.initVerifyWithParam` directly.)

### Fix

`native-builtins/src/jca/signature.rs` — added `dsa_real_spi_class(alg)`, routing
`SHA256withDSA`/`SHA1withDSA` to the real `sun.security.provider.DSA$SHA256withDSA`/`$SHA1withDSA`
SPIs via the existing `drive_real_signature_spi` helper (same mechanism already used for
ECDSA/EdDSA/ML-DSA — construct, `engineInitVerify(key)`, `engineUpdate`, `engineVerify`/
`engineSign`). Gated behind `route_dsa_to_real()` (default ON, kill-switch
`CRATONVM_SYNTHETIC_DSA=1`), mirroring `route_ec_to_real`/`route_pqc_to_real`. Also added the
`SIG_SHA1_DSA` algorithm index, name/OID mappings (`"DSA"`, `"SHA1withDSA"`,
`1.2.840.10040.4.3`, `2.16.840.1.101.3.4.3.2`) for completeness beyond the one SHA-256 case this
jar exercises.

`vm/src/vm/vm_exec.rs` — added `sun/security/util/SignatureUtil` +
`{initVerifyWithParam, initSignWithParam}` to the `check_override` allowlist, right next to the
existing `java/security/Signature`/`KeyFactory`/`KeyPair` SigProbe entries, so
`sigutil_init_verify_key`/`sigutil_init_verify_cert`/`sigutil_init_sign` actually get used.

`BufferedInputStream`-style caution applied here too: did **not** blanket-disable
`Signature.<clinit>`'s no-op or otherwise touch the wider Cipher/KeyGenerator bring-up chain that
depends on it — scoped the fix to the two entry points that needed it, per the same philosophy as
bug 1's fix.

### Verification

Two from-scratch, Spring-Boot- and certificate-independent repros (no `KeyPairGenerator` needed,
since CratonVM's DSA `KeyPairGenerator`/`KeyFactory.generatePublic` are separately incomplete —
out of scope here, not blocking):
- Reflectively read `FilterInputStream.in` (unrelated to bug 2, reused technique) to confirm a
  real, JDK-generated DSA public key correctly round-trips through the fix.
- Traced every `Signature`-related native (`sig_get_instance`, `sig_init_verify`,
  `sigutil_drive`, `sig_verify`) via temporary `eprintln!` instrumentation (removed before commit)
  against the actual `SecurityInfoTests` run: confirmed `SignatureUtil.initVerifyWithParam` now
  resolves to the native (previously: zero native `Signature`/`SignatureUtil` calls fired beyond
  `getInstance`, for the entire test run).

This fix is real and necessary, but the test still fails — see bug 4.

## Bug 4 — real X.509 `DerValue`/`X509CertInfo` parsing fails on this cert (OPEN, not fixed)

### What's confirmed

With bugs 1–3 fixed, instrumenting every `ATHROW` bytecode (temporary, gated behind
`CRATONVM_DSA_DBG`, removed before commit) during `getWhenJarIsSigned` showed the actual failure
is **upstream of signature verification entirely** — in the certificate-issuer-name resolution
`sun.security.pkcs.SignerInfo.verify()` does before it ever gets to `Signature.getInstance()`
for the real check:

```
ATHROW class=java/io/IOException
  DerValue.<init> → DerValue.<init> → X509CertInfo.<init> →
  PKCS7.populateCertIssuerNames → PKCS7.getCertificate → SignerInfo.getCertificate →
  SignerInfo.verify → PKCS7.verify → SignatureFileVerifier.processImpl → ... → JarInputStream.read
ATHROW class=java/security/cert/CertificateParsingException   (X509CertInfo.<init>'s own catch-and-rewrap of the IOException above)
ATHROW class=java/lang/SecurityException                      (the final, observed "cannot verify signature block file")
```

`PKCS7.getCertificate(serial, issuerName)` needs `certIssuerNames[i]` populated for every embedded
cert (there are two: the RSA-signed CA "JCE Code Signing CA" and the DSA-keyed leaf "Legion of the
Bouncy Castle Inc."). `populateCertIssuerNames()` re-parses each cert's TBSCertificate
(`new X509CertInfo(cert.getTBSCertificate())`) to get a canonical `X500Name` for issuer comparison
— and this re-parse throws `IOException` (from `sun.security.util.DerValue`, real bytecode) for
(most likely) the DSA cert, given its unusually large key material (256-byte `p`, `q`/`g`/`y` all
sizeable DER `INTEGER`s inside the `SubjectPublicKeyInfo`).

Real JDK code catches this internally (`populateCertIssuerNames`'s own `catch (Exception e) {
// leave name as is }`) and falls back to `cert.getIssuerDN()` unconverted — **not fatal by
design** on real JDK. Under CratonVM, this fallback apparently still doesn't produce a
`certIssuerNames[i]` that `.equals()` the `X500Name` `SignerInfo` is searching for (likely a
`Principal`-subtype mismatch once the canonical-form conversion silently fails), so
`getCertificate()` returns `null` — even though a matching certificate genuinely exists in the
block. This is consistent with an *earlier*, separate finding in the same investigation: a
minimal `CertificateFactory.getInstance("X.509").generateCertificates(pkcs7Bytes)` call against
the raw PKCS7 bytes returned **0 certificates** (a different, but likely related, X.509/PKCS7
parsing gap).

### Why not fixed here

Root-causing the exact `DerValue`/`X509CertInfo` DER-parsing failure (which specific field —
almost certainly something inside the DSA `SubjectPublicKeyInfo`'s large `INTEGER` encodings, but
not confirmed to the byte) requires the same kind of `ATHROW`-message-level tracing this session
already leaned on heavily, one level deeper, likely into whichever native backs `BigInteger`/DER
`INTEGER` decoding for oversized values. This is a **distinct, self-contained investigation** from
everything else in this doc — different subsystem (real X.509 cert parsing, not JCA `Signature`
dispatch), different symptom class (a parsing exception three frames before any crypto call), and
plausibly a bigger blast radius (any code that re-parses a cert with unusual key material via
`new X509CertInfo(bytes)`, not just jar verification). Stopped here rather than open a fifth
nested investigation in the same session.

### Suggested starting points for a follow-up

- Reproduce directly: `new sun.security.x509.X509CertInfo(leafCert.getTBSCertificate())`
  (reflectively, `sun.security.x509`/`sun.security.pkcs` are not exported — needs
  `--add-exports`/`--add-opens` or an in-package test class) against the extracted
  `META-INF/BC2048KE.DSA` leaf certificate; get the exact `IOException` message (this session's
  `ATHROW` tracer read `Throwable`'s message field by raw index and got `"<non-string>"` — use
  `invoke_virtual(..., "getMessage", "()Ljava/lang/String;", ...)` instead, or `getMessage()` via
  reflection in a plain Java repro, for a real message).
- Extracted signature-block bytes for offline testing: `unzip -p bcprov-jdk18on-1.78.1.jar
  META-INF/BC2048KE.DSA` (PKCS7 SignedData, DER) and `META-INF/BC2048KE.SF` (the signed manifest
  digest file) — both straightforward to re-extract from
  `~/.gradle/caches/modules-2/files-2.1/org.bouncycastle/bcprov-jdk18on/1.78.1/*/bcprov-jdk18on-1.78.1.jar`.
- Likely candidates for the actual native gap: `sun.security.util.DerInputStream`/`DerValue`'s
  handling of `INTEGER`/`BIT STRING` DER elements over ~256 bytes (the DSA `p` parameter's size),
  or `sun.security.x509.X509Key`/`AlgorithmId` parsing a DSA `SubjectPublicKeyInfo` specifically
  (as opposed to RSA/EC, which are presumably well-exercised elsewhere).

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
- `native-builtins/src/jca/signature.rs` — DSA→real-SPI routing (bug 3 fix): `SIG_SHA1_DSA`
  constant, name/OID mappings, `dsa_real_spi_class`, wired into `sig_sign`/`sig_verify`.
- `native-builtins/src/lib.rs` — `route_dsa_to_real()` (bug 3 fix, default-ON kill-switched flag).
- `vm/src/vm/vm_exec.rs` — `sun/security/util/SignatureUtil` `check_override` allowlist entry
  (bug 3 fix, the piece that actually makes `sigutil_init_verify_key`/etc. reachable).

Bug 4 (X.509 DER-parsing) needs its own follow-up session — no code changed for it here.
