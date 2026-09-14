# Compatibility and Support Policy

CratonVM implements the Java Virtual Machine and a broad JDK runtime surface,
but compatibility is multidimensional. A statement such as "supports Java 21"
does not imply certification, complete library coverage, every host platform,
or production support for every optional subsystem.

## Compatibility dimensions

Evaluate a workload across:

1. **Class-file/language level** — can the reader, verifier, interpreter, and
   JIT execute the emitted bytecode?
2. **JDK library surface** — are the required classes, methods, and native entry
   points present?
3. **JDK mode** — real-JDK and synthetic-JDK modes have different library
   ownership and coverage.
4. **Host platform** — OS, architecture, filesystem, networking, and desktop
   integrations vary.
5. **Execution engine** — interpreter behavior is the semantic baseline; JIT,
   OSR, moving GC, G1, and GPU add independent coverage dimensions.
6. **Interop** — JNI, agents, embedding, and foreign-function support each have
   their own maturity.
7. **Security/compliance** — algorithm availability is not the same as
   constant-time implementation, audit, or certification.

## Status vocabulary

Project documentation uses these meanings:

| Status | Meaning |
|--------|---------|
| Supported | Covered by normal tests and intended for routine use within the documented limits. |
| Experimental | Implemented and usable for evaluation, but interfaces, performance, or coverage may change and the default may remain off. |
| Partial | A meaningful subset works; callers must review named gaps. |
| Best-effort | Maintained when practical but not a primary CI/release target. |
| Unsupported | Not implemented or intentionally out of scope. |
| Stub | Shape/metadata exists but is not a production implementation. |

Avoid "full" unless an exhaustive, reproducible conformance source supports it.

## Java versions

The class reader understands class-file versions through the documented Java
release range, and modern language features are exercised in the suite.
Library behavior depends on the selected JDK and native coverage. See [Java
Version Support](../java-support/version-support.md), [Language
Features](../java-support/language-features.md), and [Standard Library
Coverage](../java-support/standard-library.md).

CratonVM is not JCK-certified. The repository's JCK document is an engineering
coverage estimate, not a claim of passing the licensed Technology
Compatibility Kit.

## Real-JDK and synthetic-JDK modes

Real-JDK mode is the application default:

- real JDK classes provide Java-visible library semantics;
- CratonVM implements genuine VM/native boundaries; and
- loader/module behavior follows the selected JDK artifacts.

Synthetic-JDK mode exists for standalone operation, bootstrapping, focused
tests, and differential diagnosis. It may implement behavior directly in Rust.
A synthetic-mode success does not prove real-JDK compatibility, and a
synthetic-mode failure does not necessarily block a real-JDK application.

The project rule is to prefer real class bytecode for application-visible JDK
types rather than hiding missing behavior behind synthetic stubs.

## Semantic contract

For deterministic Java code, the primary compatibility oracle is observable
behavior:

- return values and output;
- exception types and control flow;
- object/field/array semantics;
- monitor and thread behavior;
- class-loader identity;
- reflection/native results; and
- persistence or protocol bytes where relevant.

The regression and differential suites compare CratonVM with a pinned HotSpot
JDK. A mismatch is either a CratonVM defect, a documented intentional
divergence, or a non-portable test assumption; it is never accepted solely
because one side is faster.

Both JIT and `--nojit` should be tested for a new semantic failure. Compilation
is allowed to decline an unsupported shape and continue in the interpreter; it
is not allowed to produce different Java behavior.

## Platform support

x86-64 is the primary JIT target. Other architectures can use the interpreter;
the AArch64 JIT backend remains partial. Linux and Windows are the primary
development targets, while macOS is best-effort for platform-specific paths.
See the [Platform Support Matrix](platform-support.md).

Headless AWT/Java2D support does not imply an on-screen desktop backend. JavaFX
is out of tree.

## Optional subsystem policy

- The default collector and interpreter are correctness baselines.
- G1, moving-young behavior, AArch64 JIT, GPU offload, and some agent/foreign
  paths are experimental or partial as documented.
- An optional subsystem should fail closed to a supported path when its
  correctness precondition is not proven.
- A feature flag being present does not make that feature production-supported.

## Security and cryptography

CratonVM has not completed a formal security audit and must not be treated as a
security boundary for hostile bytecode. Use OS/container isolation.

Cryptographic algorithm availability does not imply constant-time behavior,
FIPS validation, or protocol-stack completeness. Review the per-algorithm
matrix and TLS limitations before using Java security APIs.

## Version and API stability

The command line aims for practical compatibility, but experimental flags can
change. The C embedding ABI and Rust embedding facade document their own
stability boundaries. Internal Rust crates, generated-code layouts, debug
environment variables, and feature-design documents are not stable public APIs
unless explicitly declared.

Pin the executable, JDK, and configuration for deployments. Read the changelog
and rerun the application's differential smoke before upgrading.

## Reporting a compatibility gap

Include:

- minimal source and compiled class if possible;
- exact CratonVM/JDK versions and host;
- exact arguments and relevant configuration;
- JIT, `--nojit`, and HotSpot outputs;
- real-JDK versus synthetic-JDK result if relevant;
- missing-native audit output if relevant; and
- whether the behavior is deterministic.

Open unresolved bug documents under `docs/known-issues`. When fixed and covered
by a regression, move the document under `docs/internal` so the open-issue view
does not retain resolved work.
