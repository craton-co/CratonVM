# Native capability and application-pack boundary remediation

Status: fixed on `codex/complete-architecture-remediation-20260726`.

## Problems

The native ABI exposed 280 unrelated VM operations through one
`NativeContext` implementation. Adding a native therefore made class loading,
heap mutation, Java invocation, threads, exceptions, GPU offload, process I/O,
and diagnostics look like one architectural dependency.

`register_essential_natives` also installed four large third-party
compatibility families on every boot. Some family registrars mixed JDK bridges
with application-owned overrides, so simply making the family conditional
would have removed JAAS, JNDI, or `DataSource` support from unrelated programs.

## Changes

The implementation contract is split into seven capability facets:

| Facet | Operations |
|---|---:|
| `NativeClassAccess` | 97 |
| `NativeInvokeAccess` | 11 |
| `NativeHeapAccess` | 66 |
| `NativeThreadAccess` | 42 |
| `NativeExceptionAccess` | 4 |
| `NativeGpuAccess` | 12 |
| `NativeSystemAccess` | 48 |

`NativeContext` is now an empty compatibility marker composed from those
facets. The production VM adapter and all test adapters implement the facets
independently. New helpers can name the narrowest facet they need without
depending on the rest of the VM. The callback function-pointer ABI remains
source-compatible through the zero-method marker; it is no longer the
implementation surface.

Application overrides are grouped into four explicit packs:

- application intrinsics;
- BouncyCastle;
- datasource/pool integrations;
- JBoss/WildFly/XNIO.

VM startup derives an immutable `ShimSelection` from existence-only lookups in
the already-built application classpath resource indexes. No witness class is
loaded and no JAR member is inflated. Only selected packs populate the native
registry.

JDK-owned methods were removed from the application registrars and placed in
unconditional core registrars:

- `register_jdk_security_natives`;
- `register_jdk_naming_natives`;
- `register_jdk_datasource_natives`.

The pack tests reject any registration outside a pack's declared application
namespace and verify that disabling every pack preserves the five core JDK
bridge families. They also compare callback keys so a core registration cannot
silently overlap an application pack.

Native configuration reads now use the process-start `VmFlags` snapshot.
Tests exercise pure inputs or explicit state setters rather than changing
declared environment variables after the snapshot has latched.

## Verification

- `cargo check -p cratonvm-vm`: pass.
- `cargo test -p cratonvm-native-api`: all executed tests pass.
- `cargo test -p cratonvm-native-io`: 369 passed, 1 ignored.
- `cargo test -p cratonvm-native-collections`: 149 passed across its test
  binaries.
- `cargo test -p cratonvm-native-builtins`: 3100 passed, 6 ignored, with five
  failures independently reproduced at the pre-change commit `16587f6f6f`.

The five baseline failures are:

- `cglib_enhancer::fb_ref_bytecode_tests::fb_ref_splice_shifts_exception_table_by_exactly_8_bytes`;
- `lang_string::tests::string_join_array_uses_to_string_for_custom_charsequence`;
- `logmanager::tests::t19_h3_get_logger_names_returns_snapshot_enumeration`;
- `logmanager::tests::t19_h3_reset_clears_logger_registry_but_keeps_singleton`;
- `regex_matcher::regex_lookbehind_tests::pem_block_to_der_roundtrip`.

They are unrelated pre-existing defects and remain tracked outside this
architecture remediation.
