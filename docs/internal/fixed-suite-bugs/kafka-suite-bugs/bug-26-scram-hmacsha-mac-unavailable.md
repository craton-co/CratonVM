# Bug 26 — SCRAM: `NoSuchAlgorithmException: Algorithm HmacSHA256 not available`

> **STATUS: CORE FIXED** (commit `435d3d73`, branch `kafka-suite-verify`).
> Root cause: the synthetic `javax.crypto.Mac` native (getInstance/init/update/doFinal,
> computing a real RFC-4231 HMAC) was registered only in `register_synthetic_overrides`,
> which **real-JDK mode never calls** — so the real `Mac.getInstance` bytecode ran and
> the real provider chain has no working HMAC `MacSpi`. **Fix:** promote
> `register_p68_crypto_mac` to `register_essential_natives` (the universal real-JDK
> path), matching Cipher/MessageDigest.
> **Result:** `ScramMessagesTest` 0/8 → **8/8**; `ScramFormatterTest` 0/2 → 1/2;
> `ScramCredentialUtilsTest` 0/6 → 1/6; `ScramSaslServerTest` 0/3 (now executes).
> **Residual (follow-on, previously masked):** the remaining failures throw
> `NullPointerException: null object argument` (a CratonVM native receiving a null at
> arg idx=2 of a 3-arg call) from inside **`ScramFormatter.hi()` (ScramFormatter.java:76)**
> — the HMAC iteration loop (`Hi`/PBKDF2). Pin with a symbolized/`eprintln`-instrumented
> build (release backtraces are `<unknown>`; `CRATONVM_DBG_NULL_NATIVE`/`DBG_ATHROW`
> only reach the JUnit rethrow). Likely the synthetic `Mac.doFinal()` returns null on a
> later loop iteration after its accumulator reset, or a `SecretKeySpec(key, algo)`
> path passes a null. Separate from the getInstance gap fixed here.
>
> **UPDATE — residual resolved in 2 of 3 layers (commit `09a7b4e3` / dev `016c8a88`):**
> - **L1 (getAlgorithm null):** `Mac.getInstance` read the algorithm from `args[1]`, but
>   static natives have no receiver placeholder (algo is `args[0]`, cf. `md_get_instance`)
>   → slot 0 null → `SecretKeySpec(key, null)` NPE. Fixed: pick the first non-null ref arg.
> - **L2 (MAC not initialized in the Hi() reuse loop):** the synthetic int init-flag
>   (slot 2, stored in a real `javax.crypto.Mac`) gets clobbered across init-once/
>   doFinal-many. Fixed: treat a Mac that still holds its key (slot 1) as initialized.
>   → **`ScramCredentialUtilsTest` 0/6 → 6/6**; `ScramMessagesTest` 8/8 (still green).
> - **L3 (FIXED — commit `7bfbfa47` / dev `e254ac08`):** `ScramFormatterTest.rfc7677Example`
>   failed an exact RFC-7677 vector (`array[0] expected 116, was -71`) because the
>   synthetic Mac stored algorithm/key/data/init in raw slots 0–3 of a REAL
>   `javax.crypto.Mac` object, aliasing that class's real fields and getting corrupted by
>   the allocation-heavy Hi() init-once/doFinal-many loop's GC → wrong `saltedPassword`.
>   **Fix:** moved all Mac state off-object into a process-wide table keyed by the
>   object's identity hash (stable across GC); the Mac handle is now opaque. Reworked
>   getInstance/init/update×3/doFinal×2/reset/getMacLength/getAlgorithm/clone.
>   **Result: `ScramFormatterTest` 1/2 → 2/2** (rfc7677 RFC vector passes); ScramMessages
>   8/8 and ScramCredentialUtils 6/6 still green. **all 4 SCRAM classes now fully green** (Formatter 2/2, Messages 8/8, CredentialUtils 6/6, SaslServer 3/3)
>   (from 0 at session start); ScramSaslServerTest 0/3 -> 3/3.

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
