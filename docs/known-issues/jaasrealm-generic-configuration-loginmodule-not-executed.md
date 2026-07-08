``# TestJAASRealm — generic file-based JAAS `Configuration`/`LoginModule` chain not executed

> **STATUS: PARTIAL FIX.** The originally-reported bug — `LoginContext.<init>`'s 4-arg overload
> (`String, Subject, CallbackHandler, Configuration`) wasn't registered as a CratonVM native, so it
> ran as unintercepted real bytecode and never populated the Rust-side `login_context_handles()`
> side table, making the native `login()` override always throw
> `IllegalStateException: LoginContext state missing post-GC or never initialized` — is fixed
> (`native-builtins/src/wildfly_security.rs`, registers the 4-arg `<init>` onto the same
> `native_login_context_init` handler as the 3-arg overload already used). That exact error message
> is gone from the log and is confirmed root-caused: **not** a GC/pinning bug (the identity-hash
> side-table keying for `Subject`/`LoginContext` was already correct, see the module doc comment at
> `wildfly_security.rs:708-719`) — it reproduced deterministically on *every* `authenticate()` call
> because Tomcat's real `JAASRealm.authenticate()` unconditionally calls the 4-arg constructor
> (`JAASRealm.java:386`), which simply had no matching native registration.
>
> **Residual — a separate, much larger gap:** `TestJAASRealm.testRealm`/`testMemoryLoginModule` still
> fail, now with `SecurityException: LoginException: authentication failed`. `LoginContext.login()`
> now runs, but `native_login_context_init` resolves the module chain purely via
> `lookup_security_domain(&name)` — a Rust-side registry populated only by WildFly/Keycloak's own
> boot-time domain registration (management-realm, `ApplicationPolicy` for a named `SecurityDomain`).
> It never reads the actual `Configuration` object passed in the 4-arg constructor, so it can't see
> the `AppConfigurationEntry[]` this test defines via a real login-config file
> (`CustomLogin { org.apache.catalina.realm.TesterLoginModule sufficient; };` /
> `MemoryLogin { org.apache.catalina.realm.JAASMemoryLoginModule sufficient pathname=...; };`), and
> has no mechanism at all to instantiate + drive a real, arbitrary `javax.security.auth.spi.LoginModule`
> class via reflection (`initialize`/`login`/`commit`/`abort`). With no domain registered under
> "CustomLogin"/"MemoryLogin", `lookup_security_domain` returns `None`, the module chain defaults to
> empty, and the JAAS "at least one non-optional module must succeed" rule makes every login fail.

## Symptom (post-fix)
```
ERROR [org.apache.catalina.realm.JAASRealm] Unexpected error (java/lang/SecurityException: LoginException: authentication failed)
```
`testRealm`/`testMemoryLoginModule` both fail on `Assert.assertNotNull(p)` — `JAASRealm.authenticate()`
returns `null` for valid credentials because the (empty) module chain never succeeds.

## Affected tests
- `org.apache.catalina.realm.TestJAASRealm.testRealm`
- `org.apache.catalina.realm.TestJAASRealm.testMemoryLoginModule`

## Why this is a separate item, not a residual of the GC-root bug class
This is not a stale-pointer/moving-GC hazard (same class as BUG-U/BUG-W/BUG-Z) — it's a missing
**feature**: real, `Configuration`-driven, reflection-based `LoginModule` execution. CratonVM's
current `wildfly_security.rs` JAAS model is entirely Rust-closure-based
(`LoginModuleEntry.login_fn: Box<dyn Fn(...)>`), built for WildFly/Keycloak's own fixed bootstrap
paths (`management-realm`, named `SecurityDomain`s), not for arbitrary user-supplied
`javax.security.auth.login.Configuration` + real third-party `LoginModule` classes. Implementing
that generically would need, at minimum:
1. In `native_login_context_init`, when a non-null `Configuration` arg is present, call its real
   `getAppConfigurationEntry(String)` (`ctx.invoke_virtual`) to get the real `AppConfigurationEntry[]`
   for the given app name (instead of / in addition to `lookup_security_domain`).
2. For each entry, read `getLoginModuleName()` (String), `getControlFlag()`, `getOptions()` (Map).
3. In `login()`, for each entry: `Class.forName(loginModuleName)` + `newInstance()` (real bytecode),
   then `invoke_virtual` its real `initialize(Subject, CallbackHandler, Map, Map)` /
   `login()` / `commit()` / `abort()` per the JAAS control-flag chain semantics already implemented
   for the Rust-closure path (`ControlFlag`/`ModuleOutcome` in this same file).

That's a materially larger change (real reflection-driven module execution, not a pointer/rooting
fix) — flagging here for scoping rather than folding into the registration-gap fix above.

## Fix applied (this doc)
`native-builtins/src/wildfly_security.rs::register_wildfly_security_natives` now also registers
`<init>(Ljava/lang/String;Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;Ljavax/security/auth/login/Configuration;)V`
onto the existing `native_login_context_init` (which already only reads `args[0..=2]`, so the extra
trailing `Configuration` arg needs no new handling to eliminate the reported diagnostic).

## Repro
```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jaasrealm -Start 198 -Count 1 -TimeoutSec 60 -Parallel 1
```
(`org.apache.catalina.realm.TestJAASRealm` is class #198 in the `all` category list as of 2026-07-07.)
