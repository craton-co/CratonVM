# Glossary

Terms used throughout this manual and CratonVM's source.

**AOT (Ahead-of-Time) compilation**
Compiling/caching class data or code before run time. CratonVM exposes
HotSpot-style `--XX:AOTMode` / `--XX:AOTCache` flags.

**AVX2**
An x86-64 SIMD instruction-set extension the JIT uses to vectorize data-parallel
reduction loops (when the CPU supports it).

**BCE (Bounds-Check Elimination)**
A JIT optimization that removes redundant array-bounds checks, including a
speculative loop-header guard that hoists the check out of a provably safe loop.

**Bytecode**
The portable instruction format a Java compiler emits into `.class` files.
CratonVM executes bytecode (it does not compile Java source).

**Card table**
A data structure that records which regions of the old generation contain
references into the young generation, so a young collection can scan only "dirty"
cards instead of the whole old generation.

**CDS (Class Data Sharing)**
A mechanism for sharing pre-parsed class metadata across runs, exposed via
`--Xshare` / `--XX:SharedArchiveFile`.

**Cheney collector**
The copying garbage-collection algorithm used for the young generation: live
objects are evacuated from one space to another, compacting them.

**cgroup**
The Linux kernel mechanism container runtimes use to impose memory and CPU
limits. CratonVM detects these (see [Containers](../user-guide/containers.md)).

**Generational GC**
The default collector, which segregates the heap into a frequently-collected
young generation and a less-frequently-collected old generation, promoting
survivors.

**G1**
A region-based garbage collector, opt-in via `-XX:+UseG1GC` and experimental.

**Intrinsic**
A fast, special-cased implementation of a hot, well-known method, installed as an
inline-cache shortcut by the interpreter.

**invokedynamic**
A bytecode that resolves its call target via a bootstrap method on first
execution; the basis for lambdas, method references, and modern string
concatenation.

**JCA (Java Cryptography Architecture)**
The provider-based API (`java.security.*` / `javax.crypto.*`) through which
CratonVM advertises its cryptographic algorithms. See
[Cryptography](../security/cryptography.md).

**JFR (Java Flight Recorder)**
An event-based, low-overhead profiling/diagnostics facility, implemented by
CratonVM.

**JIT (Just-In-Time) compiler**
The component that compiles hot bytecode to native machine code at run time.
CratonVM's JIT targets x86-64. See [The JIT Compiler](../internals/jit.md).

**JMOD**
The packaging format for JDK modules. CratonVM loads real standard-library
classes from `java.base.jmod` in real-JDK mode.

**JNI (Java Native Interface)**
The standard C interface for native code to interact with the JVM. CratonVM
implements the JNI Invocation API and a substantial function table — see
[Embedding](../embedding/overview.md).

**JPMS (Java Platform Module System)**
The module system (JEP 261), driven by `--module-path` and the `--add-*` flags.
See [Modules](../user-guide/modules.md).

**LICM (Loop-Invariant Code Motion)**
A JIT optimization that hoists computations whose result doesn't change across
loop iterations out of the loop body.

**Native method**
A method implemented outside Java bytecode. In CratonVM these are Rust functions
in the `native-*` crates. See [Native Methods](../internals/native-methods.md).

**OSR (On-Stack Replacement)**
Compiling a hot loop and transferring execution from the interpreter into the
compiled code mid-method, without waiting for the method to be re-entered.

**Panama / `java.lang.foreign`**
The foreign-function and memory API, gated by `--enable-native-access`.

**Precise JIT stack maps**
Metadata recording which registers/spill slots hold live object references at
each safepoint, so the collector can find roots in compiled frames accurately.
On by default.

**PTX**
NVIDIA's parallel-thread-execution assembly. CratonVM's GPU offload lowers
eligible Java methods to PTX. See [GPU Offload](../gpu/overview.md).

**Safepoint**
A point at which a thread's state is consistent enough for the collector to scan
it. Stop-the-world GC brings all threads to a safepoint first.

**SoA (Structure of Arrays)**
The operand-stack/locals layout that stores value payloads and type tags in
separate arrays, for cache efficiency and tag-based GC scanning.

**Synthetic mode / synthetic JDK**
The mode in which the standard-library classes are CratonVM's own Rust
implementations rather than real JDK bytecode — what lets the VM run with no JDK
installed. See [JDK Modes](../getting-started/jdk-modes.md).

**Verification**
The bytecode safety check performed before a class runs (JVMS 4.8–4.10). On by
default for non-boot classes.

**Write barrier**
A small action performed on a reference store so the collector can track
cross-generational references (works with the card table).
