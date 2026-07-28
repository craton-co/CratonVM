# `getInstance(type, unregisteredProviderName)` loses the provider name from its exception — FIXED

**Status: FIXED — filed 2026-07-23, closed 2026-07-28**

## Symptom (as filed)

```
java.lang.AssertionError:
Expecting throwable message:
  "Unable to create key store: PKCS12 not found"
to contain:
  "com.example.KeyStoreProvider"
but did not.

java.lang.IllegalStateException: Unable to create key store: PKCS12 not found
	at org.springframework.boot.ssl.jks.JksSslStoreBundle.createKeyStore(JksSslStoreBundle.java:114)
Caused by: java.security.KeyStoreException: PKCS12 not found
Caused by: java.security.NoSuchAlgorithmException: no KeyStore PKCS12 implementation for provider com.example.KeyStoreProvider
```

`JksSslStoreBundleTests.whenHasKeyStoreProvider()` / `.whenHasTrustStoreProvider()`
name a provider (`"com.example.KeyStoreProvider"`) that is never registered and
expect the resulting exception to mention it. Real
`KeyStore.getInstance(String, String)` only catches `NoSuchAlgorithmException`
and rewrites it to `KeyStoreException(type + " not found", cause)`, discarding
the message; `NoSuchProviderException` is declared but NOT caught, so it
propagates with the provider name intact. CratonVM produced the former.

## Root cause

The filed doc could not pin the divergence and recorded a hypothesis, noting
that "no CratonVM native override of `Security.getImpl` itself, `GetInstance`,
or `Provider.getService` was found in `native-builtins/src/*.rs`". **That search
was the miss**: the override does exist, as
`getinstance_instance_provider` in `native-builtins/src/jca/provider_chain.rs`
— a *subdirectory*, which the `src/*.rs` glob does not cover. It has been there
since `a9244b596` (2026-06-02), well before the doc was filed, and it went
straight to algorithm lookup without ever resolving the provider name. That is
exactly the "falls through into algorithm-lookup logic instead" behaviour the
doc predicted; it just lives one directory deeper than where it was looked for.

Real `sun.security.jca.GetInstance.getInstance` resolves the provider FIRST:
an unregistered name is `NoSuchProviderException("no such provider: X")`, an
empty one is `IllegalArgumentException("missing provider")`, and only then is
the algorithm looked up.

## Resolution

### 1. The two filed methods — fixed 2026-07-23, before this session

`e6d532776` (core39 cluster D, item 3) added the `find(&provider).is_none()`
→ `throw_no_such_provider` check to `getinstance_instance_provider`.
`JksSslStoreBundleTests` runs **14/14 green** on `dev`, JIT and `--nojit`.

### 2. The same defect on four other engines — fixed here

Closing the doc on the two asserted methods alone would have been wrong. A
19-case probe
(`docs/internal/fixed-suite-bugs/repros/jca-provider-lookup-parity/ProviderLookupProbe.java`,
HotSpot 25 reference alongside it) found **15 divergent lines**, because
`KeyFactory`, `Signature`, `SecureRandom` and `Cipher` never reach
`Security.getImpl` at all: each registers ONE native for all three
`getInstance` overloads and reads only argument 0, so the provider argument was
discarded outright. `KeyFactory.getInstance("RSA", "com.example.KeyStoreProvider")`
**succeeded** — the identical "provider name swallowed" defect this doc is
about, on four engines it was never checked against.

Fixed by a shared `provider_chain::check_named_provider_arg`, called from each
of those natives (and from both registration sites for `Cipher`/`Signature` —
`jca/*.rs` and `phases_early.rs` — so whichever wins behaves the same). It
discriminates the `(algorithm, String)` overload from `(algorithm, Provider)`
by the argument's actual class rather than by whether `read_string` happens to
succeed, and is a no-op for the single-argument form.

Three further parity gaps the probe exposed in the same path:

| Case | Real JDK 25 | CratonVM before |
| --- | --- | --- |
| `MessageDigest.getInstance("SHA-256", "SUN")` | works | `NoSuchAlgorithmException` |
| `MessageDigest.getInstance("NoSuchAlgo")` | `NoSuchAlgorithmException` | `java.lang.SecurityException` |
| `KeyStore.getInstance("NoSuchType", "SUN")` | `no such algorithm: NoSuchType for provider SUN` | `no KeyStore NoSuchType implementation for provider SUN` |
| `KeyStore.getInstance("NoSuchType")` | `NoSuchType KeyStore not available` | `no KeyStore NoSuchType implementation in any provider` |

- **`MessageDigest.getInstance(algo, provider)` failed for a digest the
  single-argument form served happily.** Only the one-argument overload was
  natively registered; the two-argument ones fell through to real bytecode →
  `Security.getImpl` → the provider-chain *service table*, which has no
  `MessageDigest` entry under `"SUN"`. Ordinary correct application code
  (`MessageDigest.getInstance("SHA-256", "SUN")`) threw. Both overloads are now
  registered and routed at the same intrinsic, after the provider check.
- **`md_get_instance` raised `SecurityException`** for an unsupported
  algorithm, on a stale premise recorded in its own comment ("cratonvm's
  nearest mapping is SecurityException (we don't carry NSAE)"). The crate does
  carry it — `throw_no_such_algorithm` builds a genuine
  `java/security/NoSuchAlgorithmException`, and `KeyFactory` already used it.
  Because `SecurityException` is unchecked, it sailed past every
  `catch (NoSuchAlgorithmException)` handler instead of being handled.
- Message wording for both `NoSuchAlgorithmException` shapes now matches.

`Cipher` needed its own spelling throughout: it rolls its own provider lookup
rather than going through `GetInstance`, and capitalises both messages
(`"No such provider: X"`, `"Missing provider"`). Verified on HotSpot, not
assumed.

## Verification

Fresh release binary `cratonvm-ksprov-fixed.exe`, branch
`fix/base64-decoder-msg-closure-20260727`.

**Probe:** 15 divergent lines → **2**, identical under JIT and `--nojit`.

**Spring Boot**, `run-single-class.ps1`, all before/after:

| Module | Class | before | after |
| --- | --- | --- | --- |
| core/spring-boot | `JksSslStoreBundleTests` | 14/14 | 14/14 |
| core/spring-boot | `Base64ProtocolResolverTests` | 3/3 | 3/3 |
| core/spring-boot | `AppendableByteArrayTests` | 4/4 | 4/4 |
| core/spring-boot | 10 × `ssl/*Tests` | all green | all green |
| core/spring-boot | 5 × `ssl/pem/*Tests` | all green | all green |
| core/spring-boot | `PemSslStoreTests` | 2/5 | 2/5 |
| core/spring-boot-autoconfigure | `BundleContentPropertyTests` | 9/9 | 9/9 |
| core/spring-boot-autoconfigure | `PropertiesSslBundleTests` | 6/6 | 6/6 |
| core/spring-boot-autoconfigure | `SslAutoConfigurationTests` | 4/4 | 4/4 |
| core/spring-boot-autoconfigure | `SslPropertiesBundleRegistrarTests` | 7/7 | 7/7 |
| core/spring-boot-autoconfigure | `BundleContentNotWatchableFailureAnalyzerTests` | 2/2 | 2/2 |
| core/spring-boot-autoconfigure | `CertificateMatcherTests` | 4 containersFailed | 4 containersFailed |

`PemSslStoreTests` (Mockito cannot mock `X509Certificate`) and
`CertificateMatcherTests` are pre-existing and **byte-identical before and
after** — neither is a JCA defect.

## Known residual (NOT fixed here)

A **registered** provider that does not own the requested algorithm still
succeeds on the algorithm-only engines, where real JDK throws:

```
KeyFactory.getInstance("RSA", "SUN")               HotSpot: NoSuchAlgorithmException   CratonVM: OK
Cipher.getInstance("AES/CBC/PKCS5Padding", "SUN")  HotSpot: NoSuchAlgorithmException   CratonVM: OK
```

These are the 2 remaining probe lines. This is a *permissiveness* gap, not the
swallowed-provider-name defect this doc covers: the name is now resolved and
honoured, it is provider→algorithm **ownership** that is not enforced. Doing so
requires the provider-chain service tables to be complete for every engine —
they are deliberately sparse, with several providers seeded as
`COVERAGE_UNBACKED` — so tightening it would reject calls that work today and
needs its own suite-wide validation pass. Filed as
`docs/known-issues/springboot/jca-provider-algorithm-ownership-not-enforced-20260728.md`.

## Reproducing

```
javac -d <out> docs/internal/fixed-suite-bugs/repros/jca-provider-lookup-parity/ProviderLookupProbe.java
java -cp <out> ProviderLookupProbe > hotspot.txt
cratonvm.exe --java-home <jdk25> -cp <out> ProviderLookupProbe > craton.txt
diff hotspot.txt craton.txt      # expect only the 2 ownership lines above
```

## Affected classes

| Module | Class | Result |
|---|---|---|
| core/spring-boot | org.springframework.boot.ssl.jks.JksSslStoreBundleTests (`whenHasKeyStoreProvider`, `whenHasTrustStoreProvider`) | PASS |
