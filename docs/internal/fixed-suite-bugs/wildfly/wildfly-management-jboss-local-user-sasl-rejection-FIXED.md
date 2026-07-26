# WildFly management `JBOSS-LOCAL-USER` SASL rejection — FIXED 2026-07-21

Status: **FIXED — 2026-07-21.** The dominant WildFly integration-suite blocker after the six
2026-07-21 boot fixes (`wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md`) is resolved. Two
independent CratonVM defects were fixed on branch `fix/wildfly-sasl-20260721` (commit `743aabd40`,
merged to dev). Management authentication now succeeds for both a real-JDK `jboss-cli-client.jar`
client and a CratonVM management client (`outcome => success` / rc=0, previously
`JBOSS-LOCAL-USER: Server rejected authentication`).

## The original hypothesis was WRONG

The original doc theorised the 8 random challenge bytes were corrupted between the server's file
write and the client's echo-back over the wire ("the mismatch is on the server's comparison, or the
challenge bytes were corrupted"). **This is refuted.** Instrumenting Elytron's `LocalUserServer`
(patched class injected into the module jar) proved:

- The challenge file, the client's echoed bytes, and the server's in-memory `challengeBytes` all
  match exactly (e.g. file `0b2f7c31fd617c49`, wire echo `0b 2f 7c 31 fd 61 7c 49`).
- `Arrays.equals(challengeBytes, Arrays.copyOf(message, 8))` returns **true** — the challenge check
  passes.

The real failure is one step later: `AuthorizeCallback.isAuthorized() == false` for the `$local`
identity.

## Root cause 1 (THE SASL reject): `java.security.Permissions.elements()` silently empty

`ServerAuthenticationContext.doAuthorization` loads the `$local` identity (exists=true), then checks
`authorizedIdentity.implies(LoginPermission.getInstance())` — which returns **false**, so the
identity is rejected with "no LoginPermission". The mgmt config grants `LoginPermission` to every
non-anonymous principal via the `simple-permission-mapper` `match-all` mapping, whose verifier is the
`login-permission` permission-set (containing exactly one `org.wildfly.security.auth.permission.LoginPermission`).

The mapper matches the correct (`match-all`) mapping, but its verifier `implies(LoginPermission)`
returns false. WildFly builds that verifier's `java.security.Permissions` by **copying permissions
via `Permissions.elements()`** (`PermissionMapperDefinitions.createPermissions`). Under CratonVM the
copy source enumerates **empty**, so the verifier holds no permissions.

Minimal reproduction (real-JDK mode):

```java
Permissions p = new Permissions();
p.add(new LoginPermission());
p.implies(LoginPermission.getInstance()); // true  (both cvm and real JDK)
count(p.elements());                      // cvm: 0   real JDK: 1   <-- the bug
```

CratonVM's synthetic `java/security/Permissions.add` native (`native_permissions_add`, lib.rs) stores
the added permission into the single `allPermission` field slot instead of the real JDK
`permsMap` + per-class `PermissionCollection` structure. On a **real** JDK `Permissions` object that
leaves `permsMap` empty, so:
- `implies` limps: it falls back to `allPermission.implies(p)`, true only for the *last-added*
  permission's own class (which is why the direct `implies` check accidentally passed);
- `elements()`/`size()` iterate the never-populated `permsMap` and return **empty** — dropping the
  LoginPermission on the copy path.

**Fix** (`../../../../native-api/src/registry.rs`): added `java/security/Permissions` and
`java/security/PermissionCollection` to the established `drop_real_layout_synthetic` list (same class
of bug as StringJoiner / EnumSet / StringReader / LinkedBlockingDeque / Pattern-Matcher). In real-JDK
mode the corrupting `add`/`setReadOnly`/`isReadOnly` natives are dropped and the self-contained real
JDK bytecode runs, correctly maintaining `permsMap`. The synthetic permissive collection built by
`security_manager::build_permissive_collection` seeds its slots directly (not via native `add`), so
it is unaffected. This is a **general** fix — any real-JDK-mode code that iterates a `Permissions`
collection was affected.

## Root cause 2 (the endpoint went dark before auth could even be attempted)

Before auth could be reached reliably, the management endpoint died ~30-60 s after boot. Live
watchdog + gdb capture showed the accept-pump / source-poller threads spinning forever inside
Undertow's `HttpReadListener.handleEvent`. `native_source_resume_reads` (registered for both
`resumeReads` and `wakeupReads`) synchronously invoked the Undertow read listener from the caller's
stack. Undertow's `exchangeComplete` CASes `requestState` 1→2, calls `resumeReads()`, then resets to
0; the synchronous dispatch re-entered `handleEvent` while `state == 2` and its entry loop
(`get != 0` + failing `CAS(1->2)`) spun forever.

**Fix** (`../../../../native-builtins/src/xnio_conduits.rs`): `resumeReads` now only registers interest — delivery
happens on the dedicated source-poller thread (10 ms tick, guarded by the non-reentrant `dispatching`
flag and pending-gated). `wakeupReads` sets a new `wakeup_pending` flag consumed by the poller (a
separate `native_source_wakeup_reads` handler was added and wired to the `wakeupReads` registrations).
The multi-second inline notify sleep ladders (`[0,5,20,…,2000]` / `[…,10000]`) were collapsed to a
single immediate probe: with the poller present they only postponed the jboss-remoting greeting past
the client's 5 s connect timeout (`WFLYPRT0023`).

## Verification

- `Permissions.elements()` unit repro: `elements=1` after the fix (was 0).
- Real-JDK `jboss-cli-client.jar` → CratonVM server: `outcome => success` (was
  `Server rejected authentication`).
- CratonVM management client → CratonVM server: rc=0, "Registered successful result" (was reject).
- Endpoint stays healthy through a 40 s settle; `101 Switching Protocols` + remoting greeting
  delivered promptly.
- Fast regression-suite: identical to baseline (3 pass / 8 pre-existing env fails on the Linux host).

## Residual (environmental, not this bug)

CratonVM-client → CratonVM-server occasionally still times out at `WFLYPRT0023` during the
post-upgrade remoting handshake (~1 in 3 under host load, 0 when the box is quiet). This is host
CPU contention between two heavy JITing CratonVM processes racing the handshake window — the same
class of environmental artifact as
`wildfly-stw-takeover-recurrence6-closed-host-contention-artifact-20260718`. A real suite run
(`ts.timeout.factor=100`, generous Arquillian timeouts) absorbs it. Not a code residual of the SASL
defect, which is fully fixed.
