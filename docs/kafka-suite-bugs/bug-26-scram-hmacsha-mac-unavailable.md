# Bug 26 — SCRAM: `NoSuchAlgorithmException: Algorithm HmacSHA256 not available`

**Severity:** High (for SCRAM) — **4 classes fail every test (0-pass)**. HotSpot OK.

## Symptom
```
=> java.security.NoSuchAlgorithmException: Algorithm HmacSHA256 not available
```
`javax.crypto.Mac.getInstance("HmacSHA256")` (and `HmacSHA512`) is not resolvable on
CratonVM. SCRAM's `ScramFormatter` derives keys with `Mac`/`HMAC`, so every SCRAM test
fails at setup.

## Affected classes (4, all 0-pass)
`ScramCredentialUtilsTest`, `ScramFormatterTest`, `ScramMessagesTest`,
`ScramSaslServerTest`.

## Root cause (to pin down)
CratonVM's JCA provider list exposes `Mac` for some algorithms but not the
`HmacSHA256`/`HmacSHA512` SPIs. Either the SunJCE `HmacSHA256`/`512` `MacSpi` is not
registered, or `Mac.getInstance` lookup is case/alias-sensitive and misses the
`HmacSHA256` alias. Note `Mac`/`HmacSHA1` may work (PBKDF2 PRF uses HMAC-SHA1/256 per
`reference_pemfile_pbe_crypto`), so the gap is specifically the SHA-2 HMAC Mac
registration in the provider, not HMAC itself.

## Where to look
- The JCA provider registration for `Mac` algorithms (SunJCE) — add/route
  `HmacSHA256`/`HmacSHA512` (and likely `HmacSHA384`/`HmacSHA224`) to a real HMAC
  `MacSpi` over the existing `MessageDigest` SHA-256/512 (which CratonVM has — cf.
  `reference_lhm_view_order` SHA-224 work).
- Compare with how `Mac.getInstance("HmacSHA1")` resolves today and mirror it.

## Reproduce
```
cd apps/kafka/tests
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 ./kv2.exe -cp ".;$(cat cp.txt)" KRun \
  org.apache.kafka.common.security.scram.internals.ScramFormatterTest
```
A 3-line standalone repro: `Mac.getInstance("HmacSHA256")` + `init` + `doFinal` and
compare bytes to HotSpot (RFC 4231 test vectors).
