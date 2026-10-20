# Runtime Lifecycle

This chapter follows one CratonVM process from launcher entry to shutdown. It
connects the subsystem chapters and identifies the phase boundaries where
invariants become true.

## 1. Launcher and immutable configuration

`cratonvm-cli` expands argument files, extracts Java system properties, parses
launcher/HotSpot-compatible options, selects the entry point, and resolves the
JDK mode. Declared VM flags are installed into an immutable typed snapshot
before runtime subsystems initialize.

```text
argv + selected environment
        │
        ▼
CLI parse and normalization
        │
        ▼
typed VM configuration snapshot
```

Environment variables are a launcher input format, not the preferred
cross-subsystem runtime API. Code that needs a declared VM flag should read the
typed flag surface; ordinary application/OS environment variables remain live
for Java semantics.

## 2. JDK and boot source selection

In real-JDK mode, the launcher resolves a JDK from explicit `--java-home`,
CratonVM/JAVA_HOME configuration, or the host Java installation. The loader
uses the JDK modules and application classpath.

In synthetic mode, Rust-provided standard-library compatibility classes and
natives form the boot surface. The modes share the VM engine but do not promise
identical library coverage.

## 3. Typed bootstrap phases

VM initialization is encoded as a typestate transition:

```text
BootstrapPhase<Allocated>
        │ validate class universe and java/lang/Object
        ▼
BootstrapPhase<ClassesReady>
        │ validate native registration
        ▼
BootstrapPhase<NativesReady>
        │ validate runtime hooks
        ▼
BootstrapPhase<RuntimeReady>
        │ finish()
        ▼
running VM
```

Only the matching state can invoke the next transition, and only
`BootstrapPhase<RuntimeReady>` can finish bootstrap. This turns ordering from a
comment into a compile-time API and makes each boundary independently testable.

The phases mean:

| Phase | Guaranteed state |
|-------|------------------|
| `Allocated` | Core VM objects and owners exist, but consumers must not assume classes/natives/runtime hooks are ready. |
| `ClassesReady` | Required boot classes are present and the class universe invariants hold. |
| `NativesReady` | Native registration is populated and callable by the later runtime. |
| `RuntimeReady` | Runtime callbacks/hooks and final execution dependencies are wired. |

## 4. Main-class resolution

For class mode, the launcher supplies the named class. For JAR mode, it reads
`Main-Class` and manifest class-path entries. The class manager then:

1. locates class bytes in the correct loader namespace;
2. parses the class file;
3. verifies bytecode and metadata;
4. links symbolic references;
5. prepares fields and methods; and
6. initializes the class before first active use.

Class identity is `(defining loader, binary name)`, not the name alone.
Loader-aware operations must preserve the declaring class/loader through
native, reflection, and invoke caches.

## 5. Invocation and interpretation

The VM creates the Java `String[]` arguments, resolves `main`, installs the
initial frame, and enters the interpreter. The interpreter owns Java control
flow and exception-table routing when a frame is not compiled.

Method and loop counters feed the tier manager. A hot method can compile for
its next invocation; a hot loop can use OSR to enter compiled code without
waiting for another method call.

## 6. Compilation

There are two x86-64 compilation front ends:

- a baseline bytecode-to-machine-code emitter; and
- an optimizing sea-of-nodes IR pipeline.

They share `jit::runtime_lowering` for runtime-sensitive semantics, including:

- object allocation;
- virtual/interface hashed dispatch tails; and
- live monitor enter/exit calls.

The tiers may differ in graph construction, scalar replacement, scheduling,
and optimization budget. They must not invent different runtime ABIs for the
same operation.

Unsupported or unproven shapes fail closed to another tier or the interpreter.
For example, precise exceptional-frame handoff is admitted only for protected
shapes whose throwing sites can publish the required locals and operand state.

## 7. Calls from compiled code

Compiled code handles arithmetic and supported memory operations directly. A
runtime bridge covers slow or stateful operations:

```text
compiled method
  ├─ direct compiled call
  ├─ monomorphic/PIC/hashed virtual or interface dispatch
  ├─ allocation fast path → refill/initialization slow path
  ├─ thin monitor path → contention slow path
  ├─ native capability call
  └─ exception/deoptimization handoff → interpreter
```

Compiled code publishes enough frame information before safepoint-capable
calls for GC and deoptimization to reconstruct live Java state.

## 8. Allocation and collection

The ordinary allocation fast path is a TLAB bump followed by header
initialization and reference publication. A refill or exceptional condition
enters the common VM helper.

When collection is requested, threads reach safepoints, interpreter and JIT
roots are scanned, registered external/native roots participate, and the
selected collector traces live objects. Moving collection remaps every
registered owner; if the VM cannot prove complete moving coverage, it diverts
to the non-moving path.

## 9. Native execution

The registry hashes the class/method/descriptor key and resolves the native
implementation. Native code receives a capability facade rather than importing
the VM implementation. Common argument decoding, forwarding, and pin tracking
use an eight-slot inline buffer; larger signatures spill safely.

Crypto and security implementation kernels are separately compiled:

- `cratonvm-native-builtins-crypto`; and
- `cratonvm-native-builtins-security`.

`cratonvm-native-builtins` owns registration and Java-object marshalling around
those packs.

## 10. Exceptions, deoptimization, and return

Java exceptions remain Java control flow. Compiled helpers publish an
out-of-band signal and return the JIT sentinel; the caller distinguishes a real
signal from a legitimate wide value, reconstructs the frame, and routes the
exception through the Java exception table.

Deoptimization similarly restores a Java-visible frame from metadata and
continues in the interpreter. Unsupported reconstruction is a compile/admission
failure, not permission to fabricate locals.

Normal completion returns from `main`, runs the application's normal shutdown
behavior, emits requested final diagnostics, and releases VM-owned resources.
An explicit process exit follows its controlled shutdown path; fatal OS signals
or OOM kills cannot guarantee Java shutdown hooks.
