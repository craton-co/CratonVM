# `HttpsURLConnectionImpl.delegate` is null across the whole SSL/TLS + OCSP test cluster — OPEN, plus one unrelated fixture gap

**Status:** OPEN, not yet root-caused past the point of dispatch. One
NullPointerException explains essentially the entire SSL/TLS + OCSP portion
of Tomcat's non-passed set; not yet traced to why `delegate` is null.

**Found by:** cross-referencing the 2026-08-21 full 640-class 3-GC Tomcat run
(Azure, `dev@5b606e85e`) against
[nonpassed-class-census.md](nonpassed-class-census.md) (2026-08-14). Same
failures on all three collectors (ZGC/G1/Generational) — not GC-specific.

## 1. `HttpsURLConnectionImpl.delegate` null (the big cluster)

```
java.lang.NullPointerException: Cannot invoke
  "sun.net.www.protocol.https.DelegateHttpsURLConnection.setUseCaches(boolean)"
  because "this.delegate" is null
	at sun.net.www.protocol.https.HttpsURLConnectionImpl.setUseCaches(HttpsURLConnectionImpl.java:443)
	at org.apache.catalina.startup.TomcatBaseTest.methodUrl(TomcatBaseTest.java:691)
```

Confirmed present in 14 of 14 classes checked (all via `grep -c` on the raw
per-class log, so each count below is how many times the exact string
appeared in that class's run, not distinct tests):

| class | occurrences |
|---|---:|
| `TestAlpnFallback` | 1 |
| `TestClientCert` | 2 |
| `TestClientCertTls13` | 1 |
| `TestCustomSsl` | 1 |
| `TestCustomSslTrustManager` | 3 |
| `TestLargeClientHello` | 1 |
| `TestSSLHostConfigCipher` | 4 |
| `TestSSLHostConfigCompat` | 26 |
| `TestSSLHostConfigProtocol` | 2 |
| `TestSslHandshakeFailure` | 1 |
| `TestSsl` | 5 |
| `ocsp.TestOcspSoftFail` | 3 |
| `ocsp.TestOcspSoftFailInternalError` | 4 |
| `ocsp.TestOcspTimeout` | 2 |

Also present (not individually re-counted, but appears in the same non-passed
list with the same call site): `TestOcspEnabled`, `ocsp.TestOcspSoftFailTryLater`.

None of these classes are in the 2026-08-14 census's 64-class list — this is
new since then, not a recurrence of a previously-documented issue.

### What the call site tells us

`TomcatBaseTest.methodUrl` opens an `HttpsURLConnection` to the embedded test
server and immediately calls `setUseCaches(false)` before connecting.
`HttpsURLConnectionImpl` is a JDK class whose real-JDK implementation wraps a
`DelegateHttpsURLConnection` instance created during `URL.openConnection()` /
the constructor — `delegate` should never be observably null by the time user
code holds a reference to the `HttpsURLConnectionImpl`. A null `delegate`
here means CratonVM's handling of `https:` URL stream handler construction
(or of `HttpsURLConnectionImpl`'s own init path) isn't wiring the delegate
before returning the connection object to the caller — this looks like a VM
native-registration or constructor-ordering gap in the `sun.net.www.protocol.https`
surface, not an application-level bug (HotSpot presumably passes; not yet
individually re-verified in this record).

### Not yet done

* Not traced past the NPE site — don't know whether `delegate` is never set,
  set too late (a lazy-init path not triggered), or set on a different
  instance than the one the caller holds.
* Not checked against HotSpot on this exact fixture/class (expected to pass,
  per every other `https:`-touching native surface investigated in this
  tree, but not confirmed for this specific call path).
* Not checked whether this is `--nojit`-sensitive or collector-sensitive
  beyond "all three GCs fail identically" (weak evidence against a GC cause,
  not a substitute for `--nojit`).
* Relevant native code: `native-builtins/src/http_url_connection.rs` (no
  direct hit for how `delegate` gets populated — grepped for `"delegate" =`
  and found nothing, so the wiring likely happens through generic
  constructor/field-init machinery rather than a dedicated native method;
  worth checking `net_phase_e.rs` too, which references
  `DelegateHttpsURLConnection` in a comment near line 8712 in the current
  tree).

## 2. `util.TestCookieFilter` — fixture gap, not a VM defect

```
java.lang.NoClassDefFoundError: util/CookieFilter
	at util.TestCookieFilter.test01(TestCookieFilter.java:29)
```

A test-support helper class (`util.CookieFilter`) referenced by
`util.TestCookieFilter` was never compiled into the classpath this harness
uses (`.suite/cp-linux-fixed.txt` / `output/testclasses`). This is a Tomcat
test-source-set compilation gap on this fixture, not a CratonVM finding —
HotSpot would fail identically against the same incomplete classpath. Fix is
to include whichever source set `util.CookieFilter` lives in in the
`ant test-compile` step (or equivalent), not to chase it as a VM bug.
