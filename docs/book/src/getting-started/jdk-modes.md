# JDK Modes: Architecture and Execution Policies

CratonVM offers a flexible execution architecture designed around authentic JDK execution, performance acceleration, and hermetic standalone capability.

## Overview of Execution Modes

| Mode | Invocation | Description |
|------|------------|-------------|
| **Real-JDK Mode** | Default launcher mode (or `--real-jdk`) | Foundational primary target and default: runs authentic OpenJDK class libraries directly, treating real JDK bytecode as authoritative with zero synthetic stand-ins. |
| **Fast-Boot / Compatible Mode** | `--fast-boot` (alias: `--compatible`) | Optimization layer engineered for fast cold-start loading and specialized high-throughput execution paths using native Rust stand-ins and optimized bridges. |
| **Synthetic Mode** | `--synthetic-jdk` (embedding default: `VmConfig::default()`) | Hermetic, fully standalone mode executing CratonVM's built-in Rust standard-library implementations with no external JDK dependency. |

### Real-JDK Mode (The Foundational Primary Target)

Real-JDK mode (`--real-jdk`) is the foundational core architecture and primary target of CratonVM: running authentic OpenJDK class libraries directly, treating real JDK bytecode as authoritative. The VM loads genuine OpenJDK class files, interprets and JIT-compiles their authentic bytecode, and supplies native method implementations through its Rust native registry (`Bridge` and reviewed `Intrinsic` categories).

In this mode:
- Concrete Java bytecode from the JDK runtime image is authoritative.
- No synthetic stand-in classes or stubs are fabricated.
- Unimplemented native methods raise structured link errors (`MissingNative`) rather than executing dummy stubs.
- Maximum specification compliance and HotSpot fidelity are preserved.

### Fast-Boot / Compatible Mode (The Optimization Layer)

Fast-boot mode (`--fast-boot`, with legacy alias `--compatible`) and synthetic stubs were engineered as an optimization layer for fast cold-start loading and specialized high-throughput execution paths. Synthetic stubs exist to accelerate cold start and bypass complex initialization paths in compatible mode, while foundational execution runs real bytecode. While continuing to execute against the host JDK's module path, it selectively activates high-performance native Rust stand-ins and bypasses complex initialization sequences.

This mode provides:
- Ultra-fast cold start by short-circuiting heavy JVM bootstrap paths.
- Native Rust fast paths for selected core library components and high-throughput execution paths.
- Permissive compatibility fallbacks for non-standard class library edge cases.

### Synthetic Mode (Standalone Hermetic Execution)

Synthetic mode (`--synthetic-jdk`) enables CratonVM to run Java applications without requiring any JDK, `JAVA_HOME`, or `rt.jar` on the host system. The standard library is implemented natively in Rust. This mode is the default for embedded use cases (`VmConfig::default()`) to ensure embedded applications remain strictly self-contained.

## How the Mode is Chosen

The mode is determined by explicit command-line flags or the launcher default:

- The `cratonvm` launcher defaults to **Real-JDK mode** (`--real-jdk`).
- Passing `--fast-boot` (or `--compatible`) enables the acceleration layer.
- Passing `--synthetic-jdk` forces standalone synthetic execution.
- Embedding entry points (`VmConfig::default()`, `libcratonvm`, `cratonvm-embed`) default to compatible/synthetic mode for hermetic embedding safety.

When running in real JDK modes (Real-JDK or Compatible/Fast-Boot), the launcher discovers the host JDK in the following order:

1. The `--java-home` command-line flag.
2. The `CRATONVM_JAVA_HOME` environment variable.
3. The `JAVA_HOME` environment variable.
4. A `java` executable located on `PATH`.

The selected installation must contain `jmods/java.base.jmod` or a modular runtime image (`lib/modules`). If no valid JDK image is detected, CratonVM reports an informative error detailing every searched path.

## Choosing and Forcing Modes

**Real-JDK Mode (Default):**
```bash
cratonvm --classpath . HelloWorld
# or explicitly:
cratonvm --real-jdk --classpath . HelloWorld
```

**Acceleration / Fast-Boot Mode:**
```bash
cratonvm --fast-boot --classpath . HelloWorld
# or using the compatible alias:
cratonvm --compatible --classpath . HelloWorld
```

**Hermetic Synthetic Mode:**
```bash
cratonvm --synthetic-jdk --classpath . HelloWorld
```

**Explicit Host JDK Target:**
```bash
cratonvm --java-home /path/to/jdk-25 --classpath . HelloWorld
```

**Environment Override:**
```bash
CRATONVM_JAVA_HOME=/path/to/jdk-25 cratonvm --classpath . HelloWorld
```

## Summary Recommendation

- Use **Real-JDK mode** (the default, `--real-jdk`) for strict HotSpot equivalence, conformance testing, and executing authentic JDK class libraries.
- Use **fast-boot mode** (`--fast-boot` / `--compatible`) when prioritizing rapid cold start, high-throughput accelerated native paths, or working around missing native implementations.
- Use **synthetic mode** (`--synthetic-jdk`) when shipping zero-dependency, hermetic self-contained binaries without a host JDK.

For deeper technical details on enforcement and diagnostics in strict execution, see [Real-JDK Mode](../user-guide/real-jdk-mode.md).
