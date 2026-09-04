# H24-1 — the second door onto the module source, closed at the source rather than at the guard; and the two modules a boot-layer probe could not see

**Status: FIXED IN SOURCE, NOT VERIFIED BY AN ARM.** Lane H24, 2026-08-21.
This lane must not build, so the change below has been **read, reasoned about
and committed, and never executed.** Treat it exactly as `H19-1` asked to be
treated: as a diagnosis with a patch attached, not as a closed vector.

Diagnosis measured on the prebuilt `C:/craton/cratonvm-r8.exe` (clean build at
`025780ff7`). Oracle HotSpot **25.0.3+9**.

Every claim is **MEASURED** (this lane ran it today) or **ARGUED** (read it).

---

> **VERIFIED AGAINST A BINARY 2026-09-04, partially — falsifier 2 is answered,
> falsifier 1 could not be observed.** Status was **FIXED IN SOURCE, NOT
> VERIFIED BY AN ARM**: *"read, reasoned about and committed, and never
> executed."*
>
> **Falsifier 2, "the load-bearing negative control", does not fire.**
> `RJdkModule` was predicted to go red if the change is wrong. It passes, in
> both modes, and so do its two siblings — run through
> `regression-suite/run.sh`, which is the harness that builds the
> `--module-path` these vectors need:
>
> ```text
> RJdkModule                  PASS   (compatible and --jdk-only)
> RJdkServices                PASS   19 checks
> RServiceLoaderDoubleSource  PASS
> ```
>
> `RServiceLoaderDoubleSource` passing is the substantive one: this record's
> measured before-state is that *"the vector then dies at
> `RServiceLoaderDoubleSource.java:490`"*. It no longer dies.
>
> **Falsifier 1 is NOT settled here, and the reason is mine, not the VM's.**
> The check is whether the diagnostic still prints `providers=2`. No `[SL-DBG]`
> line could be captured: through `run.sh` the diagnostic did not reach the
> captured output, and invoked directly — even with the suite's own
> `--module-path regression-suite/build-modules --add-modules
> cratonvm.jdkonly.svc` — the vector produces no output at all, so that
> invocation is not the harness's and its silence is evidence about my command
> line, not about the filter. **No claim is made about `providers=`.**
>
> That matters because this record says the two consequences travel together:
> *"the filter is not reaching this module and `desc.automatic` is false for it
> — which would also make `E4-R11`'s boot-layer guard inert, contradicting the
> `:459` assertion that passes today."* The half that could be checked — the
> vectors — is green. The half that names the mechanism is unmeasured.
>
> **What this does NOT verify.** The diagnosis in §1 was MEASURED on
> `cratonvm-r8.exe` and is not re-derived. The two modules a boot-layer probe
> could not see are the record's subject and no probe here looks for them; a
> green vector is not a census. Nothing here is `--synthetic-jdk`.

## 1. The diagnosis is live, not stale — re-measured before acting

`D1-R11`'s fix landed and its diagnosis expired; `H15-3` said so, and a triage
page is stale the day after it is written. So the first thing this lane did was
re-run the instrument rather than trust the record.

**MEASURED**, `CRATONVM_DIAG_SERVICELOADER=1`, Compatible mode, on `r8` today:

```
[SL-DBG] ServiceLoader service=com.cratonvm.jdkonly.svc.Greeter
         loader_delegation=false descriptors=0 providers=2
         (["com.cratonvm.jdkonly.svc.internal.EnGreeter",
           "com.cratonvm.jdkonly.svc.internal.FactoryGreeter"])
```

Byte-for-byte what `H15-1` §5.1 recorded a day earlier. The vector then dies at
`RServiceLoaderDoubleSource.java:490`:

> `ServiceLoader's provider set must be exactly what the META-INF/services
> descriptors on the class path name: got 2 provider(s), descriptors name 0`

**The boot-layer door is confirmed closed in the same run**: the vector's
`ModuleLayer.boot().findModule(<the -cp module>)` assertion is at `:459` and the
run reaches `:490`, so `E4-R11`'s guard is live and `module_is_class_path_only`
answers `true` for this module. That also disposes of `H15-1` §5.4's falsifier
1 — the `automatic` flag is not the thing that is broken.

**And the tree still matches the diagnosis**: `service_providers_from_declared_modules`
did not exist anywhere in the workspace, and `vm/src/vm/vm_exec.rs:9511` still
forwarded verbatim to the unfiltered `ModuleRegistry::service_providers`. The
patch `H15-1` wrote out had been applied nowhere.

## 2. The defect

`native-builtins/src/service_loader.rs` collects providers from the module
source behind this guard:

```rust
    if !loader_view_is_exhaustive {
```

`loader_view_is_exhaustive` is a property of the **LOADER**. The rule is about
the **MODULE**: a modular jar reached through `-cp` is an unnamed-module citizen
and a real JVM ignores its `module-info` outright, every `provides` clause
included. The receiver here is the system application loader, which carries no
recorded URL list, so the guard is false and the arm fires.

It cannot be repaired at the consumer. `ModuleRegistry::service_providers`
returns provider **names** with the owning module discarded, so by the time
`service_loader.rs` holds the strings the module they came from is gone.

## 3. What changed, and why it is not the shape `H15-1` wrote out

`H15-1` §5.3 specified two hunks: **add**
`service_providers_from_declared_modules` to `classloading/src/module.rs`, then
point `vm/src/vm/vm_exec.rs:9511` at it.

`vm/src/vm/vm_exec.rs` is **outside this lane's ownership**. Landing hunk 1
alone would have added a function with no caller — a consumer-less producer,
inert, and indistinguishable from a feature at a glance.

So the filter went into `ModuleRegistry::service_providers` itself. This is
sound because of a fact `H15-1` did not state: **that function has exactly ONE
production caller in the entire workspace** — `vm_exec.rs:9511` — the very
caller that needs the filter. Its other three references are its own unit tests.
So the two designs have identical reach, and this one needs no edit outside the
lane's files.

It is also the better shape on its own merits. There is no legitimate consumer
of the unfiltered walk — the JDK rule admits no exception — and a correct twin
sitting beside a wrong original is precisely the arrangement where one of ten
call sites gets the fix.

The diff is four lines of code inside a long doc comment:

```rust
        for desc in self.modules.values() {
            // The rule is about the MODULE, not the loader that asked.
            if desc.automatic {
                continue;
            }
```

## 4. The load-bearing safety check — the platform leans on this door HARDER than the app does

This is the half that could have gone very wrong, and it is the reason the
record spends more space here than on the fix.

**MEASURED**, same diagnostic run, per service:

| service | descriptors | providers |
|---|---:|---:|
| `com.cratonvm.jdkonly.svc.Greeter` | 0 | **2** ← the defect |
| `java.security.Provider` | **0** | **8** |
| `java.util.spi.ToolProvider` | **0** | **9** |
| `java.nio.charset.spi.CharsetProvider` | **0** | **1** |
| `java.nio.file.spi.FileSystemProvider` | 1 | 2 |

**Seventeen platform providers arrive through a `provides` clause and through
nothing else.** A filter that caught them would silently delete the entire JCA
provider set and `ToolProvider.getSystemJavaCompiler()` — and it would do so
with no error, because `ServiceLoader` returning an empty iterator is a legal
answer. This is the `H22` shape exactly: a change that scores as a win while
quietly removing service.

### 4.1 The probe that could not answer it, and why I am recording the failure

The first instrument was a boot-layer membership probe: `populate_boot_layer_modules`
skips exactly the `automatic` modules, so "present in `ModuleLayer.boot()`"
should imply "would survive the filter". Seventeen module names, one process
(`H0-8`: no multi-case probes).

**MEASURED, CratonVM:** `bootModules=70`, missing: `[jdk.naming.ldap, jdk.jnativescan]`.

Two missing looks like the filter is about to delete `JdkLDAP` and the
`jnativescan` tool provider. **It is not.** MEASURED on HotSpot 25.0.3+9, the
same probe: `bootModules=62`, missing `[jdk.naming.ldap, jdk.jnativescan]` —
**the identical two.** Their absence is ordinary JPMS root resolution: neither
is resolved as a root in a plain `-cp` launch. It says nothing about `automatic`.

**The probe reported its own reach.** Boot-layer membership is a sufficient but
not necessary condition, so its negatives are worthless, and only running the
oracle revealed that. Without the oracle arm this record would have carried two
invented hazards — the same species as the anchor diff that "invented 5 missing
CAs" by keying on subject DNs.

### 4.2 The discriminator that does answer it

`automatic` is stamped in **exactly two places** in the workspace
(grep-exhaustive on `automatic:` and `desc.automatic =`), and both answer
`false` for a platform module — so the runtime probe was never needed:

1. `classloading/src/class_manager.rs:2829-2832`, the eager scan, hardcodes the
   flag per search path: `bootstrap → false`, `extension → false`,
   **`application → true`**. Only the app class path produces `automatic`.
2. `classloading/src/class_manager.rs:6181`, the lazy path:
   `desc.automatic = !already_explicit && !is_platform_module_name(&desc.name);`
   and `is_platform_module_name` (`module.rs:1397`) is `true` for every
   `java.*` / `jdk.*` / `javafx.*` / `oracle.*` name.

`jdk.naming.ldap` and `jdk.jnativescan` both start with `jdk.`, so both are
`automatic = false` on either path and both survive the filter. Genuine
`--module-path` modules are the `already_explicit` half: `vm_init` re-registers
them with `automatic = false`, which is what `RJdkModule` asserts.

**ARGUED, not measured** — this is a source-level argument, and the runtime
probe that would have confirmed it is the one §4.1 shows cannot.

## 5. Prediction, and what would falsify it

**PREDICTED:** `RServiceLoaderDoubleSource` goes **GREEN** in `SUITE=all`
(1266 checks).

Falsifiers, in the order they should be checked:

1. **The diagnostic still prints `providers=2`.** Then the filter is not
   reaching this module and `desc.automatic` is false for it — which would also
   make `E4-R11`'s boot-layer guard inert, contradicting the `:459` assertion
   that passes today. This is a tight prediction precisely because those two
   consequences travel together.
2. **`RJdkModule` goes red.** The load-bearing negative control: it asserts a
   `--module-path` module's providers arrive through both the catalog and layer
   routes. Red means `vm_init`'s explicit re-registration is not reaching it.
   **MEASURED GREEN pre-fix today** — `PASS RJdkModule (163 checks)`, and
   byte-identical to HotSpot — so a post-fix red is attributable to this change
   and nothing else.
3. **`RJdkServices` goes red, or `ToolProvider.getSystemJavaCompiler()` returns
   null.** That is §4 realised. **MEASURED GREEN pre-fix today.**
4. **`--jdk-only` moves off 105/105.** It should not — but **not** for the
   reason a first reading suggests, and I got this wrong before measuring it.

   **MEASURED today on `r8`:** `--jdk-only` gives
   `PASS RServiceLoaderDoubleSource (1266 checks)` while Compatible mode fails.
   So strict mode is not merely "unaffected" — it *already passes*, which
   confirms `H15-1`'s five-of-five framing for this vector as well. The reason
   is `H15-1` §3's mechanism C: strict mode drops the **whole native
   `ServiceLoader`** as a `SyntheticStub`, and the real JDK bytecode never
   consults CratonVM's module registry at all.

   That matters for what this change is: **`ModuleRegistry::service_providers`
   is on the Compatible path only.** The fix cannot move the strict arm in
   either direction, because strict mode does not call it. A strict-arm change
   of any kind would therefore mean something unrelated moved — and would be a
   genuine surprise rather than a graded failure.

**NOT PREDICTED:** that the vector reaches `PASS` rather than merely getting
past `:490`. `classPathModuleSeparation` is the last section it runs, so there
is no known assertion behind this one — but "no known assertion behind it" is
what `H15-3` said about `Function.andThen`, and `H24-3` is the record of that
being wrong by three families.

## 6. What this lane did NOT verify

* **No arm ran against the change.** Not one. The lane cannot build.
* Whether any **application** suite depends on a `-cp` modular jar's `provides`
  being honoured. Such an app is already broken on HotSpot in the same
  configuration, so matching the oracle is right — but the blast radius across
  the Spring / Hibernate / Netty suites is **unmeasured**.
* The `providerCensus` section's stability assertions were **read** and argued
  unaffected (it asserts no-duplicates, no-drift and cache identity, and its
  `CK` line prints `services=6` — a count of services probed, not of providers,
  so a service dropping from 2 providers to 0 does not move it). Not re-run
  under the change.

## 7. NOMINATIONS

* **N1 — run `SUITE=all` on a build carrying this change** and check the four
  falsifiers in §5 in that order. Until then this is a diagnosis.
* **N2 — `ModuleRegistry::service_providers` should return the owning module
  alongside each provider name.** The reason this defect could not be fixed at
  the consumer is that the function discards the one fact the rule is about.
  Three ServiceLoader-shaped consumers inherit a filter they cannot see, and a
  fourth will eventually want a different one.
* **N3 — the `CRATONVM_DIAG_SERVICELOADER` line should print the owning module
  per provider.** It prints `descriptors=N providers=M` and the provider names;
  had it named the module, §4's whole platform-safety analysis would have been
  one run instead of a failed probe plus a source audit.
* **N4 — boot-layer membership must stop being used as a proxy for
  `automatic`.** It appears sound and is not (§4.1). If a runtime witness for
  `automatic` is wanted, expose it directly — a one-line addition to the
  `--dump-class-origins` census would do it, and it would have answered §4 in
  one run on both VMs.
