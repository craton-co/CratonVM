# `javax.net.ssl.SSLContext`'s natives answered for a context whose real SPI belonged to somebody else

**Status: FIXED 2026-08-20**, branch
`fix/netty-sslctx-spi-and-openssl-keymat-20260820`, Azure Linux host, release
builds from dev `86b13ed4c`. Supersedes the OPEN page
`known-issues/netty/sslcontext-natives-ignore-a-third-party-spi-20260819.md`,
which stated the defect and deliberately did not fix it: "that is a design
decision about which JSSE CratonVM claims to be, not a mechanical fix, which is
why it is a page and not a patch."

The class-level result, same host, same corrected classpath, one fork each:

| | `BouncyCastleEngineAlpnTest` |
|---|---|
| HotSpot 25 | `found=1 started=1 **ok=1** failed=0 ms=877` |
| CratonVM, dev `86b13ed4c` | `found=1 started=1 ok=0 **failed=1** ms=504` |
| CratonVM, this branch | `found=1 started=1 **ok=1** failed=0 ms=711` |

## 1. What it was

CratonVM registers natives on `javax/net/ssl/SSLContext` because its own
`SSLContext.getInstance(String)` allocates a synthetic instance of that exact
class. Those registrations also answered for a REAL `javax.net.ssl.SSLContext`
that real JDK bytecode built around a third party's `SSLContextSpi` — and they
ignored the SPI entirely.

Same defect family as the NIO one fixed the day before (see the
foreign-nio-subclass write-up): a native registered on a JDK class, correct for
the objects CratonVM allocates, applied to an object somebody else built.

`probes/SslContextSpiProbe.java` prints the whole registered surface for a
context built around BouncyCastle's SPI (`bc.*`) and for one CratonVM built
itself (`own.*`). Both arms are printed on purpose: a probe that showed only the
BC arm could be satisfied by delegating everything, which is exactly the fix
this page's predecessor rejected. Measured on JDK 25, `diff` of the `bc.*`
half against HotSpot:

```
                          HotSpot 25 / CratonVM after      CratonVM before
bc.getProtocol            TLSv1.3                          BCJSSE version 1.0023
bc.getProvider            BCJSSE version 1.0023            BCJSSE version 1.0023
bc.init                   ok                               ok  (recorded in the WRONG place)
bc.createSSLEngine        o.b.j.p.ProvSSLEngine_9          sun.security.ssl.SSLEngineImpl
bc.createSSLEngine(h,p)   o.b.j.p.ProvSSLEngine_9          sun.security.ssl.SSLEngineImpl
bc.engineToString         ProvSSLEngine_9@X                THREW NullPointerException:
                                                             Cannot read field "conSession"
                                                             because "this.conContext" is null
bc.getSocketFactory       o.b.j.p.ProvSSLSocketFactory     javax.net.ssl.SSLSocketFactory
bc.getServerSocketFactory o.b.j.p.ProvSSLServerSocketF...  javax.net.ssl.SSLServerSocketFactory
bc.getClientSessionContext o.b.j.p.ProvSSLSessionContext   javax.net.ssl.SSLSessionContext
bc.getServerSessionContext o.b.j.p.ProvSSLSessionContext   javax.net.ssl.SSLSessionContext
bc.defaultProtocols       TLSv1.3,TLSv1.2                  TLSv1.3,TLSv1.2
bc.supportedProtocols     TLSv1.3,TLSv1.2,TLSv1.1,TLSv1,   TLSv1.3,TLSv1.2,TLSv1.1
                          SSLv3
```

**After the fix the `bc.*` half is identical to HotSpot, line for line, all
fourteen lines.** The `own.*` half is byte-identical between the pre-change and
post-change binaries — see §5.

Two distinct mistakes hid behind one symptom:

* **A LAYOUT read.** `getProtocol()` was `ctx.get_field(this, 0)`. Slot 0 of
  the SYNTHETIC layout is the protocol; slot 0 of the real
  `javax.net.ssl.SSLContext` is `provider`
  (`provider`/`contextSpi`/`protocol`, `javap -p --module java.base`), so the
  provider was echoed back as a protocol NAME.
* **A DELEGATION that never happened.** `createSSLEngine()` allocated
  CratonVM's own rustls-backed `sun.security.ssl.SSLEngineImpl`
  unconditionally, so a BouncyCastle context handed back a SunJSSE-named engine
  BC never built and that was never initialised. `engine.toString()` alone
  threw `NullPointerException: Cannot read field "conSession" because
  "this.conContext" is null`, which is where netty's
  `BouncyCastleUtil.isBcJsseInUse(engine)` died.

## 2. Why the obvious guard was wrong, and what replaced it

The obvious guard — "if `this.contextSpi` is a real object, delegate to
`SSLContext`'s own `final` bytecode" — is not safe.
`jca::provider_chain::seed_sunjsse_services` mirrors SunJSSE's service table
into CratonVM's provider map, so a real `SSLContext` whose `contextSpi` is a
genuine `sun.security.ssl.SSLContextImpl` is reachable **today**, and today it
works precisely BECAUSE `createSSLEngine` short-circuits to the rustls-backed
engine. Delegating that one to real SunJSSE bytecode would drive it into JDK
JSSE internals CratonVM does not implement — and the netty TLS classes are on
exactly that path.

So the boundary is a DECISION, stated once, in `jca/ssl_context_spi.rs`:
**`sun.security.ssl.*` is the JSSE CratonVM claims to be; every other SPI is its
own author's business.** `ADOPTED_SPI_PACKAGES` is that decision and
`context_owner` is the only thing that reads it.

It answers in **three** kinds, not two, because "no real SPI at all" and "a real
SPI that is ours" want different things from `getProtocol()`:

| kind | what it is | `getProtocol()` | `engine*` methods |
|---|---|---|---|
| `Synthetic` | a context CratonVM allocated | slot 0 (the natives own it) | the natives |
| `Adopted(spi)` | a real context around `sun.security.ssl.*` | the real `protocol` field | the natives |
| `Foreign(spi)` | a real context around anyone else's SPI | the real `protocol` field | `spi.engine*()` |

`Adopted` exists because the LAYOUT bug is about the object, not about the SPI's
author: a real `SSLContext` around `SSLContextImpl` has the real layout too, and
reading slot 0 on one is just as wrong.

Nine methods delegate — `init`, `createSSLEngine` ×2, `getSocketFactory`,
`getServerSocketFactory`, the two `SSLParameters` accessors and the two
session-context getters — plus `getProtocol`, whose fix is the layout read
rather than a delegation.

The `contextSpi` value is also type-checked against `javax.net.ssl.SSLContextSpi`
before it is believed. The synthetic layouts store an `Int` flag in the slot the
real class calls `contextSpi`, but nothing states that contract where a future
edit would have to read it; the check is what stops a `String` or a
`KeyManager[]` landing there from being taken for somebody's provider.

## 3. All three registration sets, not the one that wins today

There are three registrations of the `javax/net/ssl/SSLContext` surface —
`tls.rs::register_ssl_context`,
`phases_late::ssl_security::register_p68_ssl` and
`net_phase_e::register_re6_ssl_context` — and last-write-wins decides which
answers. In real-JDK mode `register_re6_ssl_context` runs last and wins
everything; the other two are inert.

All three carry the guard anyway. A guard on only the current winner is one
re-ordering away from being dead, and the boot order is a property of `lib.rs`,
not of any of those files.
`registrar_tests::all_three_registration_sets_route_a_foreign_spi` asserts the
boundary against each set independently, and every "must delegate" case has a
"must NOT delegate" twin — an `SSLContextImpl` receiver and a synthetic one — so
the suite cannot be satisfied by delegating unconditionally. The test also
counts how many registrations it actually exercised and fails below 12, so a
`find()` that stopped resolving turns into a failure rather than a silent
no-op.

## 4. `getProvider()` — the same slot confusion, the other way up

Found with the probe while validating the above, not by looking for it:

```
                    HotSpot 25          CratonVM before
own.getProvider     SunJSSE version 25  TLSv1.3
```

`getProvider()` had NO registration at all, so the real JDK bytecode answered
it: `return provider;`, slot 0 — the PROTOCOL on every synthetic layout. The
method handed back a `java.lang.String` where a `java.security.Provider` is
contracted, and every caller doing anything with the answer beyond printing it
failed on the type.

It is registered now, through the same boundary: a real context answers its own
`provider` field (so `getInstance(algo, p).getProvider() == p` holds by
identity); a synthetic one answers `SunJSSE`, the provider CratonVM's own
`getInstance(String)` stands in for.
`provider_chain::provider_object_named` is `Security.getProvider(String)`'s
body, EXTRACTED rather than re-implemented, so the "prefer the object the
application actually registered" rule — what makes `assertSame(added,
Security.getProvider("BC"))` and `instanceof BouncyCastleProvider` work — has
one implementation.

Both registrar ratchets were re-taken by one row each, with the reason recorded
on the constants: `registrar_drift.rs`'s `BASELINE_TOTAL_DRIFT` 1232 → 1233 and
`registrar_reachability.rs`'s `register_tls_natives` 50 → 51. That new drift row
answers the drift gate's own first question ("do the two bodies agree?")
structurally rather than by inspection: all three registrations forward to the
SAME free function.

## 5. No regression on the path the netty TLS classes actually use

`diff` of the probe's `own.*` half, pre-change binary vs post-change binary:

```
18c18
< own.getProvider = TLSv1.3
---
> own.getProvider = SunJSSE version 25
```

One line, and it is the §4 fix. Nothing else in the CratonVM-owned arm moved.

What still differs from HotSpot in that arm is unchanged by this work and is
recorded so the next reader does not mistake it for fallout:

```
own.engineToString        SSLEngine[hostname=null, port=-1,   THREW NullPointerException:
                          Session(...|SSL_NULL_WITH_NULL_NULL)]  Cannot read field "conSession"
own.getSocketFactory      sun.security.ssl.SSLSocketFactoryImpl  javax.net.ssl.SSLSocketFactory
own.getServerSocketFactory sun.security.ssl.SSLServerSocketFactoryImpl
                                                              javax.net.ssl.SSLServerSocketFactory
own.get{Client,Server}SessionContext
                          sun.security.ssl.SSLSessionContextImpl javax.net.ssl.SSLSessionContext
own.supportedProtocols    …,TLSv1,SSLv3,SSLv2Hello           TLSv1.3,TLSv1.2,TLSv1.1
```

The factory/session-context rows are CratonVM's synthetic allocations answering
under the abstract class name, and the protocol list is deliberately shorter.
**`own.engineToString` is a genuine open residual**: `toString()` on a
CratonVM-allocated `sun.security.ssl.SSLEngineImpl` runs the real JDK body,
which dereferences a `conContext` CratonVM never populates, so it throws where
HotSpot returns a string. It is on `SSLEngine`, not `SSLContext`, and it is not
what this page was about — but any log line that prints an engine hits it. Filed
in `known-issues/netty/` rather than fixed here.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
./gen-openssl-args.sh --bc18 -o /tmp/bc18.args     # see that script for why --bc18

javac -cp "$(sed -n 2p /tmp/bc18.args)" -d . <repo>/probes/SslContextSpiProbe.java
java  @/tmp/bc18.args SslContextSpiProbe                       # the oracle
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m \
      @/tmp/bc18.args SslContextSpiProbe                       # diff against it

java  @/tmp/bc18.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.BouncyCastleEngineAlpnTest
<cratonvm> … @/tmp/bc18.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.BouncyCastleEngineAlpnTest
```

`--bc18` is load-bearing: `common.args` lists `bcprov-jdk15on-1.70.jar` AHEAD of
`bcprov-jdk18on-1.84.jar`, so `bctls-jdk18on-1.84` resolves
`NISTObjectIdentifiers` from 1.70 and dies in `TlsUtils.<clinit>` on the missing
`id_ml_dsa_44` — identically on both VMs, before any of this surface is reached.
The probe also registers `BouncyCastleProvider` before
`BouncyCastleJsseProvider`, because BC's JSSE builds its `JcaTlsCrypto` through
`SecureRandom.getInstance("DEFAULT")` and without bcprov both VMs fail `init`
and then agree, for a reason that has nothing to do with either of them.

## Related

- the foreign-nio-subclass-and-bc-provider-object write-up — the fix that made
  this reachable, and the same defect family one layer down.
- `known-issues/netty/not-cratonvm-bugs-consolidated.md` — the
  `BouncyCastleEngineAlpnTest` row, now updated: on the corrected classpath both
  VMs pass.
