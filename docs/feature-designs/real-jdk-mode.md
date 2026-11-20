# Foundational OpenJDK execution — runtime contract

CratonVM executes authentic OpenJDK class libraries directly. Real class bytes
are authoritative. The launcher defaults to `--real-jdk`; `--compatible` enables
the cold-start and throughput optimization layer. Both policies target HotSpot
25's Java-visible behavior, including exceptions, reflection, and debugger answers.

## 1. Normative semantics

Foundational execution requires a real JDK runtime image. It rejects synthetic
stub natives and compatibility-class fabrication. Concrete Java bytecode wins
over a registered bridge or stub; only a reviewed intrinsic may override it.
`ACC_NATIVE` methods require a bridge or reviewed intrinsic; absence is a
structured error rather than an invented result.

VM arrays, hidden classes, lambdas, proxies, and reflection accessors are
legitimate generated classes with distinct origins. They are not compatibility
stubs. An error must name the failing class/method/descriptor and policy context
so the caller can diagnose it or explicitly select compatible execution.

## 2. Crate layering

`types` owns policy tokens. `native-api` owns native kinds and registration;
`classloading` owns class origins. The VM combines them at dispatch. Strictness
is a runtime policy, separate from the class-library choice and Cargo features.
Compatibility state belongs to each VM, registry, and class manager; process-global
compatibility state is prohibited.

## 3. Contract: `types` crate

`JdkMode::{Real, Synthetic}` selects the class library.
`CompatibilityMode::{JdkOnly, Compatible}` selects allowed substitutions and is
carried by `ExecutionPolicy`. These Rust identifiers and the machine-readable
`jdk-only` report value remain stable API/wire names.

## 4. Contract: `native-api` crate

Every registrar sets its own `NativeKind` category and restores it on exit.
The registry has an ambient category defaulting to `SyntheticStub`; omission can
silently classify a genuine bridge as a stub, which strict registration drops.
A `with_category` scope around a function pointer covers that function's dynamic
call tree. Registration is last-write-wins for each class/name/descriptor triple;
classify the final registration explicitly.

Strict registration and invocation reject `SyntheticStub`. Compatible execution
can use the optimization implementations but still targets HotSpot behavior.
A semantic improvement in either policy is a fix; preserving a known compatible
mode divergence is not a requirement.

## 5. Contract: `classloading` crate

Real class bytes determine class identity, layout, and executable Java methods.
Strict policy rejects `ClassOrigin::CompatibilityStub` fabrication for ordinary
classes. Generated classes retain explicit origins; the presence of a Java
`ACC_SYNTHETIC` flag does not make a class a compatibility stub.
See [class resolution](class-resolution-and-caching.md) and
[synthetic-class fallibility](synthetic-class-fallibility.md).

## 6. Contract: `vm/src/config.rs`

The launcher uses `LAUNCHER_DEFAULT_COMPATIBILITY_MODE` (`JdkOnly`). Embedding
entry points deliberately use `Compatible`, including `VmConfig::default`,
`VmConfig::with_host_jdk_default`, `libcratonvm`, and `cratonvm-embed`.
The embedding defaults are API behavior and must not be changed merely to match
the launcher. A strict policy requires `JdkMode::Real`.

## 7. Contract: `vm/src/vm/vm_exec.rs` + interpreter

Resolve native versus bytecode execution through `resolve_dispatch`. Concrete
real Java bytecode takes precedence over bridges and stubs; reviewed intrinsics
have explicit admission. A new class-name allow-list is not a substitute for
fixing resolution. Inherited declarations, abstract methods, and final native
registration provenance must be resolved against the actual target method.

Implementation limits and compatible dispatch exceptions are described in
[native dispatch](native-dispatch-architecture.md) and
[natives over real JDK classes](../architecture/natives-over-real-jdk-classes.md).
They do not change the HotSpot parity target.

## 8. Contract: `vm/src/vm/vm_init.rs`

Propagate the VM's policy into its registry and class manager before admission
and class loading. Boot must use the selected real runtime image and report
missing prerequisites. A Cargo feature or environment variable cannot stand in
for the configured runtime policy.

## 9. Contract: `vm-cli/src/main.rs`

| Invocation | Library | Policy |
|---|---|---|
| Bare launcher or `--real-jdk` | Real | Foundational (`JdkOnly`) |
| `--compatible` or `--fast-boot` | Real | Compatible optimization layer |
| `--synthetic-jdk` | Synthetic | Compatible; requires the build feature |
| `--real-jdk --compatible` or `--real-jdk --synthetic-jdk` | — | Conflict |

`--real-jdk` states the default explicitly. `--fast-boot` aliases `--compatible`.
Diagnostics include `--real-jdk-report`, `--dump-class-origins`,
`--trace-real-jdk`, and `--explain-real-jdk`. Machine-readable schema fields and
legacy diagnostic names are not alternate architecture targets.

The native census records final kinds, registration provenance, class origins,
and invocation information. Report the selected policy and JDK with evidence;
a compatible census is not proof of strict admission. Absolute diagnostic paths
are redacted unless explanation is explicitly requested.

## 10. Enforcement posture

Strict execution rejects compatibility fabrication and stub admission. Compatible
execution permits the optimization layer. Both preserve Java semantics, and
changes to either must be checked against HotSpot under the other policy too.
See [per-VM state](../architecture/per-vm-state.md) for shared-state limits and
[known issues](../known-issues/) for unresolved behavior.

## 11. Acceptance criteria

- Foundational execution requires a valid real JDK image.
- Its final registry has no synthetic stubs; its ordinary classes have real bytes.
- Native dispatch uses reviewed intrinsics, genuine native bridges, and explicit origins.
- Failures carry actionable class, method, descriptor, origin, and policy context.
- Tests compare both execution policies with HotSpot, and clean checkouts need
  no local internal archive to run their public contracts.
