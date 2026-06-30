# Tribes `TestEncryptInterceptorAlgorithms` — native Cipher shim mode/validation gaps (OPEN)

**Suite:** `org.apache.catalina.tribes.group.interceptors.TestEncryptInterceptorAlgorithms`
**Status:** OPEN — 4/32 fail on CratonVM (real-JDK, JIT on **and** off); PASS on HotSpot.
**Not** the JceSecurity policy NPE — that was a *different* class
(`TestEncryptInterceptor`, fixed on `dev` `68987727`). This doc is the residual.

## The 4 failures and their two distinct causes

```
testAlgorithm[4 AES/CCM/NoPadding]    -> AssertionError: "...mode is not being refused"
testAlgorithm[5 AES/CCM/PKCS5Padding] -> AssertionError: "...mode is not being refused"
testAlgorithm[7 AES/CFB/PKCS5Padding] -> IllegalStateException: Cipher mode 'CFB' not implemented in WP6.3 dispatch
testAlgorithm[25 AES/OFB/PKCS5Padding]-> IllegalStateException: Cipher mode 'OFB' not implemented in WP6.3 dispatch
```

### Cause 1 — native `Cipher.getInstance` shim accepts everything (CCM)
`EncryptInterceptor.createEncryptionManager` lets unrecognised modes through
(comment: "Unrecognised modes ... are allowed but will be rejected if there is
no JCA provider to support them"), then `createCipher()` →
`Cipher.getInstance("AES/CCM/...","SunJCE")`. On HotSpot SunJCE has **no CCM**, so
this throws `NoSuchAlgorithmException`, which the interceptor rethrows as
`IllegalArgumentException` → the test's `doTestShouldNotSucceed` passes.

CratonVM intercepts `Cipher.getInstance(String,String)` natively
(`native-builtins/src/jca/cipher.rs::register_cipher_dispatch`, `cipher_alloc`)
and returns a synthetic Cipher for **any** transformation string — CCM included —
so nothing is thrown and the test sees the algorithm "not being refused".

Verified HotSpot behaviour (`java CipherCheck`):
`AES/CCM/NoPadding` and `AES/CCM/PKCS5Padding` → `NoSuchAlgorithmException: No such algorithm`.

### Cause 2 — native AES dispatch implements only GCM/CBC/CTR (CFB, OFB)
`AES/CFB/PKCS5Padding` and `AES/OFB/PKCS5Padding` are **created fine** on HotSpot
(SunJCE supports them) and the test expects a working encrypt/decrypt round-trip.
CratonVM's native `cipher_do_final` (WP6.3 dispatch in `phases_early.rs`) only
implements GCM/CBC/CTR/ChaCha20 and throws
`IllegalStateException: Cipher mode 'CFB'/'OFB' not implemented in WP6.3 dispatch`.

## Proper fix (deferred)
The real-bytecode-first direction ([[feedback_real_java_default_synthetic_experimental]])
is to let real SunJCE `Cipher.getInstance(String,String)` bytecode run so the
provider validates transforms and runs real CFB/OFB — rather than widening the
synthetic shim. A narrower stopgap (if the shim stays) is: (a) reject
SunJCE-unsupported transforms (CCM) from `cipher_alloc` with
`NoSuchAlgorithmException`, and (b) add CFB/OFB to the AES dispatch in
`crypto_impl`. Both expand the synthetic crypto surface and carry regression risk
across the many crypto-using apps, so they are intentionally out of scope of the
JceSecurityManager policy fix.

## Repro
```
apps/tomcat-suite-runner/run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real \
  -Category all -RunName algos -Start 273 -Count 1 -TimeoutSec 120 -Exe <cratonvm.exe>
```
Index 273 = `TestEncryptInterceptorAlgorithms`.
