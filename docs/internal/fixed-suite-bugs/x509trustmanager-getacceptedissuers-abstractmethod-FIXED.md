# X509TrustManager.getAcceptedIssuers AbstractMethodError

Status: FIXED 2026-07-08 on `codex/fix-x509-trustmanager-20260708`.

## Symptom

Spring WebFlux `InvalidHttpMethodIntegrationTests` failed identically across Jetty, Jetty Core, Reactor Netty, and Tomcat with:

```text
java.lang.AbstractMethodError: method javax/net/ssl/X509TrustManager.getAcceptedIssuers()[Ljava/security/cert/X509Certificate; has no Code attribute
```

The common factor across those HTTP server backends was CratonVM's JSSE/native TLS model, not a backend-specific TLS stack.

## Root Cause

Two `TrustManagerFactory.getTrustManagers()` producers could still allocate a synthetic object stamped with the bare `javax/net/ssl/X509TrustManager` interface. That is dangerous in real-JDK mode: a direct `getAcceptedIssuers()` call can resolve to the abstract interface declaration, which has no Code attribute, instead of to the concrete/native `sun/security/ssl/X509TrustManagerImpl` mirror.

The `phases_late.rs` public `javax.net.ssl.TrustManagerFactory` path had already learned to return `X509TrustManagerImpl` when an init overload captured a nonzero `tm_registry` id, but its default/null-initialized path still fell back to the bare interface object. The older `tls.rs` fallback path did the same.

## Fix

Both producers now return the real-impl-shaped `sun/security/ssl/X509TrustManagerImpl` mirror and stamp it through `x509_manager::set_tm_id`. A default/null-initialized factory uses id `0`; the existing `x509_manager` handlers intentionally interpret that as platform-default trust roots.

This keeps `getAcceptedIssuers`, `checkClientTrusted`, and `checkServerTrusted` on concrete native-backed dispatch instead of relying on an abstract interface registration rescue.

## Verification

Verified on the Azure host with a fresh release binary copied to `/data/data/cratonvm-bins/cratonvm-x509trustmanager-20260708.bin`:

```bash
cargo test -p cratonvm-native-builtins nb_tls_tmf_get_trust_managers_propagates_keystore_id
cargo test -p cratonvm-native-builtins trust_manager_factory_default_returns_concrete_x509_impl
cargo build --release -p cratonvm-cli --bin cratonvm
```

A minimal real-JDK probe now reports:

```text
tmClass=sun.security.ssl.X509TrustManagerImpl
issuers=121
```

The original Spring suite reproduction now passes all four parameterized backends:

```text
RESULT org.springframework.web.reactive.function.server.InvalidHttpMethodIntegrationTests found=4 succ=4 fail=0 skip=0 abort=0 ms=11239 status=OK
```
