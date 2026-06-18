# Fix note — nb-appserver-frameworks

Source review: `docs/reviews/fable-2026-06-10/nb-appserver.md` (findings B3, B5).

## Finding

Two forbidden app-specific synthetic shims were live in the **default**
real-JDK build path (both files are wired unconditionally from
`register_essential_natives` in `lib.rs`):

- **B3 (HIGH, forbidden "fake main" stub)** — Quarkus
  `io.quarkus.bootstrap.runner.SerializedApplication.read(InputStream, Path)`
  was natively replaced with a synthetic object whose `mainClass` defaulted to
  the hardcoded Keycloak 26 main class
  (`org.keycloak.quarkus.runtime.KeycloakMain`), bypassing the real `.dat`
  bootstrap parse. This masks an underlying VM NIO bug (`DataInputStream.readInt`
  over `Files.newInputStream` returns a drifted value on the second read).
- **B5 (HIGH, forbidden fabricated-config stub)** — Spring's `Environment`
  property access was fully faked: `getProperty()` → null, `containsProperty()`
  → false, `acceptsProfiles[_arr]()` → true, `getActiveProfiles()` → empty.
  Any Spring app reading config through `Environment` got wrong/empty answers.
  This masks an `AbstractEnvironment.<init>` CLDR/resource-bundle NPE in
  CratonVM's partial bootstrap.

## Root cause

Both are framework-behavior fakes installed in the default build to paper over
real VM defects. Per the no-synthetic-stubs policy, the default build must not
fabricate app behavior; the underlying VM bugs (NIO `readInt` drift; Spring
`AbstractEnvironment` init NPE) are the correct fix targets but are out of scope
for this surgical change. Therefore both fakes are gated behind the default-OFF
`app-stubs` cfg feature (matching the established `apps_h2.rs` precedent), so the
default build runs the real bytecode and surfaces the underlying bug honestly.

## Exact change

### B3 — `native-builtins/src/quarkus_staticinit.rs`

- Gated the **entire** body of `register_bootstrap_runner` behind
  `#[cfg(feature = "app-stubs")]`, with a `#[cfg(not(feature = "app-stubs"))]
  let _ = registry;` arm so the fn still compiles and its signature is unchanged
  (the `lib.rs` caller is untouched). The natives were already tagged
  `NativeKind::SyntheticStub`; the tag is preserved inside the gated block.
  - The `read` native is the actual fake (hardcoded Keycloak main). The
    accessor (`getRunnerClassLoader`/`getMainClass`) and `RunnerClassLoader`
    `loadClass`/`findClass` natives are gated **alongside** `read` because they
    only make sense against the synthetic `SerializedApplication` that `read`
    produces; with `read` gone by default they would otherwise shadow the real
    bytecode's own accessors. So with `app-stubs` OFF, the real
    `SerializedApplication.read` / `RunnerClassLoader` bytecode runs.
  - Expanded the fn doc-comment to FLAG the SyntheticStub rationale and the
    gating.
- Gated the three registration-surface assertion tests behind
  `#[cfg(feature = "app-stubs")]` so the default test build does not assert the
  (now-absent) registrations:
  `t19_h3_register_bootstrap_runner_natives_lists_all_entries`,
  `t19_h3_current_version_expectation_native_returns_compile_time_constant`,
  `t19_h5_register_bootstrap_runner_includes_load_class`.
  - The remaining bootstrap-runner tests call the native fns directly (not via
    the registry) and verify the native logic itself; they are left intact.

### B5 — `native-builtins/src/spring_startup_bootstrap.rs`

- Gated the faked-`Environment`-getter registration loop
  (`for env_class in &[STD_ENV, ABS_ENV, ENV_IFACE, CONF_ENV] { ... }`, the
  `getProperty`/`containsProperty`/`acceptsProfiles`/`getActiveProfiles`/
  `getDefaultProfiles`/`getPropertySources`/`resolve*Placeholders` overrides)
  behind `#[cfg(feature = "app-stubs")]`. The whole `register` fn is already
  tagged `NativeKind::SyntheticStub` via `set_category` above, so the tag
  applies. With `app-stubs` OFF the real Spring `AbstractEnvironment` /
  `StandardEnvironment` bytecode answers property queries.
  - Added a `let _ = (STD_ENV, ABS_ENV, ENV_IFACE, CONF_ENV);` reference so the
    default build does not warn about now-unused constants.
  - Expanded the comment block to FLAG the SyntheticStub rationale and gating.
  - **Left untouched** the `getEnvironment`/`createEnvironment`/
    `getOrCreateEnvironment` overrides — these are non-null guards that prefer
    the real `StandardEnvironment.<init>()` and do NOT fabricate config values;
    only the property-access getters (the B5 fake) are gated.
- Split the `environment_intercepts_registered` test: the `getEnvironment`
  assertion stays (always registered); the `StandardEnvironment.getProperty`
  assertion moved to a new `app-stubs`-gated test
  `environment_property_access_intercepts_registered_under_app_stubs`.

## Files touched

- `native-builtins/src/quarkus_staticinit.rs`
- `native-builtins/src/spring_startup_bootstrap.rs`
- `docs/reviews/fable-2026-06-10/fixes/nb-appserver-frameworks.md` (this note)

## Tests added

None new beyond the split/regating described above (no new behavior to assert —
the change is a default-OFF gate). Default-build test correctness is preserved:
registration-surface assertions that depend on the now-gated natives are
themselves `#[cfg(feature = "app-stubs")]`-gated, and the direct-native-call
tests still exercise the native logic.

## Follow-up & risk

- **Compile**: unchanged fn signatures; `lib.rs` callers untouched. Now-unused
  native fns under the default build are covered by the workspace
  `dead_code = "allow"` lint (`native-builtins` inherits via
  `[lints] workspace = true`). No `#[cfg_attr(... allow(dead_code))]` was added
  per-fn since the workspace lint already allows it; this mirrors the net effect
  of the `apps_h2.rs` precedent.
- **Behavioral**: with the default build, Quarkus/Keycloak boot will now hit the
  real `.dat` parse (and may surface the NIO `readInt` drift bug), and Spring
  apps will read real config via `Environment` (and may surface the
  `AbstractEnvironment.<init>` init NPE). This is the intended, policy-correct
  outcome: the underlying VM bugs now fail honestly instead of being faked. To
  restore the prior (faking) behavior for triage, build with
  `--features app-stubs`.
- **Real fixes (out of scope here, recommended next)**:
  1. NIO `DataInputStream.readInt` over `Files.newInputStream` arbitrary-offset
     reads (unblocks B3 — real `.dat` main-class decode).
  2. `AbstractEnvironment.<init>` CLDR/resource-bundle NPE in the partial
     bootstrap (unblocks B5 — real property-source chain).
