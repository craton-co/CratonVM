# `KeyStore.getInstance(type, unregisteredProviderName)` loses the provider name from its exception

**Status: OPEN — found 2026-07-23 (hypothesis, not fully pinned to file:line)**

## Symptom

```
java.lang.AssertionError:
Expecting throwable message:
  "Unable to create key store: PKCS12 not found"
to contain:
  "com.example.KeyStoreProvider"
but did not.

Throwable that failed the check:

java.lang.IllegalStateException: Unable to create key store: PKCS12 not found
	at org.springframework.boot.ssl.jks.JksSslStoreBundle.createKeyStore(JksSslStoreBundle.java:114)
	...
Caused by: java.security.KeyStoreException: PKCS12 not found
	at java.security.KeyStore.getInstance(KeyStore.java:939)
Caused by: java.security.NoSuchAlgorithmException: no KeyStore PKCS12 implementation for provider com.example.KeyStoreProvider
	at java.security.Security.getImpl(Security.java:762)
	at java.security.KeyStore.getInstance(KeyStore.java:936)
```

`JksSslStoreBundleTests.whenHasKeyStoreProvider()` and
`.whenHasTrustStoreProvider()` both pass a provider name
(`"com.example.KeyStoreProvider"`) that is never registered as a real
`java.security.Provider`, and expect the resulting exception to mention
that name (`withMessageContaining("com.example.KeyStoreProvider")`).

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard8/logs/core_spring-boot.org.springframework.boot.ssl.jks.JksSslStoreBundleTests.out.log`

## Root cause (hypothesis)

`JksSslStoreBundle.createKeyStore` (`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/ssl/jks/JksSslStoreBundle.java:96-121`)
calls `getKeyStoreInstance(type, provider)` →
`KeyStore.getInstance(type, provider)` and wraps whatever it throws as
`IllegalStateException("Unable to create %s store: %s".formatted(name,
ex.getMessage()))` — i.e. only the caught exception's own `getMessage()`
(not any nested cause) survives into the assertion the test checks.

Real `java.security.KeyStore.getInstance(String type, String provider)`
source:

```java
public static KeyStore getInstance(String type, String provider) ... {
    try {
        Object[] objs = Security.getImpl(type, "KeyStore", provider);
        return new KeyStore((KeyStoreSpi) objs[0], (Provider) objs[1], type);
    } catch (NoSuchAlgorithmException nsae) {
        throw new KeyStoreException(type + " not found", nsae);
    }
}
```

— it only ever catches `NoSuchAlgorithmException` and rewrites it to
`KeyStoreException(type + " not found", cause)`, discarding the original
message entirely. `NoSuchProviderException` (thrown instead, by
`GetInstance.getInstance`, when `Security.getProvider(provider) == null`)
is declared but **not** caught here, so it propagates straight through —
and *that* exception's message is (effectively) the provider name, which is
what the test expects to see.

CratonVM's `Security.getProvider(String)` is native-overridden
(`native-builtins/src/phases_early.rs:14066-14086`) and was checked here —
it correctly returns `null` for an unregistered name
(`provider_registry_find` misses → `Ok(Some(Value::Object(None)))`, matching
the JDK contract per its own comment). Despite that, the observed exception
is a `NoSuchAlgorithmException` with a message shaped like "no {engineName}
{algorithm} implementation for provider {provider}" — the wording used when
a provider *is* found but doesn't support the requested algorithm/type, not
the "provider not registered at all" case. That means somewhere between
`Security.getProvider` returning `null` and the exception actually thrown,
CratonVM's execution of `Security.getImpl`/`GetInstance.getInstance` does
not take the early `NoSuchProviderException` exit real HotSpot takes when
the provider lookup misses — it falls through into algorithm-lookup logic
instead. This was not traced further this session (`Security.getImpl` is
real, unmodified JDK bytecode as far as could be confirmed via line
numbers; no CratonVM native override of `Security.getImpl` itself,
`GetInstance`, or `Provider.getService` was found in
`native-builtins/src/*.rs`), so the exact divergence point is not pinned.

**What would confirm/refute:** trace (or add temporary logging around)
`sun.security.jca.GetInstance.getInstance(String, Class, String, String)`'s
`Security.getProvider(provider)` null-check on a minimal repro
(`KeyStore.getInstance("PKCS12", "no.such.Provider")`) to see whether the
`NoSuchProviderException` branch is ever reached on CratonVM at all, or
whether provider resolution silently falls back to iterating all registered
providers regardless of the requested name.

`JksSslStoreBundleTests` has one other, unrelated failure in the same run
(`invalidBase64EncodedLocationThrowsException`) — see
`../../internal/fixed-suite-bugs/springboot/core-spring-boot-base64-decoder-message-mismatch-20260723-FIXED.md`
for that one (fixed and retired 2026-07-28).

**Note added 2026-07-28 (while closing the base64 doc above):** the hypothesis
in this doc looks overtaken. `getinstance_instance_provider`
(`native-builtins/src/jca/provider_chain.rs`) now resolves the provider name
before the algorithm and raises `NoSuchProviderException` — item 3 of
`../../internal/fixed-suite-bugs/springboot/core39-clusterD-lifecycle-ssl-validation-FIXED.md`.
`JksSslStoreBundleTests` runs **14/14 green** on unmodified `origin/dev`
(`ccf774db3`), JIT and `--nojit`, including both methods listed below. This doc
was not independently re-root-caused in that session, so it is left OPEN for a
triage pass to confirm and retire rather than closed here.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.ssl.jks.JksSslStoreBundleTests (2 of 3 failures: `whenHasKeyStoreProvider`, `whenHasTrustStoreProvider`) |
