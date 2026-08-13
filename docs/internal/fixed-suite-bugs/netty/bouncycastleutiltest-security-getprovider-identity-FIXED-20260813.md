# FIXED — `Security.getProvider` handed back a stand-in, not the registered `Provider`

**Status:** ✅ FIXED 2026-08-13 on `fix/netty-known-issues-retire-20260813`.
Retires `docs/known-issues/netty/bouncycastleutiltest-no-longer-a-non-defect-20260813.md`,
whose measurement stands and whose premise-change reading was correct.

`io.netty.handler.ssl.util.BouncyCastleUtilTest` is **2/2 on CratonVM**, matching
HotSpot 25 test for test.

| | CratonVM before | CratonVM after | HotSpot 25 |
|---|---|---|---|
| `BouncyCastleUtilTest` | 0 ok / 2 failed | **2 ok / 0 failed** | 2 ok / 0 failed |

## The premise change was real, and it exposed a defect that was always there

The archived batch-11 record
(`netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`, cause 6) closed this
class as "not a defect — `found=0 started=0` on HotSpot as well, because the
tests are gated on a BouncyCastle provider that is not on this classpath."
That was true of the classpath it measured. BouncyCastle is on the classpath
now (`bcprov-jdk18on`, `bcpkix-jdk18on`, `bctls-jdk18on` 1.84, plus the 1.70
`jdk15on` line), both VMs discover 2 tests, and the two tests then failed only
on CratonVM.

So the retired page's framing was exactly right: not a regression, a **premise
that stopped holding**. The defect underneath it had been unobservable, not
absent.

## Root cause — the read side of the provider chain fabricated its answer

`java.security.Security.getProvider(String)` is a CratonVM native
(`native-builtins/src/jca/provider_chain.rs`). It looked the name up in the
provider chain and, on a hit, always returned a **freshly allocated
`java.security.Provider` synthetic** carrying just the name and version —
including for a provider the application had registered itself moments earlier
through `Security.addProvider`.

The real object was not lost: `security_add_provider` already stored it in
`real_provider_table` behind a permanent GC root, precisely so BouncyCastle-FIPS's
private `creatorMap` stays reachable. `getProvider` simply never consulted it.

That breaks the JDK's contract in the two ways the test checks, and both are
identity questions a stand-in cannot answer:

```
expected: org.bouncycastle.jce.provider.BouncyCastleProvider@36b<BC version 1.7>
 but was: java.security.Provider@37b<BC version 1.7>          <- assertSame

Unexpected type, expected: <org.bouncycastle.jce.provider.BouncyCastleProvider>
                but was:  <java.security.Provider>            <- assertInstanceOf
```

* **`assertSame`** — `testBouncyCastleProviderLoaded` registers a
  `BouncyCastleProvider` and asserts that `BouncyCastleUtil.getBcProviderJce()`
  hands back that same instance.
* **`assertInstanceOf`** — `BouncyCastleUtil.ensureLoaded` decides BouncyCastle
  is present by testing the answer's *class*. A non-null stand-in also stops it
  from falling through to its `Class.forName(BC_PROVIDER).newInstance()`
  branch, which is how it obtains a genuine provider on HotSpot (where
  `getProvider("BC")` is null, since a jar on the classpath is not a
  registration).

The second test's failure was a cascade of the first's: `testBouncyCastleProviderLoaded`
threw at its `assertSame` *before* reaching `Security.removeProvider`, so `"BC"`
stayed in the chain and the FIPS test then found a stand-in where HotSpot finds
nothing.

## Fix

`security_get_provider` prefers the registered object
(`resolve_real_provider`) and only falls back to `make_provider` for chain
entries CratonVM seeds itself (`SUN`, `SunJCE`, …), which have no real object
behind them. `security_get_providers` does the same per element — it is the same
read of the same table, and the JDK's array likewise holds the registered
instances.

Cost: none. The object is already rooted by `remember_real_provider`, so
preferring it adds no rooting and removes an allocation per call.

## Why this is broader than one netty class

Any library that installs a JCA provider and then reads it back was getting a
different object than it registered. That is the same family as the
already-fixed "CratonVM discarded an explicitly requested `Provider`"
(retired `netty-tls-batch10-provider-routing-and-close-notify` write-up):
both are the provider chain answering with something plausible rather than with
the thing the application supplied.

## Repro

```bash
cd apps/netty-suite-runner
CP=$(sed -n 2p common.args)
cratonvm --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.ssl.util.BouncyCastleUtilTest
java -cp "$CP:." -Dcraton.batch=1 \
  CratonRunner io.netty.handler.ssl.util.BouncyCastleUtilTest
```

## Related

- The retired `netty-batch11-inet6-and-sha1-oid-CLOSED-20260812` write-up,
  cause 6 — the "not a defect" verdict this supersedes.
- `docs/known-issues/netty/ssl-cert-validation-residuals-20260813.md` — the
  same-day page that noticed the same environment shift for
  `SslContextBuilderTest`. Its rows are **not** explained by this fix; they were
  re-measured on the fixed build and are unchanged.
