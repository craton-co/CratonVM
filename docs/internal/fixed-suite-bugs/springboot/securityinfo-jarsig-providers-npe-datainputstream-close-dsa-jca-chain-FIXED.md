# `SecurityInfoTests`/`NestedJarFileTests`: `Providers.<clinit>` NPE + `DataInputStream.close()` no-op + DSA JCA-provider gaps — FIXED (6 of 7 bugs); nested PKCS7 timestamp-token ASN.1 parsing bug OPEN

**Status:** 6 root causes FIXED. A 7th, distinct, deeper bug in nested-PKCS7 (RFC 3161 timestamp
token) attribute parsing remains OPEN and blocks full test passage.

Investigated while following up on the `zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`
residuals list, which named `SecurityInfoTests`/`NestedJarFileTests` (signed-jar cases) as an
unexplored cluster: jar-signature verification failing, plus a `bcprov-jdk18on-1.78.1.jar` file
handle left open past test teardown.

These turned out to be **seven separate root causes**, not one — what was originally logged as a
single "bug 4: X.509 DER-parsing failure" in an earlier round of this investigation turned out,
once actually root-caused, to be **three separate JCA-provider gaps**, all specific to DSA (bugs
4–6 below); fixing those revealed an eighth-inning **new, genuinely distinct** bug (7) in nested
PKCS7 timestamp-token verification that no earlier round of this investigation had reached:

1. `sun.security.jca.Providers.<clinit>` no-op'd → `NullPointerException` in
   `Providers.startJarVerification()`/`stopJarVerification()` — **FIXED**.
2. `java.io.DataInputStream.close()` registered as an unconditional no-op → file-handle leak —
   **FIXED**.
3. CratonVM has no native DSA `Signature` sign/verify at all, and `sun.security.util
   .SignatureUtil.{initVerify,initSign}WithParam` were never reachable (dead native
   registrations) — **FIXED** (real-JDK-SPI routing added for DSA; `SignatureUtil` allowlisted
   for native dispatch).
4. `KeyFactory.getInstance("DSA")` had no native support at all (`algo_idx("DSA")` unmapped) —
   **FIXED** (routed to the real `sun.security.provider.DSAKeyFactory` SPI, mirroring the
   existing RSA/EC pattern).
5. `AlgorithmParameters.getInstance("DSA")` had no registered provider service, so
   `AlgorithmId.decodeParams()` silently failed and `DSAPublicKey.getParams()` returned null —
   **FIXED** (seeded the real `SUN` provider's `AlgorithmParameters.DSA →
   sun.security.provider.DSAParameters` service entry).
6. `CertificateFactory.getInstance(String)` (the 1-arg overload — the one virtually every real
   caller uses) was intercepted by an old synthetic-stub native that never set the real
   `certFacSpi` field, so any real-bytecode-only method not specifically re-implemented in that
   native (`generateCertPath`, `generateCRL(s)`, `getCertPathEncodings`) NPE'd — **FIXED** (the
   native now builds a genuine `CertificateFactory` wrapping a real provider SPI when the
   real-JCA/EC/DSA routing path is active, falling back to the old synthetic stub otherwise).
7. A real, JDK-independent-confirmed CratonVM bug in nested PKCS7 (RFC 3161 timestamp token)
   attribute parsing: `sun.security.pkcs.SignerInfo.verify()`, when validating the *inner*
   SignerInfo of an embedded timestamp token, fails to find a `contentType` authenticated
   attribute that real HotSpot finds without issue on byte-identical input — **OPEN**, not
   root-caused to a fix (see below). This is what actually still blocks
   `getWhenJarIsSigned`/`verifySignedJar` from passing.

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
bugs 4–6 further upstream in the same call chain, so fixing 3 alone doesn't move the test result.
Left in because it's independently correct and needed once 4–6 are fixed.

After fix 4 (`KeyFactory` DSA routing): the `SecurityException` disappears — real progress — but a
*new* failure appears: `InvalidKeyException: DSA public key lacks parameters` from
`sun.security.provider.DSA.engineInitVerify`, one layer deeper in the same call chain
(`X509Key.parse()`'s DSA public key now constructs successfully, but its `getParams()` still
returns null). This is bug 5.

After fix 5 (`AlgorithmParameters` DSA seeding): the `InvalidKeyException` disappears; a further
*new* failure appears: `NullPointerException: Cannot invoke
"CertificateFactorySpi.engineGenerateCertPath(List)" because "this.certFacSpi" is null` from
`SignatureFileVerifier.getSigners()`. Real signature verification (DSA `Signature.verify()`)
genuinely succeeded at this point — this NPE is in `CertPath` *construction*, after the crypto
check already passed. This is bug 6.

After fix 6 (`CertificateFactory` real-SPI construction): **no more exceptions at all** — jar
verification runs to completion cleanly for the first time. But `SecurityInfoTests
.getWhenJarIsSigned` still fails, now with a plain `AssertionError: Expecting actual not to be
null` (`entry.getCertificates()`/`getCodeSigners()` return null for every `.class` entry). This is
bug 7 — a **different subsystem** (nested PKCS7 ASN.1 attribute parsing, not JCA provider/SPI
routing) that no earlier round of fixes had exposed, since bugs 1–6 all threw hard exceptions
*before* code ever reached this deep.

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

`bcprov-jdk18on-1.78.1.jar`'s real jarsigner signature (`../../../../apps/META-INF/BC2048KE.{SF,DSA}`) is signed
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

This fix is real and necessary, but the test still fails — see bugs 4–7. (Note: what an earlier
round of this investigation logged as "bug 4: X.509 DER-parsing failure, OPEN" was a
misdiagnosis — the `ATHROW`-tracer catching the wrong exception in a chain of several. The real
sequence, found by fixing forward one exception at a time, was bugs 4–6 below, all DSA-specific
JCA-provider gaps, not DER-byte-level parsing at all.)

## Bug 4 — `KeyFactory.getInstance("DSA")` has no native support (FIXED)

### Root cause

`sun.security.x509.X509Key.parse()` (real bytecode, reached while re-parsing a real X.509 cert's
`SubjectPublicKeyInfo` — this is the step the earlier misdiagnosed "bug 4" `ATHROW` trace actually
caught mid-flight, one exception short of the real one) calls
`KeyFactory.getInstance("DSA").generatePublic(x509KeySpec)`. `native-builtins/src/jca/key_factory
.rs::algo_idx("DSA")` was unmapped (returned `-1`), so this fell through to the generic
unrecognized-algorithm synthetic path and failed with `InvalidKeySpecException: cannot generate a
usable Unknown public key` — not an `IOException`/DER-parsing failure at all, despite superficially
looking like one three frames up the stack.

### Fix

`native-builtins/src/jca/key_factory.rs` — added `const ALGO_DSA: i32 = 11;`, mapped
`"DSA" | "DSS"` to it in `algo_idx`/`algo_name`, and added a `drive_real_dsa_keyfactory` helper
that drives the real `sun.security.provider.DSAKeyFactory` SPI (via the existing
`drive_keyspec_spi` mechanism — CratonVM has no synthetic DSA key material to fall back to at all,
unlike RSA/EC), gated on `route_dsa_to_real()` (the same flag bug 3 added). Wired into both
`kf_generate_public` and `kf_generate_private`.

### Verification

A standalone `CertParseRepro.java` (reflective `X509CertInfo` construction from the leaf cert's
raw TBSCertificate bytes, extracted via `openssl x509 -in ... -outform DER`) went from NPE/failure
to printing the real, correctly-parsed `p`/`q`/`g`/`y` DSA public-key material. Re-running the
actual suite: the `SecurityException` from bug 3's fix disappeared, replaced by the deeper
`InvalidKeyException` of bug 5 — genuine forward progress, not a regression.

## Bug 5 — `AlgorithmParameters.getInstance("DSA")` has no registered provider service (FIXED)

### Root cause

With bug 4 fixed, `sun.security.provider.DSA.engineInitVerify(PublicKey)` (real bytecode) started
throwing `InvalidKeyException: DSA public key lacks parameters`. Traced via real JDK 25 source:
`DSAPublicKey.getParams()` calls `algid.getParameters()`
(`sun.security.x509.AlgorithmId.getParameters()` → `decodeParams()`), which calls
`AlgorithmParameters.getInstance(algidName)` — with **no** provider service registered for
`AlgorithmParameters.DSA` anywhere in `native-builtins/src/jca/provider_chain.rs`'s synthetic
provider map, this throws `NoSuchAlgorithmException`, which `decodeParams()` **silently catches**
(`algParams = null; return;` — by design, matching real JDK's tolerance for exotic/unsupported
algorithm-parameter types), leaving `getParams()` returning null and the caller (`DSA
.engineInitVerify`) throwing.

### Fix

`native-builtins/src/jca/provider_chain.rs` — added `seed_sun_dsa_services()`, registering
`AlgorithmParameters.DSA → sun.security.provider.DSAParameters` (a real JDK 25 SPI class,
confirmed pure Java/ASN.1 with no native methods and the implicit public no-arg ctor JCA requires,
via `src.zip` source inspection) under the `SUN` provider, plus the `1.2.840.10040.4.1` (id-dsa)
OID alias. Also extended the `GetInstance` bridge-registration gate (previously
`real_jca_mode() || route_ec_to_real()`) to include `route_dsa_to_real()`, so the bridge natives
that make this provider-map entry reachable are wired even if EC routing were ever independently
disabled.

### Verification

`CertParseRepro` extended to print the parsed cert's `getPublicKey()`: went from a working key with
null params to one whose `toString()` includes the real `p`/`q`/`g` DSA parameters (`"Sun DSA
Public Key"` formatting). Re-running the suite: the `InvalidKeyException` disappeared, replaced by
the deeper `NullPointerException` of bug 6.

## Bug 6 — `CertificateFactory.getInstance(String)`'s synthetic stub never sets `certFacSpi` (FIXED)

### Root cause

With bugs 4–5 fixed, the failure moved to `NullPointerException: Cannot invoke
"CertificateFactorySpi.engineGenerateCertPath(List)" because "this.certFacSpi" is null` from
`sun.security.util.SignatureFileVerifier.getSigners()`'s `certificateFactory
.generateCertPath(chain)` call — the *final* step of jar-signature verification, reached only
after DSA `Signature.verify()` itself had already genuinely succeeded.

`native-builtins/src/phases_late.rs::register_p68_security_cert` registers a native directly on
`java/security/cert/CertificateFactory.getInstance(String)` (the **1-arg** overload — the one
`SignatureFileVerifier`, and virtually every real caller, uses) that hands out a 1-field
`alloc_concurrent_synthetic` stub object with no real `certFacSpi` field ever set. Since this is a
**static** method, the registered native always wins unconditionally (no `check_override` gate
applies to statics — see `reference_native_close_dispatch_precedence_over_inherited_bytecode`'s
general finding). The stub's `generateCertificate`/`generateCertificates` methods were already
natively intercepted with a real-`certFacSpi`-first-else-legacy-ad-hoc-parser fallback, so they
worked regardless — but `generateCertPath`, `generateCRL(s)`, and `getCertPathEncodings` are
**not** natively intercepted at all, so they always ran on real bytecode, which unconditionally
needs the real `certFacSpi` field the synthetic stub never had. This bug therefore predates this
session's DSA work entirely — it was simply never reached before, since bugs 1–5 always threw
first.

### Fix

`native-builtins/src/jca/provider_chain.rs` — added `try_build_real_certificate_factory`, which
resolves the algorithm against the existing provider service map (reusing `find_service_provider`/
`build_jca_impl`/`resolve_or_make_provider`, the same primitives `getinstance_instance_search`
already uses for the 2/3-arg `getInstance` overloads, which were never natively intercepted and
already worked correctly) and constructs a genuine `CertificateFactory` via its real
`(CertificateFactorySpi, Provider, String)` constructor.
`native-builtins/src/phases_late.rs`'s `getInstance(String)` native now calls this first (gated on
`real_jca_mode() || route_ec_to_real() || route_dsa_to_real()`) and only falls back to the old
synthetic stub if it returns `None` (e.g. pure-synthetic mode, or an unresolvable algorithm).

### Verification

Re-ran the suite: the `certFacSpi` NPE disappeared entirely — jar verification now runs to
completion with **no exceptions at all**, for the first time in this investigation. `cargo test -p
cratonvm-native-builtins --lib`: 2976 passed, 6 failed — all 6 match the pre-existing baseline
documented earlier in this doc (JCA Ed25519 dead-key test, 3× jspecify type-use annotations,
ByteBuffer address test, xerces whitespace); confirmed by stashing this session's diff and
re-running the single Ed25519 test in isolation, which fails identically without any of this
session's changes.

## Bug 7 — nested PKCS7 (RFC 3161 timestamp token) attribute parsing (OPEN, not fixed)

### What's confirmed

With bugs 1–6 fixed, `SecurityInfoTests.getWhenJarIsSigned` fails differently again — no more
exceptions, but a plain `AssertionError: Expecting actual not to be null`:
`entry.getCertificates()`/`getCodeSigners()` return **null for every `.class` entry**, even though
`content.hasJarSignatureFile()` is true and nothing throws anywhere visible to the test.

Root-caused via a from-scratch, Spring-Boot-independent, reflective repro
(`SigVerifyRepro.java`) that directly constructs a real `sun.security.util.SignatureFileVerifier`
from the extracted `../../../../apps/META-INF/{MANIFEST.MF,BC2048KE.SF,BC2048KE.DSA}` bytes and calls its
package-private `process(...)` method, catching whatever it throws:

```
process() FAILED: java.security.SignatureException: Error verifying signature
	at sun.security.pkcs.SignerInfo.verify(SignerInfo.java:473)
	at sun.security.pkcs.PKCS7.verify(PKCS7.java:534)
	at sun.security.pkcs.PKCS7.verify(PKCS7.java:551)
	at sun.security.pkcs.SignerInfo.getTimestamp(SignerInfo.java:675)
	at sun.security.util.SignatureFileVerifier.getSigners(SignatureFileVerifier.java:751)
Caused by: java.io.IOException: No value found for attribute 1.2.840.113549.1.9.3
	at sun.security.pkcs.PKCS9Attributes.getAttributeValue(PKCS9Attributes.java:278)
	at sun.security.pkcs.SignerInfo.verify(SignerInfo.java:344)
```

`SignatureFileVerifier.getSigners()` (line 751: `signers.add(new CodeSigner(certChain,
info.getTimestamp()));`) calls `SignerInfo.getTimestamp()` **unguarded** — unlike the *other*
`getTimestamp()` call site inside `SignerInfo.verify(PKCS7, byte[])` (lines 311–323), which wraps
it in its own `catch (Exception e) { /* signed but w/o a timestamp */ }` specifically so a broken
timestamp token doesn't fail the primary signature. `getSigners()`'s call has no such guard, so any
exception from `getTimestamp()` propagates all the way up through `processImpl()`/`process()`,
caught only by `JarVerifier.processEntry()`'s broad, silent
`catch (IOException | CertificateException | NoSuchAlgorithmException | SignatureException e) { //
ignore and treat as unsigned }` — explaining why the test sees no exception at all, just silently
missing certs.

`getTimestamp()` parses the RFC 3161 timestamp token BC embedded as an unauthenticated attribute on
the *outer* SignerInfo — itself another, nested PKCS7 `SignedData` structure — and verifies its
*own inner* SignerInfo. That inner `SignerInfo.verify()` throws at line 344:
`authenticatedAttributes.getAttributeValue(PKCS9Attribute.CONTENT_TYPE_OID)` (OID
`1.2.840.113549.1.9.3`), meaning the inner SignerInfo's `PKCS9Attributes` — a `Hashtable
<ObjectIdentifier, PKCS9Attribute>` — doesn't contain (or fails to look up) a `contentType`
attribute that RFC 3161 timestamp tokens are required to carry (`id-ct-TSTInfo`).

**Confirmed genuinely CratonVM-specific, not a jar/JDK-version quirk**: ran the *identical*
`SigVerifyRepro.java`, unmodified, against the identical extracted bytes under real HotSpot JDK 25
(`--add-opens java.base/sun.security.pkcs=ALL-UNNAMED --add-opens
java.base/sun.security.util=ALL-UNNAMED`) — it succeeds completely, reporting 5369 signed entries,
no exception.

None of `sun.security.pkcs.PKCS7`, `sun.security.pkcs.SignerInfo`, or `sun.security.util
.PKCS9Attributes` have ANY native registration anywhere in `native-builtins` (confirmed by
exhaustive grep) — they run entirely on real bytecode both here and for the *outer* SignerInfo,
which verifies fine. The divergence must therefore come from a lower-level ASN.1 primitive that
behaves differently for this specific nested structure than it does for the outer one.

### Leading hypothesis (not confirmed)

`PKCS9Attributes` stores attributes in a `Hashtable<ObjectIdentifier, PKCS9Attribute>` and looks
them up by a **separately-constructed** `ObjectIdentifier` (`PKCS9Attribute.CONTENT_TYPE_OID`, a
different object instance than whatever the inner SignerInfo's parser constructed while decoding
the attribute set). If `ObjectIdentifier.equals()`/`hashCode()` are inconsistent for an instance
that's survived a moving-GC relocation since insertion — a bug pattern with substantial precedent
in this codebase (see `reference_stale_ref_decode_hardening`,
`reference_native_io_read_stale_objectref_pin_fix`, and others in the GC/moving-young-gen memory
cluster) — a `Hashtable.get()` miss on a logically-present key would produce exactly this symptom.
This is a hypothesis, not a confirmed diagnosis; the outer SignerInfo's `PKCS9Attributes` clearly
works, so whatever's different is specific to something about the inner/nested parse (timing,
object lifetime, or recursion depth into the same native primitives) — not a blanket
`ObjectIdentifier` bug, or the outer attributes would fail too.

### Why not fixed here

This is a **different subsystem** from bugs 1–6 (real ASN.1/DER attribute-set parsing depth, not
JCA provider/SPI routing) and a different symptom class (silent data loss via a caught-and-ignored
exception, not a hard failure) — confirming the exact native gap requires byte-level ASN.1
inspection of the nested timestamp token's attribute SET, one level deeper than anything this
session's tooling was built for. Stopped here rather than open an eighth nested investigation in
the same session.

### Suggested starting points for a follow-up

- `SigVerifyRepro.java` (recipe below) is a complete, JDK-independent, Spring-Boot-independent
  repro — no rebuild needed to iterate, since it hits real (unmodified) `sun.security.pkcs`/
  `sun.security.util` bytecode directly via reflection. Re-extract the byte inputs and re-run it
  under CratonVM after any candidate fix.
- Extract the byte inputs: `unzip -p bcprov-jdk18on-1.78.1.jar META-INF/MANIFEST.MF
  META-INF/BC2048KE.SF META-INF/BC2048KE.DSA` from
  `~/.gradle/caches/modules-2/files-2.1/org.bouncycastle/bcprov-jdk18on/1.78.1/*/bcprov-jdk18on-1.78.1.jar`.
- The repro itself: reflectively construct `sun.security.util.ManifestDigester(byte[])`, then
  `sun.security.util.SignatureFileVerifier(ArrayList, ManifestDigester, String, byte[])` (the
  `.DSA` bytes), call its `setSignatureFile(byte[])` (the `.SF` bytes), then `process(Hashtable,
  List, String)` — catch `InvocationTargetException`, print `getCause()`'s full chain.
- To go one level deeper without modifying the repro's shape: reflectively call
  `SignerInfo.getTsToken()` on the outer signer to get the raw nested `PKCS7`, then walk its
  `ContentInfo`/`SignerInfo[]` and attribute `Hashtable` directly (via more reflection) to compare
  the RAW attribute set actually parsed against what the DER bytes should decode to — this would
  confirm or refute the `Hashtable`/`ObjectIdentifier` hypothesis above without guessing.
- If the hypothesis holds, the fix is almost certainly in whatever native backs `ObjectIdentifier`
  construction/equality/hashing during DER SET-OF decoding, or in ensuring GC-pinning discipline
  around `Hashtable` insertion during nested/recursive ASN.1 parsing — not in
  `sun.security.pkcs`/`sun.security.util` itself (all real bytecode, unmodifiable target).

## Files changed

- `native-builtins/src/jca/cipher.rs` — `Providers.startJarVerification`/`stopJarVerification`
  natives (bug 1 fix).
- `native-io/src/lib.rs` — `native_dis_close` (bug 2 fix, the registration that actually wins at
  boot).
- `native-builtins/src/classloader.rs` — `DataInputStream.close()` fixed for consistency (dead in
  practice); `BufferedInputStream.close()` investigated and left as documented-intentional no-op.
- `native-builtins/src/phases_late.rs` — `InflaterInputStream.close()`/`ZipInputStream.close()`
  fixed for `--synthetic-jdk` mode correctness (bug 2 fix, dead code under real-JDK boot, kept
  regardless — see "Dead ends" above); `CertificateFactory.getInstance(String)` now prefers a real
  `CertificateFactory` when available (bug 6 fix).
- `native-builtins/src/jca/signature.rs` — DSA→real-SPI routing (bug 3 fix): `SIG_SHA1_DSA`
  constant, name/OID mappings, `dsa_real_spi_class`, wired into `sig_sign`/`sig_verify`.
- `native-builtins/src/lib.rs` — `route_dsa_to_real()` (bug 3 fix, default-ON kill-switched flag).
- `vm/src/vm/vm_exec.rs` — `sun/security/util/SignatureUtil` `check_override` allowlist entry
  (bug 3 fix, the piece that actually makes `sigutil_init_verify_key`/etc. reachable).
- `native-builtins/src/jca/key_factory.rs` — `ALGO_DSA` constant, `drive_real_dsa_keyfactory`,
  wired into `kf_generate_public`/`kf_generate_private` (bug 4 fix).
- `native-builtins/src/jca/provider_chain.rs` — `seed_sun_dsa_services` (bug 5 fix);
  `try_build_real_certificate_factory`, extended `ec_real` gate to include `route_dsa_to_real()`
  (bug 6 fix).

Bug 7 (nested PKCS7 timestamp-token attribute parsing) needs its own follow-up session — no code
changed for it here.
