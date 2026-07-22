# H2 — `LoginContext(String, CallbackHandler)` (2-arg) never populates native JAAS state — every JAAS-authenticated connection reports "Wrong user name or password"

## Status
**CLOSED** — fixed 2026-07-22, on `dev`. Two independent root causes, both
fixed; see "Fix (2026-07-22)" and "Second bug found during verification"
below.

## Severity
**MEDIUM** — any code path that constructs `javax.security.auth.login.LoginContext`
via its 2-arg `(String, CallbackHandler)` constructor is silently broken:
`login()` always throws, and callers that catch broadly (as H2 does) see a
generic downstream error with no indication JAAS is involved at all.

## Affected test class
`org.h2.test.auth.TestAuthentication` (`testExternalUser`, exercised via
`allTests()`) — PASSes on the HotSpot JDK25 baseline.

## Symptom
```
org.h2.jdbc.JdbcSQLInvalidAuthorizationSpecException: Wrong user name or password
	at org/h2/test/auth/TestAuthentication.testExternalUser(TestAuthentication.java:176)
	at java/sql/DriverManager.getConnection(DriverManager.java:199)
	...
	at org/h2/engine/Engine.validateUserAndPassword(Engine.java:396)
```
`testExternalUser` connects with the **correct** external-realm password —
the failure is not a real credential mismatch. H2's `Engine.openSession`
(`org/h2/engine/Engine.java:126-142`) calls
`database.getAuthenticator().authenticate(...)`, catches any
`AuthenticationException`, logs it only to the (disabled-by-default) H2
trace system, and falls through to the same generic
`WRONG_USER_OR_PASSWORD` the normal password-mismatch path uses — so the
real underlying exception never reaches stdout/the JDBC exception the test
sees.

## Root cause
H2's `org.h2.security.auth.impl.JaasCredentialsValidator.validateCredentials`
does:
```java
LoginContext loginContext = new LoginContext(appName, new AuthenticationInfoCallbackHandler(authenticationInfo));
loginContext.login();
```
— the **2-arg** `LoginContext(String, CallbackHandler)` constructor.

CratonVM's JAAS reimplementation (`native-builtins/src/wildfly_security.rs`,
built for WildFly/Tomcat JAAS support) only registers a native `<init>`
override for two overloads:
```rust
r.register(lc, "<init>", "(Ljava/lang/String;Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;)V", native_login_context_init);
r.register(lc, "<init>", "(Ljava/lang/String;Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;Ljavax/security/auth/login/Configuration;)V", native_login_context_init);
```
i.e. only the 3-arg `(String, Subject, CallbackHandler)` and 4-arg
`(..., Configuration)` constructors. The real JDK's `LoginContext` class has
**five** public constructors (`(String)`, `(String, CallbackHandler)`,
`(String, Subject)`, `(String, Subject, CallbackHandler)`, and the 4-arg
form); the 2-arg one H2 uses is **not registered**, so it runs as real
(unmodified) JDK bytecode — which never calls `native_login_context_init`
and therefore never inserts this `LoginContext` instance into the
process-wide `login_context_handles()` side-table.

`login()`, however, **is** unconditionally registered as a native override
for `LoginContext` regardless of which constructor path was taken:
```rust
r.register(lc, "login", "()V", native_login_context_login);
```
`native_login_context_login` looks up the instance's Rust-side state via
`get_login_context_from_this`, which does a side-table lookup keyed by the
object's GC-stable identity hash. For an instance constructed via the
un-intercepted 2-arg constructor, that lookup always misses, and the native
`login()` throws:
```rust
fn login_context_missing_err() -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: "LoginContext state missing post-GC or never initialized".into(),
    }.into()
}
```
This is **not** actually a GC/relocation issue (the side-table itself was
already hardened against that — see the `SUBJECT_HANDLES`/
`LOGIN_CONTEXT_HANDLES` GC-stable-key comment in the same file) — it is a
deterministic constructor-overload coverage gap. Every call to
`new LoginContext(name, handler).login()` anywhere in the codebase hits this,
100% of the time, not just under GC pressure.

## Verification (2026-07-21)
Standalone repro, no H2 involved — a `Configuration.setConfiguration(...)`
override plus a trivial `LoginModule`, using the exact 2-arg constructor:
```java
LoginContext lc = new LoginContext("testJaas", callbackHandler); // 2-arg
lc.login();
```
- **HotSpot JDK25**: succeeds, prints `LOGIN SUCCEEDED`.
- **CratonVM** (`cratonvm-h2-fail-triage-20260721 --java-home
  /home/victor/jdk25`): throws
  `java.lang.IllegalStateException: LoginContext state missing post-GC or
  never initialized` at the `lc.login()` call, every time.

## Fix direction
Register `native_login_context_init` for the remaining `LoginContext`
constructor overloads too — at minimum `(String, CallbackHandler)` (used by
H2) and ideally `(String)` and `(String, Subject)` as well, so any JDK-legal
construction path populates the side-table before `login()` can be called.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.auth.TestAuthentication
```
or the standalone `LoginContext("name", handler)` snippet above against any
custom `Configuration`/`LoginModule` pair.

## Fix (2026-07-22)

Two changes were needed — the "Fix direction" above (registering the
missing constructor overloads) turned out to be necessary but not
sufficient.

### 1. Registered the missing `LoginContext` constructor overloads

`native-builtins/src/wildfly_security.rs`: split `native_login_context_init`
into per-overload entry points (`native_login_context_init_name_only`,
`native_login_context_init_name_handler`, `native_login_context_init_name_subject`)
that normalize each overload's argument layout and delegate into a shared
`native_login_context_init_core`. Registered all five public `LoginContext`
constructors — `(String)`, `(String, CallbackHandler)`, `(String, Subject)`,
plus the two already-registered `(String, Subject, CallbackHandler[, Configuration])`
forms.

This alone fixed the exact repro in this doc (the standalone 2-arg snippet
now prints `LOGIN SUCCEEDED`), but H2's actual
`TestAuthentication.testExternalUser` still failed with the same generic
"Wrong user name or password" — a **different** exception was now being
swallowed (`IllegalStateException: LoginContext state missing...` was gone;
a `SecurityException: LoginException: authentication failed` took its
place).

### 2. `login()` never consulted the process-wide default `Configuration`

Root cause: `native_login_context_login`/`run_java_configuration_login` only
drives real Java `LoginModule` bytecode when the `LoginContext`'s `config`
field was **explicitly** passed to the constructor (the 4-arg
`(..., Configuration)` overload, used by Tomcat's `JAASRealm`). Every other
overload — including the 2-arg one this doc is about — leaves `config` null
and falls through to `lc.login()`, which only knows about modules
pre-registered in the Rust-side security-domain registry
(`lookup_security_domain`, populated by WildFly/Keycloak's bootstrap path).

But real JDK's `LoginContext` constructors *without* an explicit
`Configuration` don't skip configuration — they grab the process-wide
default via `Configuration.getConfiguration()` at construction time. H2's
`JaasCredentialsValidator` relies on exactly this: `TestAuthentication`
installs a custom `Configuration` via `Configuration.setConfiguration(...)`
and never touches the Rust domain registry at all.

Fix: added `run_default_java_configuration_login`, consulted by
`native_login_context_login` only when `lc.modules` (the Rust-side registry
lookup) is empty — so WildFly/Keycloak's pre-registered domains are
completely unaffected. It fetches `Configuration.getConfiguration()` via
`ctx.invoke("javax/security/auth/login/Configuration", "getConfiguration", ...)`
and drives the same real-`LoginModule` execution path
(`run_login_with_java_config`, extracted from the old
`run_java_configuration_login` body) against it. Returns "fall through" when
there's no default `Configuration` or it has no entry for the name either,
so an unregistered/unconfigured name still ends in `lc.login()`'s normal
no-modules failure.

### 3. Second, distinct bug found during verification: `Properties.remove()` dropped non-`String` CHM values

Fixing (1) and (2) got `TestAuthentication.testExternalUser` (and 6 more
direct-`DriverManager` sub-tests) passing, but `testDatasource` — same
realm, same credentials, routed through `JdbcConnectionPool`/`JdbcDataSource`
instead of `DriverManager` directly — still failed identically. Root-caused
with a standalone repro (bypassing H2's swallowing `catch` in
`DefaultAuthenticator.authenticate`/`Engine.openSession` by writing a
`LoginModule` that prints the real exception before rethrowing):

```
java.lang.NullPointerException: Cannot invoke "String.toCharArray()" because
the return value of "org.h2.security.auth.AuthenticationInfo.getPassword()"
is null
	at org.h2.security.auth.impl.JaasCredentialsValidator$AuthenticationInfoCallbackHandler.handle(...)
```

`JdbcDataSource.getXAConnection()` passes the password as a `char[]`
(`StringUtils.cloneCharArray(passwordChars)`) rather than the `String` that
`DriverManager.getConnection(url, user, password)` produces.
`ConnectionInfo.removePassword()` does `Object p = prop.remove("PASSWORD")`
then `preservePasswordForAuthentication(p)`, which stashes it into the
`AUTHZPWD` property that `AuthenticationInfo.getPassword()` later reads —
but only if `p != null`.

`native-builtins/src/properties_sidetable.rs`'s `native_properties_remove`
backs `Properties.remove(Object)` with a String-only Rust side-table
(`remove_kv`); non-`String` values (this `char[]`) are stored only in the
real JDK `ConcurrentHashMap` backing (`put_non_string_into_chm`, used by
`native_properties_put` whenever the value isn't a `String`). `remove()`
checked *only* the side-table and, on a miss, discarded whatever
`remove_from_properties_backend` actually removed from the CHM
(`let _ = ctx.invoke_virtual(chm, "remove", ...)`) — returning `null`
instead of the real `char[]`. `native_properties_get` already had the
correct fall-through-to-CHM pattern for exactly this case; `remove` was the
outlier. Confirmed with an isolated repro
(`Properties.put("k", charArray); Properties.remove("k")`) — HotSpot returns
the `char[]`, CratonVM returned `null`. Plain `HashMap`/`Hashtable`
`remove()` were unaffected (only `Properties`' native override had the bug).

Fix: `remove_from_properties_backend` now returns the CHM's removed `Value`
instead of discarding it; `native_properties_remove` uses that as the
result whenever the side-table had no entry.

### Verification

- `cargo test -p cratonvm-native-builtins wildfly_security` — 22/22, then
  24/24 after the second fix (2 new test-count from unrelated interleaved
  work) — no regressions.
- `cargo test -p cratonvm-native-builtins properties_sidetable` — 24/24.
- Full crate suite, `cargo test -p cratonvm-native-builtins` (unfiltered) —
  **3057 passed, 0 failed, 6 ignored**.
- Standalone 2-arg `LoginContext` repro: `LOGIN SUCCEEDED` (was
  `IllegalStateException`).
- Standalone `Properties.remove()` on a `char[]` value: returns the
  `char[]` (was `null`).
- `org.h2.test.auth.TestAuthentication` (`allTests()` — all 10 sub-tests
  including `testDatasource`, `testXmlConfig`): **exit 0**, matching the
  HotSpot JDK25 baseline (also exit 0). Previously failed at
  `testExternalUser` (step 3 of 10).

Fixed on `dev` via branch `fix/h2-jaas-logincontext-twoarg-20260721`.
