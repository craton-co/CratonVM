# Tribes `TestEncryptInterceptor*` — JCE policy NPE + Cipher mode/validation gaps (FIXED)

**Suites:**
`org.apache.catalina.tribes.group.interceptors.TestEncryptInterceptor`,
`...TestEncryptInterceptorAlgorithms`
**Status:** FIXED on `dev` (`68987727`, `334324cf`). Verified real-JDK, JIT on **and** off.

Three distinct CratonVM-only defects, all in the native `javax.crypto` shim
(`native-builtins/src/jca/cipher.rs`). All PASS on HotSpot.

## Bug 1 — `JceSecurityManager.defaultPolicy` null NPE (`TestEncryptInterceptor`)
`test192/256BitKey` gate on `Cipher.getMaxAllowedKeyLength("AES") >= 192/256`,
real-JDK bytecode that routes `JceSecurityManager.getCryptoPermission →
getDefaultPermission → getstatic defaultPolicy → getPermissionCollection`.
We no-op `JceSecurity.<clinit>` (it would read the null synthetic
`Security.props` and throw "Missing mandatory jurisdiction policy files"), so
`JceSecurity.defaultPolicy` — and the `JceSecurityManager.defaultPolicy` copied
from it — is null → NPE.

**Fix (`68987727`):** intercept
`JceSecurityManager.getCryptoPermission(String)` → return
`CryptoAllPermission.INSTANCE`. JDK 9+ defaults `crypto.policy=unlimited`, where
the real method resolves to exactly that singleton (`maxKeySize =
Integer.MAX_VALUE`; the real bytecode has an `if_acmpne
CryptoAllPermission.INSTANCE` early-return). Faithful unlimited result, not a
stub. Reads the static via `ensure_class_initialized` +
`static_field_index_by_name` + `get_static_field`; CryptoAllPermission's
`<clinit>`/`<init>` read no Security/policy state. Purely additive (the path
always NPE'd). → `TestEncryptInterceptor` 12/12.

## Bug 2 — AES CFB/OFB not implemented (`TestEncryptInterceptorAlgorithms`)
`AES/CFB/PKCS5Padding` and `AES/OFB/PKCS5Padding` are `shouldSucceed`
round-trips on HotSpot (SunJCE supports both feedback modes). Our in-tree AES
(`cipher_do_final_impl`) only did GCM/CBC/CTR/ECB → threw
`IllegalStateException: Cipher mode 'CFB'/'OFB' not implemented in WP6.3 dispatch`.

**Fix (`334324cf`):** route AES `CFB`/`OFB` to the real SunJCE
`com/sun/crypto/provider/AESCipher$General` SPI via the existing
`drive_real_cipher` path (already used for CBC), forwarding the actual feedback
mode to `engineSetMode`.

## Bug 3 — CCM accepted instead of rejected (`TestEncryptInterceptorAlgorithms`)
`AES/CCM/{NoPadding,PKCS5Padding}` are `shouldNotSucceed`: SunJCE has no CCM
(it's a BouncyCastle AEAD mode), so HotSpot's
`Cipher.getInstance("AES/CCM/…","SunJCE")` throws `NoSuchAlgorithmException`,
which `EncryptInterceptor` rethrows as `IllegalArgumentException` → refused. Our
getInstance shim accepted **any** transform and returned a synthetic Cipher →
"mode is not being refused".

**Fix (`334324cf`):** `check_transformation_supported()` on the three
`Cipher.getInstance` overloads throws `java.security.NoSuchAlgorithmException`
(the catchable checked exception) for CCM, matching SunJCE.
→ `TestEncryptInterceptorAlgorithms` 32/32.

## Not fixed (separate, pre-existing)
`TestEncryptInterceptorLargeHeap.testHugePayload` —
`OutOfMemoryError (alloc_array length 1073741824)`: the test allocates a 1 GiB
payload that exceeds the suite runner's default `-Xmx`. Heap-ergonomics, not
crypto. Identical to baseline `overnight0629c`.

## Repro
```
apps/tomcat-suite-runner/run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real \
  -Category all -RunName algofix -Start 272 -Count 2 -TimeoutSec 120 -Exe <cratonvm.exe>
```
Index 272 = `TestEncryptInterceptor`, 273 = `TestEncryptInterceptorAlgorithms`.
