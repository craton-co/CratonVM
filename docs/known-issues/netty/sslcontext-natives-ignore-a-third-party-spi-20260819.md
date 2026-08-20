# `javax.net.ssl.SSLContext`'s natives answer for a context whose real SPI belongs to somebody else

**Status: OPEN.** Found 2026-08-19 on the Azure Linux host while closing
`foreign-nio-subclass-and-bc-provider-object-FIXED-20260819.md`.
Not reachable with the netty fixture's classpath as it stands, which is why it
was invisible until now — and why it is filed rather than folded into that fix.

## What it is

CratonVM registers natives on `javax/net/ssl/SSLContext` because its own
`SSLContext.getInstance(String)` allocates a synthetic instance of that class.
Those registrations also answer for a REAL `javax.net.ssl.SSLContext` built by
real JDK bytecode around a third party's `SSLContextSpi` — and they ignore the
SPI entirely.

With `SSLContext.getInstance("TLSv1.3", new BouncyCastleJsseProvider())` now
resolving correctly (that was the JCA half of the fix referenced above), the
context object is a genuine `javax.net.ssl.SSLContext` whose `contextSpi` is
BouncyCastle's `ProvSSLContextSpi`. Then:

```
                          HotSpot 25                        CratonVM
getProtocol()             TLSv1.3                           "BCJSSE version 1.0023"
createSSLEngine()         o.b.jsse.provider.ProvSSLEngine_9 sun.security.ssl.SSLEngineImpl
BouncyCastleUtil
  .isBcJsseInUse(engine)  true                              (engine.toString() NPEs first)
```

`getProtocol()` is registered as `ctx.get_field(this, 0)` — field 0 of the
SYNTHETIC layout is the protocol index, but field 0 of the real
`javax.net.ssl.SSLContext` is `provider`, so it echoes the provider back as a
protocol name. `createSSLEngine()` is registered to allocate CratonVM's own
rustls-backed `sun.security.ssl.SSLEngineImpl` unconditionally, so a
BC-provider context hands back a SunJSSE-named engine that BC never built and
that is not initialised — `engine.toString()` alone throws
`NullPointerException: Cannot read field "conSession" because "this.conContext"
is null`.

This is the same defect family as the NIO one fixed alongside it: a native
registered on a JDK class, correct for the objects CratonVM allocates, applied
to an object somebody else built.

## Why it is not simply "guard it like the NIO natives"

The obvious guard — "if `this.contextSpi` is a real object, delegate to
`SSLContext`'s own `final` bytecode, which calls
`contextSpi.engineCreateSSLEngine()`" — is **not safe as stated**.
`jca::provider_chain::seed_sunjsse_services` mirrors SunJSSE's service table
into CratonVM's provider map, so a real `SSLContext` whose `contextSpi` is a
genuine `sun.security.ssl.SSLContextImpl` is reachable today, and today it
works precisely BECAUSE `createSSLEngine` short-circuits to CratonVM's
rustls-backed engine. Delegating that one to real SunJSSE bytecode would drive
it into JDK JSSE internals CratonVM does not implement.

So the guard has to distinguish "an SPI CratonVM has adopted" from "a third
party's SPI" — i.e. treat `sun.security.ssl.*` as ours and everything else as
the SPI's own business. That is a deliberate design decision about which JSSE
CratonVM claims to be, not a mechanical fix, which is why it is a page and not
a patch.

The same question applies to `init(KeyManager[], TrustManager[], SecureRandom)`
(currently records two presence booleans in synthetic slots and never reaches
`contextSpi.engineInit`), `getSocketFactory`, `getServerSocketFactory`, and the
`SSLParameters` accessors. There are three separate registration sets for
`javax/net/ssl/SSLContext` in the tree — `tls.rs`, `net_phase_e.rs` and
`phases_late/ssl_security.rs` — and last-write-wins decides which answers, so
whatever shape the fix takes has to account for all three rather than patching
the one that happens to win today.

## Repro

The fixture's `common.args` puts `bcprov-jdk15on-1.70.jar` on the classpath
ahead of `bcprov-jdk18on-1.84.jar`, and `bctls-jdk18on-1.84` then dies in
`TlsUtils.<clinit>` on the missing `NISTObjectIdentifiers.id_ml_dsa_44` —
identically on both VMs — before any of the above is reached. Remove the three
`*-jdk15on-1.70` jars from the `-cp` line first:

```bash
cd /data/cratonvm/apps/netty-suite-runner
python3 - <<'PY'
import io
lines = io.open("common.args").read().split("\n")
i = lines.index("-cp")
lines[i+1] = ":".join(p for p in lines[i+1].split(":") if "jdk15on" not in p)
io.open("/tmp/bc18.args", "w", newline="\n").write("\n".join(lines))
PY

# HotSpot: passes
java @/tmp/bc18.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.BouncyCastleEngineAlpnTest
# CratonVM: fails on assertTrue(BouncyCastleUtil.isBcJsseInUse(engine))
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m @/tmp/bc18.args \
    -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.BouncyCastleEngineAlpnTest
```

`apps/netty-suite-runner/probe-udtbc/BcAlpnProbe.java` prints the context
class, provider, protocol and engine class on either VM and is the shorter
statement of the divergence. The fixture is gitignored, so it is not staged
with any fix.

## What it would take to close

- Decide the boundary: which `SSLContextSpi` implementations CratonVM claims
  (today, everything named `sun.security.ssl.*`), and make that boundary
  explicit in one place rather than implicit in three registration sets.
- Guard `getProtocol`, `createSSLEngine` ×2, `init`, the factory getters and
  the `SSLParameters` accessors on that boundary, in all three registration
  sets (or by re-registering once, last, over whichever won).
- Validate on the netty TLS classes, which today exercise the CratonVM-owned
  path exclusively and must not move.

## Related

- `foreign-nio-subclass-and-bc-provider-object-FIXED-20260819.md`
  — the fix that made this reachable, and the same defect family one layer down.
