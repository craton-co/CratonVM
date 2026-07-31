# `sun/jdk.internal.reflect.ReflectionFactory` serialization hooks are present in `cargo build --workspace` and absent from `cargo build -p cratonvm-cli`

## Status
**OPEN — build-configuration divergence.** Found 2026-07-31 while auditing the
`experimental-serialization` feature gate for
`jdk/internal/misc/VM.latestUserDefinedLoader0()` (that one is FIXED — see
`docs/internal/fixed-suite-bugs/h2-suite-bugs/`). This is the *other* thing
that gate turned out to be hiding, and it is deliberately left alone here
because flipping it is a behaviour change, not a link fix.

## Severity
**MEDIUM** — not a crash. The defect is that two builds of the same commit run
different serialization code, so a suite result does not predict a CI result
and vice versa.

## What is gated
`native-builtins/src/lib.rs`, inside `register_essential_natives_with_shims`
(the real-JDK path):

```rust
#[cfg(feature = "experimental-serialization")]
serialization::register_reflection_factory_serialization(registry);
```

That registrar installs 18 overrides across
`sun/reflect/ReflectionFactory` and `jdk/internal/reflect/ReflectionFactory`:
`getReflectionFactory`, `newConstructorForSerialization` (both arities),
`newConstructorForExternalization`, `readObjectForSerialization`,
`readObjectNoDataForSerialization`, `writeObjectForSerialization`,
`readResolveForSerialization`, `writeReplaceForSerialization`,
`hasStaticInitializerForSerialization`.

Its own comment says it is there because "Real-JDK mode does not call the full
synthetic/experimental `register_builtins` surface, but JBoss Marshalling calls
`sun.reflect.ReflectionFactory` directly for serialization hooks" — i.e. it
claims a **real-JDK-mode** role while sitting behind a feature that is
default-off for both `cratonvm-vm` (`default = ["awt", "management"]`) and
`cratonvm-cli`.

## Why the two builds differ
`libcratonvm/Cargo.toml` and `cratonvm-embed/Cargo.toml` both list
`experimental-serialization` in their **default** feature set. Cargo unifies
features across every member built in one invocation, so:

| invocation | `experimental-serialization` on `cratonvm-native-builtins`? |
| --- | --- |
| `cargo build --workspace` / `cargo test --workspace` (CI's Build & Test job) | **YES** — via `libcratonvm` |
| `cargo build --release -p cratonvm-cli` (every suite runner) | **NO** |

Verified with `cargo tree -e features -i cratonvm-native-builtins`, run both
`--workspace` and `-p cratonvm-cli`. `experimental-aot` and
`experimental-debug` leak the same way.

So the `cratonvm` binary CI produces has these ReflectionFactory overrides
active; the `cratonvm` binary every H2/Hibernate/Spring suite session builds
does not.

## Why this is NOT simply ungated
Unlike `VM.latestUserDefinedLoader0()` — a genuinely `native` JDK method whose
absence is an `UnsatisfiedLinkError` — none of these 18 methods are native in
the real JDK. They are ordinary bytecode in `java.base`. Registering them
*replaces* working JDK bytecode with Rust implementations across all
serialization. Turning that on by default is a wide behaviour change and no
currently-failing test asks for it, so it needs evidence first.

## What to do
1. Decide the intent. Either the JBoss Marshalling case is real, in which case
   ungate and re-run the Hibernate/WildFly suites to price the blast radius;
   or it is not, in which case delete the call and the dead registrar.
2. Independently: stop `libcratonvm`/`cratonvm-embed` default features from
   silently deciding what a `--workspace` build contains. Either drop the
   `experimental-*` features from their defaults, or accept the split and keep
   testing the `-p`-scoped resolve explicitly (CI now does, see the
   "Default-feature native registry surface" step in `.github/workflows/ci.yml`).

## Related
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md`
  — the fixed sibling, and where the feature-unification analysis was done.
- `docs/internal/fixed-suite-bugs/hibernate/hib-linux-fail-bucket-triage-20260703.md`
