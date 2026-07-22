# Bug DF01 — `AbstractSelectableChannel.keyFor` NPE kills the NioEndpoint selector loop → every serving test hangs

> **✅ FIXED 2026-06-17** (worktree `C:/craton/CratonVM-tcfull`, branch
> `tomcat-fullsuite-triage`, `native-io/src/nio_selector.rs`). Added a native
> `keyFor(Selector)` override on the channel classes, backed by the existing
> native key registry, so the real bytecode's `synchronized(keyLock)` (null
> `keyLock` → NPE) is never executed. **Verification:** standalone repro
> (`.tooling/drv/KeyForRepro.java`) now matches HotSpot semantics (pre-register →
> null, post-register → the registered key, `matchesRegistered=true`), no NPE.
> `jakarta.servlet.http.TestHttpServlet` went **HANG → runs to completion**
> (`Tests run: 13, Failures: 6`, 0 selector-loop NPEs, no crash); the residual 6
> failures are downstream of DF04 (`Net.available`) + serving-detail asserts, not
> a regression. Full-suite before/after re-run in progress (tag `devfix`).
> Root-cause analysis below confirmed correct.

**Severity:** CRITICAL (single dominant blocker — accounts for the majority of the 213 hangs).
**Status on CratonVM:** HANG (server never serves) → **FIXED**. **HotSpot:** PASS.
**Run date:** 2026-06-17
**Binary:** dev `77620f55` (worktree `C:/craton/CratonVM-tcfull`, branch `tomcat-fullsuite-triage`).
**Affected classes:** **≈123** test-class logs contain this error (every embedded-server test that needs to accept an HTTP connection).

## Symptom

Every embedded-server test starts Tomcat, the connector binds, then the NIO
poller thread dies on the very first selector iteration:

```
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error in selector loop
   (java/lang/NullPointerException: monitorenter in
    java/nio/channels/spi/AbstractSelectableChannel.keyFor pc=7)
INFO  [org.apache.coyote.http11.Http11NioProtocol] Pausing ProtocolHandler [...]
INFO  [org.apache.catalina.core.StandardService] Stopping service [Tomcat]
```

The endpoint then tears the connector down and the next test case re-initializes
a fresh `ProtocolHandler` — which fails identically. Because the server never
accepts a connection, the test's in-process HTTP client blocks until the harness
per-class timeout (120 s) → **HANG**. HotSpot serves normally and passes.

## Root cause (analysis)

`java.nio.channels.spi.AbstractSelectableChannel.keyFor(Selector)` is:

```java
public final SelectionKey keyFor(Selector sel) {
    synchronized (keyLock) {      // bytecode: aload keyLock; monitorenter  <-- pc=7
        ...
    }
}
```

`keyLock` is `private final Object keyLock = new Object();`, assigned in the
`AbstractSelectableChannel` constructor. The NPE is **`monitorenter` on a null
operand** — i.e. on CratonVM the channel's `keyLock` field reads back `null`
when `keyFor` runs on the poller thread.

This is the classic CratonVM **synthetic-object field-layout / field-init gap**
for the `java.nio` channel hierarchy — the same family as the already-fixed
`Selector.open()` slot-collision (see `docs/tomcat-selector-investigation.md`
"## RESOLUTION" and BUG-2 in `../CRATONVM_BUGS.md`). The channel object that
reaches `keyFor` either:
- never had `keyLock` populated (the `AbstractSelectableChannel.<init>` path that
  sets `keyLock`/`regLock` is not run, or a native shadows the ctor with a
  synthetic layout that omits `keyLock`), **or**
- the field is being read at the wrong slot (layout collision with a
  reference-typed JDK field, decaying to null).

Note `pc=7` is a *VM-synthesized* throw site (the interpreter detects the null
monitor operand), so there is no Java stack — the diagnosis is from the bytecode
of `keyFor` plus the NioEndpoint catch site.

## Not a regression — persistent dominant wall

This NPE is present in every prior full-suite run on dev (logs contain it:
`tc1`=96, `loop4`=115, `srun`=115 class-logs). Prior sessions got a *bootstrap*
Tomcat to serve HTTP 200 (single connector, MiniRaw probe), but the **JUnit
embedded-server path through `NioEndpoint` + `keyFor` has never worked** and is
the single highest-leverage fix in the suite: cracking it would convert ~123
HANGs toward PASS in one stroke.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
C:\craton\CratonVM-tcfull\target\release\cratonvm.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore jakarta.servlet.http.TestHttpServlet
# -> "Error in selector loop ... keyFor pc=7", server never serves, hangs.
```

Fastest isolated repro: any `jakarta.servlet.http.TestHttpServletDoHead*`
variant (they each spin up one connector and issue one request).

## Recommendation

**FIX (highest priority).** Trace where the `AbstractSelectableChannel` (concrete:
`sun.nio.ch.SocketChannelImpl` / `ServerSocketChannelImpl`) `keyLock` field is
populated under CratonVM and why it reads null on the poller thread. Likely a
native ctor/`register`/`keyFor` shadow using a synthetic field layout — align it
with the real layout (cf. the `sel_obj_ids` side-table fix used for
`SelectorImpl.open`). This one fix unblocks the largest cluster.
