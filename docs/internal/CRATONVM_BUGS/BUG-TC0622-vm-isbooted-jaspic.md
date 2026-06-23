# Bug TC0622 — `jdk.internal.misc.VM.isBooted()` returns false → `ResourcesMgr` `InternalError` breaks JASPIC `Subject.getPrivateCredentials`

> **✅ FIXED 2026-06-23** — merged to `dev` `873355f1` (branch
> `fix/tomcat-quick-wins`, commit `587352a1`). One-line registration in
> `native-builtins/src/lib.rs`. Validated: `TestJaspicCallbackHandlerInAuthenticator`'s
> `InternalError: Expected to use ResourceBundle only after booted` is
> **eliminated**. The class is not yet fully green — it now advances past the
> boot guard and fails on a **separate, newly-revealed** gap (see "Residual"
> below), which is the honest measure of this fix: it removes the documented
> root cause and exposes the next layer.

**Severity:** Medium — affects any security path that loads a `sun.security.util`
ResourceBundle after boot (JASPIC, some `Subject`/`AccessController` message
formatting). Not TLS-wide, but a correctness gap in a JDK-internal invariant.
**Status on CratonVM:** was FAIL (`InternalError` at class init time). **HotSpot:** PASS.
**Run date:** 2026-06-23
**Binary:** original run `dev df11ac00`; fix built+validated on `dev` `873355f1`.
**Affected class:** `org.apache.catalina.authenticator.TestJaspicCallbackHandlerInAuthenticator`.

## Symptom

```
java.lang.InternalError: Expected to use ResourceBundle only after booted
    at sun.security.util.ResourcesMgr.getBundle(ResourcesMgr.java:52)
    at sun.security.util.ResourcesMgr.getString(ResourcesMgr.java:40)
    at javax.security.auth.Subject.getPrivateCredentials(Subject.java:730)
    at org.apache.catalina.authenticator.jaspic.CallbackHandlerImpl.handle(CallbackHandlerImpl.java:124)
    at org.apache.catalina.authenticator.TestJaspicCallbackHandlerInAuthenticator.testCallerPrincipalCallback(...)
```

## Root cause

`sun.security.util.ResourcesMgr.getBundle()` guards bundle loading with
`if (!jdk.internal.misc.VM.isBooted()) throw new InternalError(...)`. The real
`VM.isBooted()` reads a `booted` boolean set true at the end of
`System.initPhase2`. CratonVM drives boot natively and never flips that flag, so
the genuine bytecode returns **false**, and any post-boot security ResourceBundle
load throws.

`isBooted` was registered as a native (returning 1) only for the legacy
`sun/misc/VM`, **not** for the modern `jdk/internal/misc/VM` that `ResourcesMgr`
actually calls — so that call fell through to the real (false-returning) method.

## Fix (landed)

Register `jdk/internal/misc/VM.isBooted ()Z → 1`, consistent with the existing
`sun/misc/VM.isBooted` and with `jdk/internal/misc/VM.initLevel`'s floor-of-2
("up enough") policy in the same file. By the time app/test code runs, the VM is
booted, so returning 1 matches HotSpot's post-boot behavior.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$cp = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
<cratonvm.exe> -Xmx2g -cp $cp -Dtomcat.test.basedir=...\output\build `
  --add-opens java.base/java.lang=ALL-UNNAMED `
  org.junit.runner.JUnitCore org.apache.catalina.authenticator.TestJaspicCallbackHandlerInAuthenticator
```

## Residual (separate, newly-revealed — follow-up) — ✅ FIXED 2026-06-23

> **✅ FIXED 2026-06-23** — branch `fix/tc0622-subject-privcreds` (merged to
> `dev`). `TestJaspicCallbackHandlerInAuthenticator` is now fully green:
> **`OK (5 tests)`**, byte-for-byte the same as HotSpot (`OK (5 tests)`).

### The residual failure (history)

With the boot guard fixed, the test advanced and then failed with:

```
java.lang.NullPointerException: Cannot enter synchronized block because
  "<local1>.privCredentials" is null
    at javax.security.auth.Subject.getPrivateCredentials(...)
```

### Root cause (corrected)

The native `javax/security/auth/Subject.<init>()V`
(`native-builtins/src/wildfly_security.rs`) left the three set fields **null**,
keeping all state in a Rust side-table on the assumption "real bytecode never
reads the raw sets." Two facts broke that assumption:

1. **The side-table can't hold Java credential objects.** Its sets are
   Rust-typed (`Arc<Principal>` / `Arc<str>` / `Arc<[u8]>`); JASPIC's
   `CallbackHandlerImpl` stores Tomcat `GenericPrincipal` *objects* and the test
   round-trips them via `getPrivateCredentials().add/remove` +
   `getPrivateCredentials(GenericPrincipal.class)`. The old no-arg getters
   returned throwaway count-only synthetic sets, so nothing persisted.
2. **The `getPrivateCredentials(Class)` overload was unregistered**, so it fell
   through to a stub→real-bytecode upgrade and ran the JDK's own
   `synchronized (privCredentials)` populate loop — on the still-null field →
   NPE.

### Fix (landed)

In `wildfly_security.rs`, `native_subject_init` now initialises slots 0/1/2
(`principals`/`publicCreds`/`privateCreds`) to **real, mutable
`java.util.HashSet`s** (GC-pinned across the allocating `HashSet.<init>`), and
the no-arg `getPrivateCredentials()` / `getPublicCredentials()` natives return
that backing field set so mutations round-trip. Access is **by slot index**, not
name: the synthetic-stub field order matches the real JDK `Subject`
(`principals`, `pub*`, `priv*` under `Object`), so the indices remain valid after
the in-place stub→real upgrade — at which point the JDK's real
`getPrivateCredentials(Class)` bytecode reads the very same slot-2 `HashSet` and
filters it faithfully (no Rust `instanceof` reimplementation needed). The Rust
side-table is retained unchanged for WildFly's login/`doAs` state; no production
WildFly path adds side-table credentials read back through these getters, so
there is no WildFly regression.

**Validation:** `TestJaspicCallbackHandlerInAuthenticator` = `OK (5 tests)` ==
HotSpot; `cargo test -p cratonvm-native-builtins` = 87 wildfly + 2689 lib tests
green.
