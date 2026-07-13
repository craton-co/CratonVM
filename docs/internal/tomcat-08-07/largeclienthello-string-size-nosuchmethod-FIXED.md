# TestLargeClientHello — NoSuchMethodError: java/lang/String.size()I during shutdown

**Status: FIXED 2026-07-13.** **Severity was:** medium (crash-adjacent —
process exits via `System.exit(1)`). **HotSpot:** PASS (fresh-verified).

## Original symptom

`org.apache.tomcat.util.net.TestLargeClientHello` logged a `WARN` during
shutdown and (unrelatedly) failed its own assertion:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/String.size()I"
  caller="org/apache/juli/ClassLoaderLogManager.resetLoggers(Lorg/apache/juli/ClassLoaderLogManager$ClassLoaderLogInfo;)V @pc=38"
```

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on).

## Root cause

`ClassLoaderLogManager.resetLoggers()` (real Tomcat/JULI bytecode) calls
`logger.getHandlers()` while iterating registered loggers — this is a
**fully native-overridden** method (`java/util/logging/Logger.getHandlers`
/ `addHandler` / `removeHandler`, registered in three places:
`native-builtins/src/lib.rs`'s `register_essential_natives` and
`register_annotation_overrides`, and `native-builtins/src/phases_late.rs`'s
`register_p61_logging`). All three implementations stored/read the
handler `ArrayList` at raw instance field **slot 2**, an assumption baked
in from CratonVM's own synthetic 3-field `Logger` layout
(`0=name,1=level?,2=handlers`).

Under `--jdk real`, `Logger` objects are constructed via the **real JDK
25 bytecode layout**, which has 12 instance fields in this order:
`config, manager, name, loggerBundle, anonymous, catalogRef, catalogName,
catalogLocale, parent, kids, callerModuleRef, isSystemLogger` (confirmed via
`javap -p java.util.logging.Logger`). Slot **2 is `name`** (a `String`) in
this layout — JDK 9+ moved handler storage inside `Logger$ConfigurationData`
(referenced from slot 0, `config`), so real `Logger` has no direct
`handlers` field at all.

So `getHandlers()` read the logger's own **name string** out of slot 2,
believed it was the handlers `ArrayList`, and called
`ctx.invoke_virtual(nameString, "size", "()I", &[])` — throwing exactly
`NoSuchMethodError: java/lang/String.size()I`. `addHandler`/`removeHandler`
had the same bug (would corrupt/overwrite the logger's real `name` field
with an `ArrayList` reference, or silently null it out).

This is the same shape of bug as two already-fixed real-vs-synthetic field
collisions in this codebase: `Socket.getSoTimeout()`'s `impl`-vs-host-string
collision (`native-builtins/src/net_phase_e.rs`) and `InetAddress.holder`
(`inet_addr_side_table`) — **not** a shared vtable/dispatch-resolution
defect as originally speculated (see "Refuted hypothesis" below).

## Fix

Replaced the field-slot-2-based storage in all three registrations with a
GC-safe side table keyed by `identity_hash_code` (`jul_logger_handlers_get`/
`_set`/`_clear` in `native-builtins/src/lib.rs`, holding the backing
`ArrayList` as a global GC root), mirroring the established
`ss_side_table`/`stream_owner_table` pattern in `net_phase_e.rs`. This
sidesteps field layout entirely — correct for both real and synthetic
`Logger` objects, and immune to future real-JDK field-order changes.

## Refuted hypothesis: shared root cause with the DoHead/HTTP2 `String.setOption` cluster

The original doc speculated this was the same bug as the
`NoSuchMethodError: java/lang/String.setOption(ILjava/lang/Object;)V`
signature seen in
[dohead-jit-heap-corruption-register-invisibility.md](../../known-issues/dohead-jit-heap-corruption-register-invisibility.md)
and
[http2-testconnection-socket-closed-cluster.md](../../known-issues/http2-testconnection-socket-closed-cluster.md)
("three independent call sites, all wrongly resolving to `java.lang.String`
... shared root cause in CratonVM's method/vtable resolution"). Investigated
2026-07-13 and **refuted**: the `Socket.setSoTimeout` failures are a
*different* bug in a *different* class, reached only under
`CRATONVM_REAL_NET_SOCKETS=1` (which the Tomcat suite runner sets by
default and which unconditionally drops every native registered on
`java/net/Socket`/`java/net/ServerSocket`, including the working
`getSoTimeout`/`setSoTimeout` side-table natives — so real bytecode's
`Socket.setSoTimeout()` → `getImpl().setOption(...)` runs instead). Unlike
the deterministic Logger bug above, this one initially looked
non-deterministic/timing-dependent: reruns of `TestCancelledUpload` under
identical conditions (only debug env vars added) produced different
failure signatures across runs — `NoSuchMethodError:
java/lang/String.setOption`, `NullPointerException: Cannot enter
synchronized block because "this.socketLock" is null` (a `final` field
that should never be null post-construction), and `SocketException: Socket
is closed`. A minimal isolated repro (`new Socket(host,port)` +
`setSoTimeout` under `CRATONVM_REAL_NET_SOCKETS=1`, no Tomcat/JUnit
harness) does **not** reproduce at all, which this doc's investigation
(mis-)read as pointing at the codebase's existing register-invisible-JIT
root / stale-reference-reuse family.

**Correction:** a concurrent 2026-07-13 session
([[project_dohead_third_cause_socketfactory_synthetic_20260713]] in
project memory) root-caused this **deterministically**, not a GC-timing
race: commit `be6055605` (2026-07-09) added synthetic
`javax/net/SocketFactory.createSocket` natives that fabricate a 5-slot
synthetic-layout `Socket`; `CRATONVM_REAL_NET_SOCKETS=1` then drops every
`java/net/Socket` native so *real* `Socket` bytecode consumes that
synthetic object — a producer/consumer layout split-brain, reproducible
with `--nojit`. Which of the three faces appears depends on the exact
construction path (e.g. `useAsyncIO`), not GC timing; the isolated probe
above doesn't reproduce it because `new Socket(host,port)` goes through
`Socket`'s own real constructor directly, never through the buggy
`SocketFactory.getDefault().createSocket(...)` producer path that
`Http2TestBase` actually uses. Still a *different bug in a different
class* from the Logger fix here (so the "shared root cause" hypothesis
this doc set out to check is still refuted), but it **is** a fixable,
already-diagnosed dispatch/layout bug, not the deep GC-precision family —
don't cite this doc's "register-invisible-root" framing as the final word;
see the other session's memory for the actual fix (prepared, not yet
merged as of this note). Left the two known-issues docs open with
corrected cross-reference notes.

## Verification

Fresh build, real JDK, JIT on:

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName strdispatch-verify2 `
  -Start 599 -Count 1 -TimeoutSec 150 -Parallel 1 -Exe <fixed-binary>
# org.apache.tomcat.util.net.TestLargeClientHello
```

Result: `FAIL 107.7s` (1 failure) — **zero** `NoSuchMethodError` occurrences
(previously always present). The one remaining failure is a pre-existing,
unrelated `SSLHandshakeException: handshake read: ... (os error 10053)` in
`testLargeClientHelloWithSessionResumption` (a real TLS-handshake-abort
behavior difference from HotSpot for this test's oversized-ClientHello
scenario) — out of scope for this doc; not a dispatch/NoSuchMethodError bug.

Branch `fix/string-dispatch-nosuchmethod-20260713`, worktree
`C:\craton\CratonVM-strdispatch-20260713` (local Windows; started on the
Azure host, branch continued locally when Azure became unreachable
mid-session). Built and merged to `dev`.
