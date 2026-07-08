# TestJAASRealm - configuration-backed LoginModule chain executed

Status: FIXED on 2026-07-08.

## Original Symptom

`org.apache.catalina.realm.TestJAASRealm.testRealm` and
`org.apache.catalina.realm.TestJAASRealm.testMemoryLoginModule` reached
`LoginContext.login()`, but authentication still failed:

```text
ERROR [org.apache.catalina.realm.JAASRealm] Unexpected error (java/lang/SecurityException: LoginException: authentication failed)
```

The first registration bug for
`LoginContext.<init>(String, Subject, CallbackHandler, Configuration)` had
already been fixed. The remaining gap was generic JAAS execution: the native
`LoginContext` stored only the Rust-side security-domain module list from
`lookup_security_domain(&name)`. Tomcat's `JAASRealm` passes a real
`javax.security.auth.login.Configuration` whose `AppConfigurationEntry[]`
names arbitrary Java `LoginModule` classes such as
`org.apache.catalina.realm.TesterLoginModule` and
`org.apache.catalina.realm.JAASMemoryLoginModule`. With no Rust domain named
`CustomLogin` or `MemoryLogin`, the native module chain was empty and the JAAS
"at least one non-optional module must succeed" rule failed every login.

## Fix

`native-builtins/src/wildfly_security.rs` now keeps the 4-arg constructor state
on the Java `LoginContext` object:

- login context name
- subject
- callback handler
- configuration

When a configuration is present, `LoginContext.login()` now:

1. Calls `Configuration.getAppConfigurationEntry(name)`.
2. Reads each `AppConfigurationEntry` class name, control flag, and options.
3. Instantiates the Java `LoginModule` class.
4. Invokes `initialize(Subject, CallbackHandler, Map, Map)`.
5. Drives `login()`, `commit()`, and `abort()` with the existing JAAS control
   flag semantics.

The synthetic `LoginContext` layout in
`classloading/src/class_manager.rs` was updated so slot 3 is the real
`Configuration` reference, not the obsolete synthetic module-list field.

`Subject.getPrincipals()` also now returns the real Java backing set once a
Java `LoginModule.commit()` has populated it, while preserving the existing
Rust-side synthetic-principal fallback for WildFly/Keycloak bootstrap modules.

## Validation

Focused validation used a unique Cargo target directory:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\target-jaasrealm-loginmodule-20260708-001'
cargo test -p cratonvm-native-builtins t19_2_c_login_context_runs_configuration_backed_login_module -- --nocapture
cargo check -p cratonvm-native-builtins
cargo check -p cratonvm-classloading
```

The focused regression covers the residual mechanism directly: a 4-arg
`LoginContext` receives a real configuration object, obtains a
`LoginModuleControlFlag: sufficient` entry, invokes the Java login module
lifecycle, returns the original subject from `getSubject()`, and exposes the
principals set populated by `commit()`.

The original Tomcat suite command was not rerun in this checkout because
`apps/tomcat-suite-runner` is not present here:

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jaasrealm -Start 198 -Count 1 -TimeoutSec 60 -Parallel 1
```
