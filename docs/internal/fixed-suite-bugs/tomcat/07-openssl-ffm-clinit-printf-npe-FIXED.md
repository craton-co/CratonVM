# Bug 07 — openssl_h `<clinit>` printf NPE → FFM init chain (FIXED)

**Status:** ✅ FIXED (commit `583cd9a6`, merged to `dev` via `a02e4395`).
**Severity:** High — the CratonVM-specific failure mode broke the OpenSSL/TLS
FFM init path for `org.apache.tomcat.util.openssl.openssl_h` and any other
java.lang.foreign downcall binding.
**Repro (isolated):** `Class.forName("org.apache.tomcat.util.openssl.openssl_h")`
on the Tomcat classpath. The user-facing repro class is
`org.apache.tomcat.util.net.TestClientCertTls13`.

## Symptom (before fix)

```
WARN <clinit> failed — wrapping in ExceptionInInitializerError
  class=org/apache/tomcat/util/openssl/openssl_h
  cause=java/lang/NullPointerException: Cannot invoke "java.io.PrintStream.printf(String, Object[])"
  at java/lang/Module.ensureNativeAccess (Module.java:322)
  at java/lang/foreign/SymbolLookup.libraryLookup (SymbolLookup.java:296)
  at org/apache/tomcat/util/openssl/openssl_h.<clinit> (openssl_h.java:94)
```

## Root cause — three distinct CratonVM gaps along the real-JDK FFM init chain

`openssl_h.<clinit>` calls `SymbolLookup.libraryLookup("ssl.dll", arena)`. In
real-JDK mode CratonVM runs the JDK's own bytecode for that, which walks:
`libraryLookup` → `Module.ensureNativeAccess` (restricted-method warning) →
`Utils`/`ValueLayout` `<clinit>` → `RawNativeLibraries.load0` (dlopen). Each
step relied on JVM-provided state CratonVM had not supplied:

1. **`System.initialErr` never populated** — `Module.ensureNativeAccess`
   (Module.java:322) emits the "restricted method called" warning via
   `VM.initialErr().printf(...)`. `VM.initialErr()` →
   `SharedSecrets.getJavaLangAccess().initialSystemErr()` → `getstatic
   java/lang/System.initialErr`. The real JDK sets that `@Stable` field in
   `System.initPhase1()` (System.java:1820); CratonVM boots via a *native*
   `initPhase1` (`native-builtins/src/lang_system.rs`) that set out/err/in but
   not `initialErr`, so it was null → `null.printf(...)` → NPE → wrapped as
   `ExceptionInInitializerError` for **every** FFM binding's `<clinit>`.
   *(Note: the `System$1.initialSystemErr` shim is a real JDK class, so its
   concrete bytecode runs — a native override does NOT win; the backing static
   field is what must be set.)*

2. **`UnsafeConstants` all-zero** — `jdk/internal/misc/UnsafeConstants.<clinit>`
   zero-inits `ADDRESS_SIZE0 / PAGE_SIZE / BIG_ENDIAN / UNALIGNED_ACCESS /
   DATA_CACHE_LINE_FLUSH_SIZE` and relies on the JVM to overwrite them at
   bootstrap (HotSpot does this natively). CratonVM never did, so
   `Unsafe.ADDRESS_SIZE` (= `UnsafeConstants.ADDRESS_SIZE0`) stayed 0. With
   step 1 fixed, the chain advanced to `ValueLayout.<clinit>`, which builds the
   ADDRESS layout with `byteAlignment = Unsafe.ADDRESS_SIZE = 0` →
   `IllegalArgumentException: Invalid alignment: 0`.

3. **`RawNativeLibraries` natives missing in real-JDK mode** — with steps 1–2
   fixed, the chain reached the actual native library load:
   `RawNativeLibraries.load0` / `NativeLibrary.findEntry0` / `unload0` were
   unimplemented (`UnsatisfiedLinkError`). CratonVM's libffi FFM natives
   (`panama.rs`) are registered only from `register_synthetic_overrides`, which
   is `#[cfg(feature="synthetic-jdk")]` and **dead-code-eliminated in real-JDK
   mode** — exactly where these are needed (real mode runs the JDK's own FFM
   bytecode, which calls these three real `ACC_NATIVE` methods directly).

## Fix

| # | File | Change |
|---|------|--------|
| 1 | `vm/src/vm/vm_init.rs` | In `ensure_system_streams`, stamp `java.lang.System.initialErr` with the err stream (static-only field index, matching `pre_init_string_statics`). |
| 2 | `vm/src/vm/vm_util.rs` | Post-clinit success-path fixup for `jdk/internal/misc/UnsafeConstants`: ADDRESS_SIZE0=8, PAGE_SIZE=4096, BIG_ENDIAN=false, UNALIGNED_ACCESS=true, DATA_CACHE_LINE_FLUSH_SIZE=0. |
| 3 | `native-builtins/src/{panama.rs,lib.rs}` | Implement `RawNativeLibraries.load0`/`unload0` + `NativeLibrary.findEntry0`, backed by `load_native_library`/`find_native_symbol`. Registered from `register_essential_natives` (always-compiled), NOT the synthetic-gated `register_pe_panama`. |

## Verification — now byte-for-byte identical to HotSpot/JDK 25

After the fix, `openssl_h.<clinit>` behaves **exactly** like HotSpot on a box
without OpenSSL installed — both attempt the load and fail with the same
exception:

```
CratonVM (fixed):  IllegalArgumentException: Cannot open library: ssl.dll
                     at SymbolLookup.libraryLookup(SymbolLookup.java:350)
HotSpot  (jdk-25): IllegalArgumentException: Cannot open library: ssl.dll
                     at java.base/java.lang.foreign.SymbolLookup.libraryLookup(SymbolLookup.java:350)
```

The CratonVM-specific `printf` NPE is gone. This realizes precisely the
behavior [Bug 06](06-openssl-ffm-clinit-segv-CRASH.md) described as correct
("exactly mirroring HotSpot's caught `IllegalArgumentException: Cannot open
library: ssl.dll`"). `ssl.dll` not being loadable is an **environment fact**
(OpenSSL not installed), identical on both VMs — not a VM bug. The three fixes
are general: they benefit all FFM bindings and all `Unsafe`/`ADDRESS_SIZE`
consumers, not just openssl.

Regression: `Hello` (stdout/stderr) passes under JIT on and off; normal boot
unaffected.

## Remaining (separate, pre-existing) blocker for the TLS suite

`TestClientCertTls13` and the other TLS classes still do **not** pass on
CratonVM — but **not** because of openssl_h (its error is now gone from the
log). They hang at the **JSSE TLS handshake** (`Starting ProtocolHandler
["https-jsse-nio-..."]` then no progress), a distinct pre-existing issue in the
JSSE/`SSLEngine` path (cf. `01-jsse-tls-chain-FIXED.md`, which is evidently
incomplete for TLS 1.3 client-cert). HotSpot passes `TestClientCertTls13`
(`OK (6 tests)`) via JSSE. The user's hypothesis that a single openssl_h fix
clears ~17 classes is therefore only partly right: the openssl_h printf NPE was
a real, now-fixed CratonVM bug, but the [JSSE] test variants are gated by a
separate handshake hang that must be fixed independently.

## Reproduction

```
# isolated openssl_h init (CWD: apps/tomcat):
cratonvm.exe -cp "<scratch>;$(cat .suite/cp.txt)" \
  --add-opens java.base/java.lang=ALL-UNNAMED InitOpenssl
#   InitOpenssl = Class.forName("org.apache.tomcat.util.openssl.openssl_h")
# before: NPE "Cannot invoke printf"; after: IllegalArgumentException ssl.dll (== HotSpot)
```
