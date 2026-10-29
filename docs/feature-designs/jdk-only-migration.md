# JDK-Only Mode and Compatible Execution Architecture

## Overview and Doctrine

CratonVM is engineered with **JDK-only mode as its foundational primary architecture**: the virtual machine executes authentic OpenJDK class libraries directly from the host JDK runtime image (`jimage` / `lib/modules`), treating authentic OpenJDK bytecode as authoritative.

**Compatible mode (`--compatible`)** serves as an engineered optimization layer. It provides fast cold-start loading paths and specialized throughput execution paths using synthetic stubs and specialized native bridges where full bytecode initialization or complex reflection scaffolding can be safely accelerated.

Both modes target OpenJDK (HotSpot 25) byte-for-byte in Java-visible behavior, including standard exception types, stack traces, reflection metadata, and JNI contracts.

---

## Launcher Defaults and Mode Selection

The `cratonvm` command-line launcher defaults to **JDK-only mode**. Conversely, embedding entry points (`libcratonvm`, `cratonvm-embed`, `VmConfig::default`, `VmConfig::with_host_jdk_default`) default to **Compatible mode** to ensure immediate, zero-configuration embeddability.

### Flag Matrix

| Invocation | `JdkMode` | `CompatibilityMode` | Behavior |
|---|---|---|---|
| `cratonvm Main` *(no flag)* | `Real` | `JdkOnly` | Default launcher execution. Authentic JDK class bytes are authoritative. |
| `cratonvm --jdk-only Main` | `Real` | `JdkOnly` | Explicitly enforces foundational execution against real JDK bytecode. |
| `cratonvm --compatible Main` | `Real` | `Compatible` | Enables the compatible optimization layer for fast cold start and throughput paths. |
| `cratonvm --real-jdk Main` | `Real` | `Compatible` | Deprecated alias for `--compatible`. |
| `cratonvm --synthetic-jdk Main` | `Synthetic` | `Compatible` | Uses standalone synthetic fallback library (requires `synthetic-jdk` feature). |
| `--jdk-only` + `--compatible` | — | — | **Configuration error** (mutually exclusive policies). |
| `--jdk-only` + `--synthetic-jdk` | — | — | **Configuration error** (synthetic classes violate strict mode). |

---

## Contract of JDK-Only Mode

When running in JDK-only mode:

1. **Authoritative Bytecode**: Authentic OpenJDK runtime image (`lib/modules`) is mandatory. No non-array class is fabricated out of thin air.
2. **Native Dispatch Strictness**: No `NativeKind::SyntheticStub` is registered or invoked. Concrete Java bytecode unconditionally beats registered bridges unless a reviewed, provably equivalent `NativeKind::Intrinsic` is established.
3. **Structured Fallibility**: Unimplemented `ACC_NATIVE` methods result in structured `MissingNative` errors rather than synthetic mock objects or silent no-ops.
4. **Legitimate Synthetic Artifacts**: Arrays, hidden classes, invokedynamic/lambdas, dynamic proxies, and reflection accessors generated at runtime are fully supported and carry explicit class origins (`ClassOrigin::Generated*`), distinct from compatibility stubs.

---

## Operational Execution

### Compiling and Running

```bash
# Build release launcher
cargo build --release -p cratonvm-cli

# Run an application under foundational JDK-only mode
target/release/cratonvm \
  --jdk-only \
  --java-home "$JAVA_HOME" \
  -cp /path/to/classes \
  com.example.Main
```

### Diagnostic Instrumentation

To inspect class loading, native registrations, and violation provenance, CratonVM exposes detailed diagnostic flags:

```bash
target/release/cratonvm \
  --jdk-only \
  --java-home "$JAVA_HOME" \
  --jdk-only-report              /tmp/cratonvm-jdk-only.json \
  --dump-native-registry         /tmp/cratonvm-natives.json \
  --dump-class-origins           /tmp/cratonvm-classes.json \
  --dump-missing-natives-grouped /tmp/cratonvm-missing.json \
  -cp app.jar \
  com.example.Main
```

- `--trace-jdk-only`: Logs each violation immediately as it occurs during execution.
- `--explain-jdk-only`: Provides detailed explanations for each recorded violation. Absolute system paths are redacted by default unless this flag is passed.
- `--dump-native-registry`: Serializes all registered native hooks and their `NativeKind` classifications.
- `--dump-missing-natives-grouped`: Groups unbridged `ACC_NATIVE` methods by JDK module to isolate missing runtime services.

---

## Diagnosing Violations

When an application encounters a compatibility boundary under JDK-only mode, a structured diagnostic is emitted:

```text
CratonVM JDK-only violation: compatibility implementation required

Requested class:
  java/util/function/Function$Identity

Requested from:
  java/util/function/Function.identity()Ljava/util/function/Function;

Reason:
  No class bytes were found, and the compatible runtime would create a
  synthetic stand-in.

JDK:
  feature version: 25
  java.home: <redacted>
  module: java.base

Remediation:
  This indicates an incomplete lambda/metafactory runtime service.
  Re-run with --compatible to use compatibility mode, or collect:
    --jdk-only-report jdk-only-report.json
```

### Violation Kinds

| Violation Kind (`JdkOnlyViolation::kind()`) | Description | Remediation |
|---|---|---|
| `compatibility-class-requested` | The runtime attempted to fabricate a class because bytecode was missing. | If a JDK internal class, a runtime service requires completion. If an application class, verify the classpath. |
| `synthetic-native-registered` | A `SyntheticStub` registration was rejected during subsystem startup. | The native method should either be promoted to a `Bridge` / `Intrinsic` or eliminated in favor of JDK bytecode. |
| `synthetic-native-invocation` | A `SyntheticStub` was dispatched at runtime. | Indicates an unresolved dispatch path bypassing initial registration filters. |
| `missing-native` | An `ACC_NATIVE` method has no registered native bridge. | Inspect `--dump-missing-natives-grouped` to identify missing VM/OS services. |
| `native-shadows-bytecode` | A registered native would inappropriately supersede authentic bytecode. | Ensure bytecode is given precedence; review native promotion criteria. |
| `missing-boot-class` | An expected boot class could not be resolved in the JDK image. | Check `--java-home` configuration and JDK installation integrity. |
| `missing-implementation` | Neither bytecode nor native implementation exists for a dispatched method. | Method is abstract or unavailable in the target JDK version. |

---

## Triage Matrix

When diagnosing behavioral differences or failures, perform a three-way comparative check:

```bash
# 1. Foundational mode (default)
cratonvm --jdk-only --java-home "$JAVA_HOME" -cp app.jar com.example.Main

# 2. Compatible optimization mode
cratonvm --compatible --java-home "$JAVA_HOME" -cp app.jar com.example.Main

# 3. Reference OpenJDK HotSpot
"$JAVA_HOME/bin/java" -cp app.jar com.example.Main
```

### Analysis of Results

| Foundational (`--jdk-only`) | Compatible (`--compatible`) | HotSpot Reference | Interpretation |
|---|---|---|---|
| **Fails** | **Passes** | **Passes** | A compatibility optimization or stub is load-bearing. This represents a missing runtime service or unpromoted bridge in JDK-only mode. |
| **Fails** | **Fails** | **Passes** | General VM defect or JIT/interpreter discrepancy independent of compatibility mode. |
| **Fails** | **Fails** | **Fails** | Application defect, malformed classpath, or invalid program arguments. |
| **Passes** | **Passes** | **Passes** | Clean execution across all targets. |
| **Passes** | **Fails** | **Passes** | Optimization layer defect in compatible mode. |

---

## Operating Guidelines

1. **Continuous Integration**: Strict CI suites must execute under foundational mode (`--jdk-only`) and assert that the mode was enforced. Strict jobs must never fall back silently to `--compatible`.
2. **Environment Filtering**: Low-level environment filters such as `CRATONVM_NO_STUBS` (or `CRATONVM_REAL=-stubs`) filter native registries but do not enforce class fabrication checks or bytecode authority. Tooling and production scripts should use `--jdk-only` and `--compatible`.
3. **Targeting Parity**: Every change affecting `--jdk-only` or `--compatible` must preserve HotSpot behavioral fidelity.
