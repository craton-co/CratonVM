# `X509TrustManager.getAcceptedIssuers()` AbstractMethodError across all HTTP server backends

| | |
|---|---|
| **Status** | OPEN, found 2026-07-07 during a Spring suite non-passed rerun on Azure (dev, real-JDK, JIT on). |
| **Area** | TLS/SSL native model — `javax.net.ssl.X509TrustManager` interface dispatch. |

## Symptom

```
java.lang.AbstractMethodError: method javax/net/ssl/X509TrustManager.getAcceptedIssuers()[Ljava/security/cert/X509Certificate; has no Code attribute
```

`org.springframework.web.reactive.function.server.InvalidHttpMethodIntegrationTests`
fails identically across **all 4** of its parameterized server backends:
`[1] Jetty`, `[2] Jetty Core`, `[3] Reactor Netty`, `[4] Tomcat`. Since the
failure is identical across backends that otherwise have very different TLS
stacks, the shared factor is almost certainly CratonVM's own
`X509TrustManager` model, not backend-specific code.

`AbstractMethodError: ... has no Code attribute` is CratonVM's standard
symptom for a method call resolving to an interface/abstract method slot
instead of the real (or synthetic) implementing method — i.e. dispatch landed
on a stub with no bytecode rather than the concrete override.

## Initial read

Somewhere in the real-JDK TLS bridge, an `X509TrustManager` instance is
constructed/vended whose `getAcceptedIssuers()` resolves to the interface
declaration itself rather than a concrete implementation (either a
JDK-internal `X509TrustManager` impl class, or a CratonVM synthetic one).
Worth checking:
- how `TrustManagerFactory.getTrustManagers()` / the default `SSLContext`
  vends trust managers in real-JDK mode,
- whether a synthetic/stub `X509TrustManager` is used as a placeholder
  somewhere in the SSL native code and its class metadata lacks the interface
  method's Code attribute.

## Reproduction

Azure host suite runner (path depths on this host shift between `/data/data/`
and `/data/data/data/` — verify with `ls -d` at both before trusting either):

```bash
WT=/data/data/wt-osr-nonpassed-20260706-1945   # prebuilt Spring suite + frozen binary
cd $WT/apps/spring-suite-runner
echo org.springframework.web.reactive.function.server.InvalidHttpMethodIntegrationTests > /tmp/list.txt
SF=$WT/apps/spring-framework RUNNER=$WT/apps/spring-suite-runner \
  CRATONVM_BIN=$WT/cratonvm-osr-nonpassed-20260706.bin JH=/data/data/jdk25-real \
  BATCH=1 BATCH_TO=120 ONE_TO=120 LIST=/tmp/list.txt OUT=/tmp/out SHARD_N=1 SHARD_ID=0 \
  bash suite-run.sh
# see /tmp/out/failcauses.log and /tmp/out/raw.log
```

HotSpot passes all 4 parameterized variants of this test.
