# Bug Y — TLS cluster: JCA factory layer unimplemented (`KeyManagerFactory` / `TrustManagerFactory` / `SSLContext` / `KeyStore JKS`)

**Severity:** High (blocks ~15 HTTPS test classes). **Partial FIX landed** —
the JCA factory layer now resolves; remaining blocker is the synthetic-vs-real
`SSLContext` SPI boundary (documented below as the next step).
**HotSpot:** PASS. **Run date:** 2026-06-13.
Affected: `TestSsl`, `TestSSLHostConfig{Cipher,Compat,Integration,Protocol}`,
`TestClientCertTls13`, `TestCustomSsl`, `TestAlpnFallback`, `TestSSLAuthenticator`,
`TestResolverSSL`, `TestManagerWebappSsl`, `TestSecurity2017Ocsp`, …

## What was broken

The JSSE connector's setup dead-ended immediately:

```
runtime error: not implemented: no KeyManagerFactory SunX509 implementation in any provider
... no KeyStore JKS implementation in any provider
```

`build_jca_instance` (provider_chain.rs) had no service entries for the TLS
engines, so `KeyManagerFactory.getInstance("SunX509")`,
`TrustManagerFactory.getInstance(...)`, `SSLContext.getInstance("TLS")`, and
`KeyStore.getInstance("JKS")` all fell through to "no implementation".

## Fix landed (this session)

1. **`seed_sunjsse_services()`** in `provider_chain.rs` — mirrors the real
   SunJSSE + SUN provider TLS service tables so the no-provider `getInstance`
   search resolves the genuine JDK 25 SPI classes (all verified to exist with
   the public no-arg ctor JCA requires):
   - `KeyManagerFactory.SunX509` → `sun.security.ssl.KeyManagerFactoryImpl$SunX509`;
     `NewSunX509`/`PKIX` → `…$X509`
   - `TrustManagerFactory.SunX509` → `…TrustManagerFactoryImpl$SimpleFactory`;
     `PKIX` (+ SunPKIX/X509/X.509 aliases) → `…$PKIXFactory`
   - `SSLContext.TLS/TLSv1.2/TLSv1.3/Default` → `sun.security.ssl.SSLContextImpl$*`
   - `KeyStore.JKS/CaseExactJKS/PKCS12` (SUN) → `sun.security.provider.JavaKeyStore$*` / `pkcs12.PKCS12KeyStore`
   Wired in next to `seed_sunec_services()`.
2. **`IteratorEnumeration.hasMoreElements`/`nextElement`** registered in the
   real-JDK `register_keystore_real` (keystore.rs). `KeyStore.engineAliases`
   returns a synthetic `java/util/IteratorEnumeration`, but its methods were
   only registered in the synthetic-JDK path (`register_synthetic_overrides`,
   never called in real-JDK mode), so the `KeyManagerFactory` init that walks
   `ks.aliases()` threw `NoSuchMethodError IteratorEnumeration.hasMoreElements`.

**Effect:** `TestSsl` went from immediate abort (NOSUMMARY at the first
`getInstance`) to **running all 21 test cases** — the KeyManagerFactory /
TrustManagerFactory / KeyStore-JKS layer all work. JSSE tests now reach the
SSLContext setup; OpenSSL/OpenSSL-FFM variants fail on the native-OpenSSL FFM
binding (environmental, like HotSpot's openssl-absent failures).

## Remaining blocker (next step)

7 JSSE failures, all one cause:

```
NPE: Cannot invoke engineGetSupportedSSLParameters on null
  at javax.net.ssl.SSLContext.getSupportedSSLParameters()  pc=7
  at org.apache.tomcat.util.net.jsse.JSSEUtil.initialise()  pc=61
```

`SSLContext.getInstance("TLS")` is intercepted by CratonVM's **synthetic** TLS
layer (`phases_late.rs::register_p68_ssl`, the always-on NEW-13 synthetic
SSLContext/SSLSocket/SSLSession), which returns a 5-field field-holder with **no
`contextSpi`**. Tomcat's `JSSEUtil` uses the real `javax.net.ssl.SSLContext`
server API (`getSupportedSSLParameters()` → `contextSpi.engine…()`), so it NPEs.

Two ways forward (both substantial):
- **(A) Complete the synthetic server path:** add the JSSEUtil-required surface
  to `register_p68_ssl` — `getSupportedSSLParameters`/`getDefaultSSLParameters`,
  `createSSLEngine`, server `SSLEngine` wrap/unwrap, etc. — keeping client+server
  on the consistent synthetic handshake.
- **(B) Route to real `SSLContextImpl`:** gate the synthetic
  `SSLContext.getInstance` off in real-JDK mode so the real bytecode + the new
  `seed_sunjsse_services` entries build a genuine `SSLContextImpl$TLSContext`,
  then make the real `SSLEngine` handshake work end-to-end (real RSA/ECDHE +
  AES-GCM + HMAC + X.509 + the TLS state machine). Largest effort, but the only
  path to a spec-faithful handshake.

## Reproduction

```
cratonvm.exe -cp <cp> org.junit.runner.JUnitCore org.apache.tomcat.util.net.TestSsl
# Before: abort — "no KeyManagerFactory SunX509 implementation"
# After:  21 run / 7 JSSE fail on "engineGetSupportedSSLParameters on null"
```

## Handshake-bridge investigation (2026-06-13) — complete architecture map

Pushed past the SSLContext-SPI boundary and mapped the entire server TLS path.

**Key architecture facts:**
- Real-JDK mode's `javax.net.ssl.SSLContext` is the **minimal stub**
  `net_phase_e.rs::register_re6_ssl_context` (getInstance/getDefault/init/
  get{Socket,ServerSocket}Factory/getProtocol on a 2-field synthetic). The full
  synthetic TLS layer with an SSLEngine (`phases_late.rs::register_p68_ssl`,
  NEW-13) is gated to **synthetic-jdk** mode (`register_synthetic_overrides`) and
  is INERT in real-JDK mode. (I first added getSupportedSSLParameters there by
  mistake — no effect; the right layer is `re6`.)
- The real-JDK **SSLEngine is rustls-backed**: `t27_tls.rs::register_sslengine_real`
  (active in real-JDK via lib.rs ~5317), engine class
  `sun/security/ssl/SSLEngineImpl`, state keyed by ObjectRef
  (`engine_id_or_alloc`); the rustls server/client configs are built in
  `default_engine_server_config` / `default_engine_client_config`.
- The synthetic **KeyStore** (`keystore.rs`) exposes the loaded cert/key as DER
  via `keystore_get_cert_der` / `keystore_get_private_key` / `keystore_get_chain`.

**Forward progress made (WIP, NOT committed — would regress FAIL→HANG alone):**
Adding to `register_re6_ssl_context` — `getSupportedSSLParameters()` (real
`javax.net.ssl.SSLParameters`), `createSSLEngine()`/`(String,I)` (→ a
`sun/security/ssl/SSLEngineImpl` object the t27 natives drive),
`getServerSessionContext()` + `SSLSessionContext` setter no-ops — makes
`JSSEUtil.initialise()` succeed and the NIO connector reach the handshake.
`TestSsl` then advances from "21 run / 7 fast JSSE FAIL" to **hanging in
`testSimpleSsl[JSSE]`** (the rustls server has no identity → handshake stalls).
That hang is a regression vs the fast FAIL, so these re6 additions must land
**together with** the identity+trust wiring below — not before.

**The two remaining walls (precise next steps):**
1. **Server identity** — `default_engine_server_config` calls
   `runtime_tls_identity()`, which is never set in production (only in tests).
   Wire it: on keystore load (`keystore.rs::engine_load`) or in
   `re6 SSLContext.init`, pull the first key entry's cert+key DER
   (`keystore_get_chain` / `keystore_get_private_key`), convert DER→PEM, and
   `t27_tls::set_runtime_tls_identity(Some(RuntimeTlsIdentity{cert_pem,key_pem,…}))`.
   NOTE: needs a base64 encoder (not currently a native-builtins dep — add one
   or hand-roll) and correct PEM headers (PKCS#8 "PRIVATE KEY" for the key).
2. **Client trust** — `default_engine_client_config` trusts only
   `load_native_root_store()` (system CAs), so the self-signed test server cert
   is rejected. Wire `TrustManagerFactory.init(trustStore)` (or the test's
   client SSLContext) to feed the truststore certs as rustls client roots.

Both ends are rustls, so once identity+trust are wired consistently the loopback
handshake should interoperate. Estimated several build/verify iterations
(~11 min each) plus handshake-interop debugging — a focused multi-step task, not
a single edit. The factory-layer fixes above are already on dev; the re6 +
identity + trust work is the remaining bridge.

## Identity+trust build-out (2026-06-13, branch `fix/tomcat-suite-loop`, NOT merged)

Built the full identity+trust wiring (commit on the branch, **not merged to dev**
— still a FAIL→CRASH regression at the walls below):
- **re6** (`net_phase_e.rs`): added `getSupportedSSLParameters` (real
  `SSLParameters`), `createSSLEngine()`/`(String,I)` (→ `sun/security/ssl/SSLEngineImpl`,
  driven by the t27 rustls natives), `getServerSessionContext`, + `SSLSessionContext`
  setter no-ops. → `JSSEUtil.initialise()` now succeeds; the connector reaches the
  handshake.
- **t27_tls.rs**: `install_identity_from_der` (keystore PKCS#8 key DER + DER cert
  chain → PEM → `set_runtime_tls_identity`), `add_extra_trust_root_der` + a global
  extra-roots store that `default_engine_client_config` now adds (so an in-process
  loopback client trusts the embedded server's cert), and a dependency-free
  base64/`der_to_pem`.
- **keystore.rs** `engine_load`: on every keystore load, install the first key
  entry as the server identity and register all certs as extra client trust roots.

**Progress (branch commit `e22ec46c`, NOT merged):** `TestSsl` now advances past
JSSEUtil into connector SSLContext/KeyManager init. Resolved walls:

1. **JKS private key — FIXED.** `load_jks` stored the *encrypted*
   `EncryptedPrivateKeyInfo` as `key_der` and never ran the JKS KeyProtector, so
   rustls got an unparseable key. Implemented `jks_recover_key` (keystore.rs):
   extract the EPKI OCTET STRING, run the JKS SHA-1 keystream
   (`Wi=SHA1(passwdUtf16be||W(i-1))`, `W0=salt`), XOR → plaintext PKCS#8, verify
   the trailing digest. (Standalone-valuable bug fix — any JKS key consumer
   benefits; could be cherry-picked to dev after a regression run.)
2. **Synthetic enum results — FIXED.** `SSLEngine.getHandshakeStatus` and the
   `SSLEngineResult` accessors returned fresh synthetic int-slot enum objects, so
   `status == OK` / `== NEED_WRAP` identity checks always failed. Now build a
   REAL `SSLEngineResult` via its ctor with REAL `Status`/`HandshakeStatus`
   constants (via `valueOf`); the loopback (`.tooling/TlsLoop.java`) shows real
   enum names and the engine wrap/unwrap runs without the engine-level SO.

Remaining walls (each a focused debug):

A. **Connector-init native stack overflow (the blocker).** With identity
   installed, `TestSsl` dies with `EXCEPTION_STACK_OVERFLOW` during connector
   *init*, right after `JSSEUtil.initialise()` (so in the real-JSSE
   SSLContext/KeyManagerFactory setup bytecode, not wrap/unwrap — the engine is
   fine in loopback). It's an OS-level SO faulting in a **DLL frame** and is
   **not** caught by `CRATONVM_EXEC_DEPTH_CEILING` → pure-native recursion in the
   real `sun.security.ssl` init path. Needs a release-with-debug build +
   `CRATONVM_SYMBOLIZE` on the faulting RVA, or bisection (e.g. temporarily
   revert the re6 `createSSLEngine`/`getSupportedSSLParameters` one at a time) to
   pin which real-JSSE call recurses.
B. **`KeyStore.aliases()` returns `[]`** though the load-time hook saw the
   entries — a `set_store_id`/`get_store_id` round-trip glitch on the real
   `JavaKeyStore$JKS` object (same class as the JarFile `jzfile` handle, BUG-X).

The branch (`e22ec46c`) preserves all of the above; dev stays at the clean
factory-layer state (no regression).
