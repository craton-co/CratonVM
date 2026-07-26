# Elasticsearch RandomizedTesting suite timeout accounting

Status: FIXED for `RestClientBuilderIntegTests` (2026-07-02, branch
`fix/es-restclient-suite-bugs-20260702`). `BinaryQuantizationTests`'
timeout-accounting entry below is UNRELATED (not CratonVM-only — HotSpot
fails it too) and untouched by this fix; left as a caveat, not a bug.

## Fix (RestClientBuilderIntegTests)

`Tests run: 0` meant the hang was in `@BeforeClass startHttpServer()` /
`getSslContext()`, before any test method could run. Root-caused via a
standalone repro (`javap -p -c -classpath jrt:/java.base <class>` on the
real JDK classes to find each missing/wrong piece) as a chain of SIX
distinct gaps in CratonVM's PKCS12-keystore / TLS-handshake plumbing, each
uncovered only after fixing the previous one:

1. **`AlgorithmParameters` service gap** — `AlgorithmParameters.getInstance
   ("PBEWithHmacSHA256AndAES_256")` (the default PKCS12 key-protection
   algorithm since JDK 8u191) dead-ended in `RuntimeError::NotImplemented`,
   which is deliberately left as an *uncatchable* VM-level abort (not a
   catchable Java exception) — fatal on the main thread, but on a spawned
   JUnit worker thread it likely hard-unwinds that thread without properly
   notifying whatever the harness is joining on, plausibly explaining why
   this surfaced as a suite HANG rather than a clean test failure. Fixed:
   `provider_chain::seed_sunjce_pbe_services` mirrors the SunJCE PBES2
   `AlgorithmParameters` service table (`com.sun.crypto.provider.
   PBES2Parameters$Hmac*AndAES_*`), same pattern as the existing
   `seed_sunec_services`/`seed_sunjsse_services`.
2. **Bare `"PBE"` `SecretKeyFactory` alias missing** —
   `PKCS12KeyStore.getPBEKey` requests `SecretKeyFactory.getInstance("PBE")`
   (literally, to derive the keystore's HMAC integrity-check key) — a SunJCE
   alias to the same password-bytes `PBEKeyFactory` family as
   `PBEWithMD5AndDES` etc. Added to
   `phases_early::is_known_pbe_keyfactory_alg`.
3. **PBES2 AES key derivation** — `Cipher.init(mode, key,
   AlgorithmParameters)` for `PBEWithHmacSHA*AndAES_*` fed the raw
   ~8-byte-ASCII password (the `PBEKey`'s undereived encoding) directly to
   AES as if it were the literal key (`InvalidKeyLength(8)`). Real SunJCE
   derives the AES key via PBKDF2 from the password + the
   `AlgorithmParameters`' embedded salt/iterationCount. Added
   `cipher_init_record_pbes2` (`../../../../native-builtins/src/jca/cipher.rs`), reusing
   the existing PBKDF2 math (`phases_early::pbkdf2_derive_for`) and routing
   the derived key/IV through the pre-existing real-bytecode
   `AESCipher$General` CBC delegation (`drive_real_cipher`).
4. **Auto-generated IV never created/persisted** — a fresh (2-arg)
   `PBEParameterSpec(salt, iterationCount)` carries no IV; real
   `PBES2Core.engineInit` auto-generates a random block-size IV and later
   `Cipher.getParameters()` returns a FRESH `AlgorithmParameters` reflecting
   it (for `PKCS12KeyStore.encryptPrivateKey`'s `new AlgorithmId(oid,
   cipher.getParameters())`, persisted for later decrypt). Added IV
   generation (`securerandom::os_random_bytes`) to
   `cipher_init_record_pbes2` and a new `Cipher.getParameters()` native that
   rebuilds a real `AlgorithmParameters`/`PBEParameterSpec`/
   `IvParameterSpec` from state tracked in `CipherState` (plain bytes, no
   `ObjectRef` — GC-safe).
5. **`HttpServer.getAddress()` echoed an unresolved address** —
   `net_phase_e.rs`'s `HttpServer.create` echoed the caller's raw hostname
   string with a null `InetAddress` (`hostString=localhost, addr=null`)
   instead of HotSpot's fully-resolved `/127.0.0.1:port`. Added
   `alloc_inet_socket_address_resolved`.
6. **`ServerSocketChannel.getLocalAddress()` hardcoded `"0.0.0.0"`** — a
   historical shortcut (`ssc_local_address`/`ss_wrapper_local_address` in
   `../../../../native-io/src/socket_channel.rs`) written when only `getLocalPort()`
   mattered (a Tomcat fix). `com.sun.net.httpserver.HttpsServer` (real
   `sun.net.httpserver.ServerImpl` bytecode, which — unlike the synthetic
   plaintext `HttpServer` (the `com.sun.net.httpserver.HttpServer` synthetic
   dispatch fixed under ES-HANG-02, see `..` history) —
   is NOT natively intercepted) binds via `ServerSocketChannel` and answers
   its own `getAddress()` from this native, so every `HttpsServer` reported
   `0.0.0.0` as its address — not a valid TLS connect target
   (`WSAEADDRNOTAVAIL`). Fixed to look up the real bound IP from the live
   `TcpListener` in the connection registry.

**Verified fixed**: the suite-timeout symptom itself (`Tests run: 0`,
`Suite timeout exceeded`) is gone — a fresh run now completes in ~19s with
`Tests run: 2`. See `elasticsearch-restclient-builder-ssl-handshake-residual.md`
(docs/known-issues/) for the 2 newly-exposed (and much narrower) test
failures this uncovered — a separate, follow-on issue, not a hang.

---

## Original report

## Summary

Some Elasticsearch tests fail under CratonVM with RandomizedTesting's suite
timeout even though the runner process exits quickly. This is a JUnit failure,
not the runner watchdog; the runner hang timeout was 300 seconds.

Representative failure:

```text
java.lang.Exception: Suite timeout exceeded (>= 580000 msec).
Tests run: 0, Failures: 1
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- `org.elasticsearch.client.RestClientBuilderIntegTests` is CratonVM-only:
  HotSpot passed it.
- `org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests` also
  reports suite-timeout accounting, but HotSpot failed that class too, so it is
  not counted as CratonVM-only.

Representative Craton-only row:

```text
index=15
module=client/rest
class=org.elasticsearch.client.RestClientBuilderIntegTests
CratonVM=FAIL, 12.457s
HotSpot=PASS, 7.394s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 15 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-randomized-suite-timeout-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

## No-JIT partial evidence

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. The partial no-JIT run reproduced both known timeout-accounting
classes:

```text
index=16  org.elasticsearch.client.RestClientBuilderIntegTests  FAIL/PASS versus HotSpot
index=1357 org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests FAIL/FAIL versus HotSpot
```

Evidence:

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
```
