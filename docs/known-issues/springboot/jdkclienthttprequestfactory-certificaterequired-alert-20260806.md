# `connectWithSslBundle` occasionally gets a `CertificateRequired` alert from an embedded Tomcat that asks for no client certificate

**Status: OPEN — found 2026-08-06**, as a side observation while closing
[`jdk-httpclient-sslbundle-tls-handshake-eintr`](../../internal/fixed-suite-bugs/springboot/jdk-httpclient-sslbundle-tls-handshake-eintr-FIXED-20260806.md).
Not investigated.

## Symptom

`JdkClientHttpRequestFactoryBuilderTests.connectWithSslBundle(String="GET")`,
on the *secure* request — the one that is supposed to succeed:

```
java.io.IOException: HttpClient request failed: received fatal alert: CertificateRequired
	at org.springframework.http.client.JdkClientHttpRequest.executeInternal(JdkClientHttpRequest.java:118)
	at org.springframework.boot.http.client.AbstractClientHttpRequestFactoryBuilderTests.connectWithSslBundle(...:119)
```

`CertificateRequired` is the alert a TLS **server** sends when it demanded a
client certificate and got none. The test's connector
(`TomcatServletWebServerFactory` + `Ssl` from `test.jks`) sets no `clientAuth`,
so it should never demand one — and the client is CratonVM's own
`java.net.http.HttpClient`, talking to CratonVM's own embedded Tomcat, in the
same process.

## Rate

Azure Linux, 16 cores, load 31–72, the `springboot-jsonreader-deprecation-20260718`
fixture, single class per process:

| binary | runs | `CertificateRequired` |
|---|---:|---:|
| stock `origin/dev` @ `2f5887fb3` | 16 | 1 |
| `fix/tls-client-eintr-retry-20260806` | 16 | 1 |

Present on both sides at the same rate, which is what rules out the EINTR
branch. It has **not** been seen on Windows (0 in ~10 runs) — expected, since
several of these TLS races are load-sensitive and the Windows box was quiet.

## Where to start

The alert is emitted by the server, so the question is why CratonVM's rustls
**`ServerConfig`** for this connector sometimes carries a client-auth
requirement. The test stands up eight HTTPS Tomcats in one process
(`connectWithSslBundle` ×2, `connectWithSslBundleAndOptionsMismatch` ×2, and
their siblings), several of which *do* configure different TLS parameters — so
a shared or cached `ServerConfig`/`SslHostConfig` leaking client-auth state
between connectors in the same VM is the first thing to check. `t27_tls`'s
server config construction and any per-process memo around it are the code to
read.

Note the neighbouring, already-fixed page
`internal/fixed-suite-bugs/tomcat/21-tls-handshake-enforcement-gap-FIXED.md`
uses this same alert as a *deliberate* signature for a genuinely rejected
handshake, so grep hits for `CertificateRequired` are mostly that, not this.

## Affected classes

- `module/spring-boot-http-client` —
  `org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests`
  (`connectWithSslBundle`, ~1 run in 16 under load).
