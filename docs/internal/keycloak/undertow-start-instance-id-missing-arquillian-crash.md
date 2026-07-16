# CratonVM crashes with `internal error: Undertow.start: instance id missing` booting the legacy Arquillian `auth-server-undertow` container — ~300+ `testsuite/integration-arquillian/tests/base` classes (Linux/Azure host)

Status: open — genuine CratonVM VM-level crash, high volume, high confidence

Date observed: 2026-07-16 (4-shard `azure-nonpassed` rerun on Azure host, branch
`fix/keycloak-nonpassed-rerun-azure-20260715`, binary `cratonvm-nonpassed-azure-20260715`, dev commit `a9b838c4`)

## Summary

On the Linux/Azure host, unlike the earlier Windows-side investigation, Arquillian's container registry
**correctly finds** the `auth-server-undertow` container (the earlier Windows finding —
`arquillian-auth-server-undertow-container-not-found.md` — was a harness-launch-specific gap on Windows; on this
Linux host with a properly `mvn install`-built reactor, that step succeeds). Suite bootstrap gets much further:

```
INFO [org.keycloak.testsuite.arquillian.AuthServerTestEnricher]

SUITE CONTEXT:
Auth server: auth-server-undertow

INFO [org.keycloak.testsuite.arquillian.AuthServerTestEnricher]

TEST PROCESS INFO:
Available processors: 16
Total memory: 64 MB
Max memory (Xmx): 2048 MB
[cratonvm] main-vm run() returned Err: Error in thread "main" internal error: Undertow.start: instance id missing
[cratonvm] main-vm run() Err (debug): Error in thread "main" internal error: Undertow.start: instance id missing
```

This is a genuine **CratonVM-internal crash** (`[cratonvm] main-vm run() returned Err`), not a JUnit-reported test
failure — the process aborts before any test method runs. Harness status: `CRASH`. As of this writing, **320 of
455 processed classes across the 4 shards are CRASH**, and grep confirms **309 of them show this exact
`Undertow.start: instance id missing` message** — this single bug accounts for the overwhelming majority of all
crashes in this rerun, and is very likely to affect most of the ~350
`testsuite/integration-arquillian/tests/base` classes still queued.

## Root cause

CratonVM implements WildFly's embedded Undertow HTTP server natively
(`native-builtins/src/wildfly_undertow.rs`, "T19.2.d — WildFly Undertow HTTP subsystem"). The `build()`/`start()`
contract works like this:

```rust
fn native_builder_build(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let undertow = alloc_concurrent_synthetic(ctx, CLS_UNDERTOW, UND_NUM_SLOTS);
    // ...copies listener/handler/thread-pool fields from the builder...
    // Pre-register an instance so start()/stop() find it.
    let id = next_id();
    ctx.set_field(undertow, UND_FIELD_BOUND_FDS, Value::Long(id as i64));
    undertow_instances().lock()...insert(id, UndertowInstance { id, ... });
    Ok(Some(Value::Object(Some(undertow))))
}

fn native_undertow_start(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, UND_FIELD_BOUND_FDS) {
        Value::Long(l) => l as u64,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "Undertow.start: instance id missing".into(),
            }));
        }
    };
    // ...
}
```

`build()` allocates a **new** synthetic `Undertow` object and stamps a unique instance id into its
`UND_FIELD_BOUND_FDS` field, registered in a global `undertow_instances()` map. `start()` reads that id back off
`this` (the receiver it was invoked on). The crash means `start()` is being invoked on an object whose
`UND_FIELD_BOUND_FDS` field is *not* a `Value::Long` — i.e. either a different object than the one `build()`
returned, or the field was lost/cleared somewhere between the two calls.

### Suspected call-site trigger

Keycloak's Arquillian Undertow container adapter
(`testsuite/integration-arquillian/servers/auth-server/undertow/.../KeycloakOnUndertow.java:203`) does **not**
use the simple `builder.build(); server.start();` two-step pattern directly — it calls a wrapper method:

```java
undertow.start(Undertow.builder()...);   // KeycloakOnUndertow.java:203
```

This passes the **builder** as an argument into a `start(Undertow.Builder)`-shaped method on
`KeycloakUndertowJaxrsServer` (a RESTEasy-provided wrapper class), which is expected to internally call
`.build()` then `.start()` on the result. A more standard direct pattern also exists elsewhere in the same file
(`server = builder.setHandler(wrappedHandler).build(); server.start();` at line 314-315) — worth checking whether
*that* code path succeeds while the indirect wrapper-mediated one (line 203, used by the primary
`auth-server-undertow` container) fails, which would strongly localize the bug to however this receiver
reference is threaded through the wrapper class (a stored field read back later, a lambda/method-reference
capture, or a GC-triggered relocation of the synthetic object losing the field along the way — this project has
precedent for exactly this class of bug, e.g. the recent `d64fab850 fix(vm): pin invoke_virtual lambda-dispatch
receiver/args across SAM-compat GC risk`).

## Next steps

1. Confirm whether `KeycloakUndertowJaxrsServer.start(Undertow.Builder)` is real JDK/library bytecode (RESTEasy)
   or something CratonVM has synthetic handling for — if real bytecode, trace exactly what it does with the
   builder/undertow references internally (does it call `.build()` once and store the result, or could it call
   `.build()` twice — which would create TWO different registered instances, and if `.start()` gets called on
   whichever ISN'T the one most recently stored somewhere, exactly this symptom would occur).
2. Add temporary tracing to `native_builder_build`/`native_undertow_start` (print the allocated `undertow` object's
   pointer/id at `build()` time and the receiver's pointer/id at `start()` time) to see directly whether they're
   the same object or two different ones.
3. Check whether this reproduces via a minimal, Arquillian-free repro exercising the exact
   `KeycloakUndertowJaxrsServer.start(Undertow.builder()...)` call pattern in isolation.
4. Given the sheer volume (300+ classes), this is likely the single highest-value fix available from this
   investigation pass — fixing it would unblock the large majority of `testsuite/integration-arquillian/tests/base`
   for real bug-hunting (currently, no useful signal can come from any of these classes since they all die at
   this single early bootstrap step).

## Repro

```
cd /data/wt-keycloak-nonpassed-azure-20260715
pwsh -NoProfile -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-undertow-instanceid -ClassList <(printf 'module\tclass\ntestsuite/integration-arquillian/tests/base\torg.keycloak.testsuite.cli.registration.KcRegTest\n') -KeycloakRoot apps/keycloak -Exe target/release/cratonvm-nonpassed-azure-20260715 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64
```

Check the `.err.log` for `[cratonvm] main-vm run() returned Err: Error in thread "main" internal error: Undertow.start: instance id missing`.

## Evidence

309+ classes (and rising as the run continues) across
`apps/keycloak-suite-runner/.suite/results/azure-nonpassed-shard{1,2,3,4}/all-jit/logs/testsuite_integration-arquillian_tests_base.*.err.log`
on the Azure host, 2026-07-16, binary `cratonvm-nonpassed-azure-20260715` built from `dev` commit `a9b838c4`.
Native source: `native-builtins/src/wildfly_undertow.rs` (`native_builder_build` ~line 594-626,
`native_undertow_start` ~line 629-640). Call site:
`apps/keycloak/testsuite/integration-arquillian/servers/auth-server/undertow/src/main/java/org/keycloak/testsuite/arquillian/undertow/KeycloakOnUndertow.java:203`.
Related earlier (Windows-side, different root cause) finding:
`docs/known-issues/keycloak/arquillian-auth-server-undertow-container-not-found.md`.
