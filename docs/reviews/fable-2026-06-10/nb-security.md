# CratonVM Security/Crypto/Network Review — `native-builtins` security layer

**Module:** `nb-security`
**Reviewer:** Fable (Opus) — static review only, no builds run
**Date:** 2026-06-10
**Scope (~35k LOC):** `native-builtins/src/{crypto.rs, crypto_impl.rs, tls.rs, tls_impl.rs, t27_tls.rs, t3_impl.rs, x509_manager.rs, security_manager.rs, net_phase_e.rs, http2.rs, http_client.rs}` plus `jca/` and `security_manager/` subdirectories.

---

## Summary

The default-build cryptographic posture is **substantially better than the surrounding `legacy-synthetic-crypto` history suggests**, because the genuinely insecure / synthetic paths are feature-gated OFF by default:

- **TLS** uses real `native-tls` (client) and `rustls` 0.23 + ring (server, `t27_tls.rs`) with system root stores, WebPKI verification, SNI, and TLS 1.2+ enforcement. No `danger_accept_invalid_certs`-style escape hatches are compiled in. The synthetic TLS-1.3 handshake emulator (`tls_impl.rs`) is gated behind `legacy-synthetic-crypto` and does not run by default.
- **X.509 trust validation** has two implementations. The **real, wired-in** one (`x509_manager.rs::validate_chain`, registered via `register_x509_manager_real`, lib.rs:5227) is rigorous: clock checks, full-DN chain continuity, BasicConstraints CA on intermediates, trust-anchor match by full subject DER, real RSA/ECDSA signature verification, and **fails closed** (unknown sig OIDs → `CertificateException`, empty chains rejected). A second, **weaker** validator (`crypto_impl.rs::verify_cert_chain`) exists but is **not wired to the TLS path** (test-only) — it would be a CN-string bypass if ever adopted.
- **SecureRandom** correctly wraps the OS CSPRNG (RtlGenRandom / /dev/urandom) with a sane time+pid+thread fallback only on total OS-entropy failure. No stub PRNG in the default path.
- **AES-GCM / AES-CBC** use RustCrypto constant-time primitives via `jca/cipher.rs`.

The findings below are concentrated in: (1) a **reachable panic / DoS in RSA PKCS#1 v1.5 encode** from malicious cert chains; (2) **DER length-field integer overflow** panics from malicious certificates; (3) **HTTP chunked / HTTP-2 unbounded-allocation DoS** from malicious servers; (4) **synthetic/empty key material** for PQC (ML-KEM/ML-DSA) keygen in the default JCA path; (5) the HPACK read-side **Huffman-not-implemented** correctness stub.

---

## Bugs

### B1 (HIGH) — RSA PKCS#1 v1.5 encode underflows on small keys → panic/OOM, reachable from `checkServerTrusted`
`crypto_impl.rs:1719-1736` `Rsa::pkcs1v15_encode`:
```rust
let t_len = digest_info_prefix.len() + hash.len();   // 19 + 32 = 51 for SHA-256
let ps_len = k - t_len - 3;                           // unguarded usize subtraction
```
`k = (key.n.bit_length()+7)/8` flows from the **issuer public key** parsed out of an attacker-supplied certificate. A chain whose intermediate carries a tiny RSA modulus (e.g. 256-bit → `k=32`) makes `32 - 51 - 3` underflow: in debug a panic, in release a wrap to ~`usize::MAX` followed by `std::iter::repeat(0xff).take(ps_len)` → multi-EB allocation → abort. This is reachable through the real trust path: `x509_manager.rs:964 verify_one_signature → Rsa::verify_sha256 (crypto_impl.rs:1707) → pkcs1v15_encode`, which is driven by `checkServerTrusted`/`checkClientTrusted`/`PKIXValidator.engineValidate`. **Fix:** reject `k < t_len + 11` (return a non-matching block or `false`) before encoding.

### B2 (HIGH) — DER length-field integer overflow → slice panic on malicious certificate
`crypto_impl.rs:2374-2381` `der_read_tag_length` / `der_read_length`:
```rust
if data.len() < total_hdr + len { return None; }       // total_hdr + len can wrap
Some((total_hdr + len, &data[total_hdr..total_hdr + len]))
```
`der_read_length` (2383-2396) accepts up to 8 length bytes and accumulates with `len = (len<<8)|byte` with no cap, so `len` can reach near `usize::MAX`. `total_hdr + len` wraps in release; the guard passes; the subsequent slice `&data[total_hdr..total_hdr+len]` recomputes the wrapped end and can produce an inverted range → panic (`slice index starts at .. ends at ..`). Reachable from any DER fed to `X509Cert::parse_der` and the cert mirror/keystore loaders. Same unbounded-`len` pattern also affects `der_read_integer` (2398-2407). **Fix:** use `checked_add`, and reject `len > data.len()` (or a sane max) directly.

### B3 (HIGH) — HTTP/1 chunked decode: chunk-size overflow + pre-cap unbounded allocation
`http_client.rs:550-581` `read_chunked`:
- Line 550: `usize::from_str_radix(size_str, 16)` accepts a chunk size up to `usize::MAX` with no upper bound.
- Line 568: `while prefix.len() < size + 2` — `size + 2` overflows for a near-`usize::MAX` size (wraps to 1), the read loop exits, then line 577 `&prefix[..size]` panics (`size > prefix.len()`).
- Even for a merely large size (e.g. `0xFFFFFFFF` = 4 GiB), the loop buffers the **entire** chunk into `prefix` *before* the `MAX_RESPONSE_BODY` check at line 578 — a single chunk header forces a multi-GiB allocation from a malicious/compromised server → memory-exhaustion DoS. **Fix:** reject `size > MAX_RESPONSE_BODY` immediately after parsing the size line; use `checked_add`.

### B4 (MEDIUM) — HTTP/2 unbounded `header_block` accumulation across CONTINUATION frames
`http_client.rs:801, 815` — HEADERS (0x1) and CONTINUATION (0x9) frames do `header_block.extend_from_slice(...)` and only `clear()` on END_HEADERS (flag 0x4). A malicious server can stream CONTINUATION frames forever (each frame payload up to the 24-bit length = 16 MiB, `vec![0u8; length]` at line 755) without ever setting END_HEADERS → `header_block` grows unbounded → OOM. There is no `SETTINGS_MAX_FRAME_SIZE` enforcement (default should be 16 KiB) and no cap on total header-block size. **Fix:** cap accumulated `header_block` length and enforce a max frame size.

### B5 (MEDIUM) — HPACK integer decode can wrap silently
`http_client.rs:967` `decode_hpack_int`: `value += ((b & 0x7f) as usize) << shift;` — the `shift > 63` guard (972) bounds the shift but not the accumulation; `value` can wrap on the add. Impact is bounded (the result is used only as a static-table index, which is `.get()`-checked, or a string length, which is `rest.len() < len`-checked), so no memory unsafety — but a wrapped value yields a misleading error rather than a clean "integer too large". **Fix:** `checked_add` / cap to a sane max.

### B6 (LOW) — Non-strict PKCS#7 unpadding in AES/ECB decrypt
`jca/cipher.rs:444-448` reads only the last byte as the pad length and truncates, without verifying that all `pad` trailing bytes equal `pad`. Accepts malformed padding (lenient). Low impact in ECB context but diverges from JDK's strict `BadPaddingException`. (`crypto_impl.rs::pkcs7_unpad` used by CBC should be checked for the same; the CBC path returns `Result` so it likely validates — verify.)

### B7 (LOW) — `to_bytes_be_padded` silently truncates high bytes when value exceeds `len`
`crypto_impl.rs:1188-1194`: when `raw.len() >= len` it returns `raw[raw.len()-len..]` (drops the most-significant bytes) instead of erroring. In the RSA verify path this is self-cancelling (both sides re-encode), but as a general helper it can mask a sizing bug elsewhere. Consider asserting/erroring on overflow.

---

## Vulnerabilities

### V1 (HIGH, latent) — `verify_cert_chain` trusts intermediates and anchors by CN string equality
`crypto_impl.rs:2552-2554, 2602-2651`. `subject_cn_matches` compares only the extracted **CN string**, not the full DN or the public key. The walk prefers `next_in_chain` over the real anchor (`next_in_chain.or(anchor)`, 2627) and terminates with `Ok(())` whenever *any* trust anchor's CN equals `cert.issuer_cn` (2640), even though the signature was verified against the attacker-supplied chain cert's key — a classic chain-confusion bypass. **This function is currently NOT wired to the live TLS/TrustManager path** (only `x509_manager.rs::validate_chain` is, which is sound), so it is a latent vuln. Recommendation: delete `verify_cert_chain`/`subject_cn_matches` or gate them loudly so they cannot be adopted as the trust path. (Sound alternative already exists in `x509_manager.rs`.)

### V2 (MEDIUM) — Custom `TrustManager`/`KeyManager` silently ignored on the client path
`phases_late.rs:27041-27062` (`new13_build_connector`): when an app passes a non-null `TrustManager[]`/`KeyManager[]` to `SSLContext.init`, the native-tls connector still uses the **system** trust store, silently discarding the app's managers. The code comments frame this as "strictly safer." It is for the *accept-everything* TM, but it **breaks pinning / restrictive TrustManagers** (app intends to trust *only* a private CA or a pinned cert; the VM instead trusts the whole public PKI) and breaks private-CA-only deployments. Document as a behavioral gap; ideally route to the rustls-based path (`t27_tls.rs`) which supports pluggable verifiers when custom managers are supplied.

### V3 (MEDIUM) — `pkix_engine_validate` / fallback TrustManager state ignores the configured KeyStore
`x509_manager.rs:1546-1553, 1593-1602`: when no per-connection trust state is registered, both `do_check_trusted` and `pkix_engine_validate` fall back to `build_trust_manager_state(0)` (system roots only). Comment claims this matches JDK's implicit default. For `PKIXValidator.engineValidate` this means an app that constructed a `PKIXParameters` over a *custom* keystore may be validated against system roots instead. Verify that the custom keystore id is actually threaded through; if not, this can both over- and under-trust relative to intent.

### V4 (LOW) — `ObjectRef::from_raw` reconstructed from an attacker-influenceable `jlong`
`net_phase_e.rs:3354-3360`: a Java `long` is converted via `jlong_bits_as_aligned_object_ptr` into a raw pointer and wrapped with `unsafe { ObjectRef::from_raw(...) }`. This is the standard Unsafe-arena handle pattern and the helper presumably validates the 2^36 tag/alignment, but reconstructing an `ObjectRef` from a value that originates in Java `long` math is memory-unsafe if the validation is incomplete. Confirm `jlong_bits_as_aligned_object_ptr` rejects non-arena and unaligned values; treat as defense-in-depth.

### V5 (LOW) — ECDSA/RSA bignum arithmetic is not constant-time
`crypto_impl.rs` `BigUint`/`FieldElement256` modpow/modinv and ECDSA `sign_with_digest` use data-dependent branching. This is a timing side-channel for the in-tree primitives. The preferred path is real SunEC (constant-time-ish via ring on the rustls side / native EC), so impact is limited, but the in-tree RSA/ECDSA should carry a "not side-channel resistant" warning and not be advertised as production crypto.

---

## Stubs and Unimplemented

### S1 (forbidden synthetic) — PQC keygen returns EMPTY key material
`jca/key_factory.rs:316-321, 551-556`: `algo_idx` recognizes `ML-KEM-512/768/1024` and `ML-DSA-44/65/87`, but `kpg_generate_key_pair` only implements RSA and EC. All other recognized algorithms (the PQC set, plus X25519) hit the fallback that allocates `PublicKey`/`PrivateKey` synthetics with **empty DER and `key_id=0`** — i.e. a `KeyPair` with no key material, presented as if keygen succeeded. This is exactly the "synthetic stub faking app behavior" the project forbids. Default (non-`real_jca_mode`) build. **Fix:** throw `NoSuchAlgorithmException` for unimplemented algorithms, or implement them.

### S2 — HPACK Huffman decoding not implemented (read side returns garbled bytes)
`http_client.rs:988-992` `decode_hpack_string`: when the Huffman bit is set, the raw (still-Huffman-coded) bytes are returned via `from_utf8_lossy` instead of being decoded. Since HPACK encoders Huffman-code compressible strings by default, real servers will yield garbled `:status`/`content-type`/etc. This is a stub returning fake data for a common case. **Fix:** implement the RFC 7541 Huffman table on the read side.

### S3 — JShell native is a synthetic mini-evaluator
`t3_impl.rs:1330-1370, 1583+` `jshell_evaluate`: `jdk.jshell.JShell.eval` is backed by a hand-rolled arithmetic/string evaluator, not a real JShell. Faked tool behavior. Lower priority (tooling), but report-worthy under the no-synthetic-stubs policy.

### S4 — Single-shot crypto stubs retained (but correctly NOT registered)
`crypto_impl.rs:976-1011, 1117-1124`: `native_message_digest_digest` hardcodes SHA-256 regardless of algorithm and `native_message_digest_update` is a no-op that drops data. These are explicitly **disabled** (`_unused_single_shot_stubs` tuple, not registered) — the real accumulate/finalize path serves these. No live impact, but dead synthetic code that should be deleted to avoid accidental re-registration.

### S5 — x509_manager `NotImplemented` sig algorithms (DSA-SHA1, RSA-PSS, Ed25519)
`x509_manager.rs:983-998`: recognized-but-unimplemented OIDs return `TrustError::NotImplemented`, which the trust path turns into a `CertificateException` (fail-closed). This is the *correct* stub behavior (not a fake-success), noted for completeness — chains signed with RSA-PSS or Ed25519 will be rejected rather than mis-validated. Implementing these is a capability gap, not a vulnerability.

---

## Performance

### P1 — Per-call `SecureRandom::new()` allocation in HTTP/2 window updates and native handlers
`http_client.rs:843-846`: a `WINDOW_UPDATE` frame (a 13-byte `Vec`) is allocated per DATA frame; acceptable, but combined with per-frame `vec![0u8; length]` (755) on every frame, large transfers churn allocations. Consider a reusable scratch buffer.

### P2 — `SecureRandom::new()` re-seeds from the OS on every `nextBytes` native call
`crypto_impl.rs:951, 968`: each `SecureRandom.nextBytes`/`generateSeed` constructs a fresh `SecureRandom` (an OS-entropy syscall for seeding even though `next_bytes` then calls the OS again directly). The seed read is wasted work on the hot CSPRNG path. Cache or skip the seed read when `use_os_entropy` is true.

### P3 — `find_subslice` is naive O(n*m) and rescans the whole buffer each read
`http_client.rs:585-595`, called in the HTTP/1 head loop (433) and chunked loop (535) on every read iteration over the growing buffer → O(n²) for large heads/chunk streams. Track a search offset to avoid rescanning already-seen bytes.

### P4 — `extract_cn` / DER walkers `remove(0)` in a loop
`crypto_impl.rs:2405` `der_read_integer` does `while ... { bytes.remove(0); }` (O(n) shift per leading zero) and several name walkers re-slice repeatedly. Minor; only on cert parse.

### P5 — BigUint RSA keygen recomputes `extended_gcd` then `modinv` (two full passes)
`crypto_impl.rs:1686-1688`: `extended_gcd(&e, &phi)` is computed just to check coprimality, then `e.modinv(&phi)` recomputes essentially the same EEA. Use the gcd pass's Bézout coefficient directly. Only on keygen, but keygen is already slow.

### P6 — Per-`checkPermission` policy evaluation walks parsed policy generically
`security_manager.rs:467-473, 504-521`: `policy_allows_full_generic` is invoked on every permission check (the comments note ~50k calls during JDK boot). The Arc-based codebase/digest sharing already mitigates allocation; consider a per-(class,target,actions) decision cache keyed on the active code source.

---

## Tests

**Estimated coverage: ~55%.** Basis: counted `#[test]` blocks across the scope (≈635 total). The **crypto primitives and trust validation are well covered**; the **JCA wrapper / synthetic-key-shape layer and the network-parser DoS edges are thin.**

Well-tested (any-to-good coverage):
- `crypto_impl.rs` (104 tests): AES vectors, SHA-2, HMAC/HKDF, RSA/ECDSA sign+verify, DER, and `verify_cert_chain` happy/sad paths.
- `crypto.rs` (60), `tls.rs` (59), `http2.rs` (59), `security_manager.rs` (52) + `security_manager/policy.rs` (51): substantial unit + a couple of live-socket integration tests.
- `x509_manager.rs` (18): expired/no-anchor/broken-continuity/non-CA-intermediate, **real RSA & ECDSA chain validate + tampered-signature rejection** — the most security-relevant validator is genuinely exercised.

Thin / missing:
- **`jca/key_factory.rs` (2 tests), `jca/signature.rs` (2), `jca/cipher.rs` (6):** the synthetic-key-shape layer, the empty-PQC-key fallback (S1), and the Cipher mode dispatch are barely covered. No test asserts that an unimplemented algorithm throws rather than returning an empty key.
- **No negative/fuzz tests for the untrusted parsers:** no test feeds malformed DER with oversized length fields (B2), a small-modulus RSA cert (B1), a near-`usize::MAX` chunk size (B3), or an unterminated CONTINUATION stream (B4). These are exactly the DoS surfaces.
- **No HPACK Huffman round-trip test** (S2) — the gap is invisible to the suite.
- `net_phase_e.rs` (12) and `http_client.rs` (22) lean on happy-path round-trips; the chunked/h2 framing edge cases are untested.

**Does it plausibly reach 85%?** No. Reaching 85% would require: (a) malformed-input/fuzz tests for `der_read_*`, `parse_der`, `read_chunked`, the h2 frame loop, and `decode_hpack_*`; (b) JCA-layer tests asserting fail-closed on unimplemented algorithms and verifying real key DER shape; (c) Huffman decode tests; (d) a test that `checkServerTrusted` rejects a small-RSA-key chain without panicking.

Most important missing tests (priority order):
1. `checkServerTrusted` with an attacker chain carrying a 256-bit RSA modulus → must reject, must not panic (covers B1).
2. `X509Cert::parse_der` / `validate_chain` fed DER with an 8-byte oversized length field → must return error, must not panic (B2).
3. `read_chunked` with chunk size `FFFFFFFFFFFFFFFF` and with `7FFFFFFF` → bounded error, no OOM/panic (B3).
4. h2 response with endless CONTINUATION (no END_HEADERS) → bounded error (B4).
5. `KeyPairGenerator.getInstance("ML-DSA-65").generateKeyPair()` → must throw, must not return an empty key (S1).
6. HPACK Huffman-coded `:status`/`content-type` decode round-trip (S2).

---

## Feature Suggestions

1. **Unify on the rigorous trust path.** Delete `crypto_impl.rs::verify_cert_chain`/`subject_cn_matches` (V1) and make `x509_manager.rs::validate_chain` the single chain validator. Add `pathLenConstraint`, `keyUsage(keyCertSign)`, and EKU checks to bring it to RFC 5280 §6 parity.
2. **Route custom TrustManagers through rustls.** When `SSLContext.init` receives non-null managers, build a `rustls::ClientConfig` with a `ServerCertVerifier` that calls back into the Java `X509TrustManager` (the `t27_tls.rs` rustls layer already exists) instead of silently using system roots (V2).
3. **Implement HPACK Huffman decode** (S2) and enforce HTTP/2 `SETTINGS_MAX_FRAME_SIZE` + a header-list size limit (B4) for a spec-compliant, DoS-resistant client.
4. **Fail-closed JCA algorithm dispatch + real PQC.** Make unimplemented `KeyPairGenerator`/`Signature` algorithms throw `NoSuchAlgorithmException` (removes S1's fake keys); optionally back ML-KEM/ML-DSA with a vetted PQC crate.
5. **Harden all untrusted-input length math.** Sweep `der_read_length`, `read_chunked`, `decode_hpack_int`, and `pkcs1v15_encode` to use `checked_add`/explicit caps (B1–B3, B5); add a `cargo fuzz` target for `parse_der` and `read_http1_response`.
6. **Constant-time crypto labeling.** Mark the in-tree `BigUint`/ECDSA primitives as non-side-channel-resistant and prefer the OS/ring-backed paths for any externally observable operation (V5).

---

## Files sampled vs fully read

**Read in full (or near-full, all security-relevant regions):**
- `crypto_impl.rs` — SecureRandom (793-935), RSA keygen/sign/verify/PKCS#1 (1550-1779), ECDSA verify (2120-2180), DER/X.509 parser (2373-2555), PKIX `verify_cert_chain` (2561-2651), CN/OID/time helpers (2653-2740). Structure-grepped the BigUint/AES-GCM/CBC regions and read AES-CBC/ECB (180-236).
- `x509_manager.rs` — `validate_chain` (824-934), `verify_one_signature` (944-998), TrustManager handlers (1505-1615), registration (795-895 cross-ref).
- `jca/key_factory.rs` — keygen dispatch (190-557), registration + `real_jca_mode` gating (795-895).
- `jca/cipher.rs` — `cipher_do_final_impl` mode dispatch (324-475); structure-grepped the rest.
- `http_client.rs` — HTTP/1 response + chunked (420-595), HTTP/2 frame loop + DATA/HEADERS (740-849), HPACK decode (871-999).
- `security_manager.rs` — checkPermission core (460-557); structure-grepped registration and the rest.
- `phases_late.rs` — SSL/TLS connector + SSLContext registration (27040-27160) for the live TLS posture (cross-file, in support of scope).
- `t27_tls.rs` — header/security posture + rustls config (1-60, 2470-2530); grepped all verifier construction.

**Sampled (header + targeted greps, not line-by-line):**
- `tls.rs` (confirmed `tls_impl` gating + cipher constants), `tls_impl.rs` (confirmed feature-gated OFF), `t3_impl.rs` (JShell stub + JNDI), `net_phase_e.rs` (unsafe ObjectRef reconstruction + grep for shell-out/SSRF — none found), `http2.rs` (HPACK static table + frame-type tests; no dynamic-table decoder), `jca/signature.rs`, `jca/provider_chain.rs`, `jca/message_digest.rs`, `jca/asn1.rs`, `jca/x500.rs`, `security_manager/policy.rs`, `security_manager/x509.rs`.

**Not deep-read (low risk or out of crypto-correctness focus):** the bulk of `provider_chain.rs` (2255 LOC provider registry), `asn1.rs`/`x500.rs` DER name encoders (encode-side), and `security_manager/policy.rs` policy-file parser internals beyond the implies/permission core.
