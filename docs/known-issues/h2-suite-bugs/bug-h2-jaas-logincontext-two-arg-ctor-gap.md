# H2 — `LoginContext(String, CallbackHandler)` (2-arg) never populates native JAAS state — every JAAS-authenticated connection reports "Wrong user name or password"

## Status
**OPEN** — new finding, 2026-07-21.

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
