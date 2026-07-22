# Bug TC0622 — `PBEWithMD5AndDES SecretKeyFactory not available` (PBE SecretKeyFactory unimplemented)

> **RESOLVED 2026-06-29 — merged dev `58723590`** (fix commit `863eb69c`,
> branch `claude/hungry-aryabhata-300730`). `TestSecretKeyCredentialHandler`
> now **4/4** (was 1 failure). The native PBE path (`pbe_generate_secret` —
> SunJCE's `PBEKey.getEncoded()` returns the 7-bit ASCII password, no key
> derivation at factory time) already existed but was gated default-off behind
> the broad `PBEWith*` prefix (`CRATONVM_NATIVE_PBE_KEYFACTORY=1`). Fix:
> `pbkdf2_get_instance` now recognizes, **by default**, the exact allowlist of
> real SunJCE `PBEKeyFactory` algorithm names (`is_known_pbe_keyfactory_alg`,
> `phases_early.rs`) — so `PBEWithMD5AndDES` works while unknown `PBEWith*`
> names still throw (HotSpot-faithful, mapped from `NoSuchAlgorithmException`).
> The broad prefix stays opt-in via the env var. The "implement the PKCS#5 v1.5
> MD5+DES KDF" recommendation below was a **misdiagnosis**: the SecretKeyFactory
> does NOT derive a key — getEncoded is the password bytes; the MD5+DES PBKDF1
> only runs later inside a `Cipher`. PBKDF2 path untouched; SHA-512/256
> unaffected. See [[reference_tc0622_pbe_secretkeyfactory]].

> **Root cause (one line):** CratonVM's native `SecretKeyFactory.getInstance`
> override (`phases_early::pbkdf2_get_instance`) *unconditionally* intercepts
> every `SecretKeyFactory.getInstance(...)` call but only recognizes
> `PBKDF2WithHmacSHA1/224/256`; for any other algorithm it throws the same
> `SecurityException: <alg> SecretKeyFactory not available` the (also-absent) real
> SunJCE provider would have thrown — so `PBEWithMD5AndDES` (a PKCS#5 v1.5 PBE
> factory present on HotSpot) has no implementation and no real-provider fallback.

**Severity:** Medium (one Tomcat realm test class; PBE-based credential storage
unusable, but PBKDF2 — the modern path — works).
**Status on CratonVM:** FAIL. **HotSpot:** PASS.
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`).
**Affected classes (1):**
`org.apache.catalina.realm.TestSecretKeyCredentialHandler`.

## Symptom

`testGeneral` iterates two algorithms — `PBKDF2WithHmacSHA1` then
`PBEWithMD5AndDES` (`ALGORITHMS = { "PBKDF2WithHmacSHA1", "PBEWithMD5AndDES" }`,
test line 26). The PBKDF2 half runs to completion; the moment the loop reaches
`PBEWithMD5AndDES`, `SecretKeyCredentialHandler.setAlgorithm` calls
`SecretKeyFactory.getInstance("PBEWithMD5AndDES")` and CratonVM throws:

```
java.lang.SecurityException: PBEWithMD5AndDES SecretKeyFactory not available
    at org.apache.catalina.realm.SecretKeyCredentialHandler.setAlgorithm(SecretKeyCredentialHandler.java:73)
    at org.apache.catalina.realm.TestSecretKeyCredentialHandler.doTest(TestSecretKeyCredentialHandler.java:66)
    at org.apache.catalina.realm.TestSecretKeyCredentialHandler.testGeneral(TestSecretKeyCredentialHandler.java:39)
...
Tests run: 4,  Failures: 1
```

The three preceding `WARN ... Unable to generate a password based key
(IllegalArgumentException: invalid keyLength / salt must not be empty /
invalid iterationCount)` lines are **not** the bug — they are the expected
`expectMatch=false`-style validation rejections inside the *working* PBKDF2
synthetic `generateSecret` (`phases_early::pbkdf2_generate_secret`) for the
zero/short parameter combinations, and HotSpot logs them too.

### Algorithms the test requests, and their CratonVM status
| Algorithm | CratonVM | Notes |
|---|---|---|
| `PBKDF2WithHmacSHA1` | **available** | handled natively (`pbkdf2_prf_code` → PRF 1) |
| `PBEWithMD5AndDES` | **MISSING** | not in `pbkdf2_prf_code` → `None` → SecurityException |

(`PBKDF2WithHmacSHA224`/`SHA256` are also recognized natively but the test does
not request them.)

## Root cause (which algorithms / where registered)

Two native registrations make `pbkdf2_get_instance` the handler for *all*
`SecretKeyFactory.getInstance` overloads:

- `native-builtins/src/phases_early.rs:9576-9587`
  (`javax/crypto/SecretKeyFactory.getInstance(String)` and
  `(String,String)` → `pbkdf2_get_instance`), plus the duplicate registration in
  `native-builtins/src/jca/cipher.rs:1187,1193`.

`pbkdf2_get_instance` (`phases_early.rs:10550`) maps the algorithm name through
`pbkdf2_prf_code` (`phases_early.rs:10470`):

```rust
"PBKDF2WithHmacSHA1"   => Some(1),
"PBKDF2WithHmacSHA224" => Some(224),
"PBKDF2WithHmacSHA256" => Some(256),
_ => None,
```

and on `None` returns:

```rust
Err(RuntimeError::SecurityException {
    message: format!("{alg} SecretKeyFactory not available"),
})
```

So `PBEWithMD5AndDES` falls into the `None` arm. Because the native override is a
**blanket intercept** of `SecretKeyFactory.getInstance`, the real SunJCE
`PBEWithMD5AndDES` SecretKeyFactory service never gets a chance to run — and that
real provider service is *also* not registered in CratonVM's JCA provider list
(the comment block at `phases_early.rs:9568-9573` notes the real path "throws
... SecretKeyFactory not available (no provider service)"). The result is that
`PBEWithMD5AndDES` (a PKCS#5 v1.5 password-based-encryption SecretKeyFactory that
HotSpot's SunJCE ships) has **neither** a native implementation **nor** a real
fallback. This is a *capability* gap, not merely a registration/name typo: PBE
key derivation (MD5 + DES with the PKCS#5 v1.5 KDF) is genuinely unimplemented.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore org.apache.catalina.realm.TestSecretKeyCredentialHandler
```

Minimal standalone repro:
`javax.crypto.SecretKeyFactory.getInstance("PBEWithMD5AndDES");`
(throws `SecurityException` on CratonVM, succeeds on HotSpot).

## Recommendation

**HANDOFF (crypto).** This is not a one-line registration fix: there is no real
SunJCE provider service behind the blanket native intercept, so making
`PBEWithMD5AndDES` work requires *implementing* the PKCS#5 v1.5 PBE key
derivation (MD5 digest + DES key/IV derivation) natively — analogous to the
existing native PBKDF2 path (`pbkdf2_generate_secret`) but with the v1.5 KDF and
a DES-keyed `SecretKey`. Suggested shape:

1. Extend `pbkdf2_prf_code` / `pbkdf2_get_instance` (or add a sibling matcher)
   so `PBEWithMD5AndDES` returns a recognized factory instead of `None`.
2. Add a native `generateSecret` branch that runs the PKCS#5 v1.5 PBE-MD5 KDF
   over password+salt+iterations and returns a real `PBEKey`/`SecretKeySpec`
   carrying the derived DES key (8-byte key + IV), so
   `SecretKeyCredentialHandler.mutate`/`matches` round-trips.
3. Verify against HotSpot's `PBEWithMD5AndDES` output for the test's parameter
   matrix (salt lengths {1,7,12,20}, iterations {1,2111,10000}, key lengths
   {8,111,256}).

Until then, the single affected class stays FAIL; the modern PBKDF2 credential
path is unaffected.
