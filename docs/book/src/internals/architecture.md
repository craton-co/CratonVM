# Architecture Overview

This part of the manual is for people working on CratonVM itself, or who want to
understand how it executes Java. It describes the codebase structure and how the
major subsystems fit together. The following chapters drill into each subsystem.

## Crate layout

CratonVM is a Cargo workspace of **20 member crates**. (The `fuzz/` directory is
a separate, standalone workspace — a nightly-only libFuzzer harness — and is
*not* a workspace member, because its `#![no_main]` harness trips the production
lints.)

| Crate | Directory | Purpose |
|-------|-----------|---------|
| `cratonvm-reader` | `reader/` | `.class` file parser |
| `cratonvm-types` | `types/` | Shared types (`Value`, `ClassId`, `ObjectRef`) |
| `cratonvm-native-api` | `native-api/` | `NativeContext` trait & FD table |
| `cratonvm-native-builtins` | `native-builtins/` | `java.lang.*` (and crypto, reflection, …) natives |
| `cratonvm-native-collections` | `native-collections/` | `java.util.*` natives |
| `cratonvm-native-io` | `native-io/` | `java.io` / `java.nio` natives |
| `cratonvm-native-awt` | `native-awt/` | AWT/Swing/Java2D native peers (headless) |
| `cratonvm-jit-api` | `jit-api/` | JIT compiler API/IR types |
| `cratonvm-jit` | `jit/` | x86-64 / AArch64 JIT compiler |
| `cratonvm-jit-cuda` | `jit-cuda/` | Java bytecode → PTX lowering for GPU offload |
| `cuda-bridge` | `cuda-bridge/` | Thin CUDA Driver API bridge for GPU offload |
| `craton-gpu` | `craton-gpu/` | Build-time Java annotation sources for GPU offload |
| `cratonvm-classloading` | `classloading/` | Class loading, linking & bytecode verification |
| `cratonvm-gc` | `gc/` | Garbage collectors & memory management |
| `cratonvm-jfr` | `jfr/` | Java Flight Recorder |
| `cratonvm-vm` | `vm/` | VM runtime engine (interpreter, threading, bootstrap) |
| `cratonvm-cli` | `vm-cli/` | Command-line entry point (the `cratonvm` binary) |
| `libcratonvm` | `libcratonvm/` | C-ABI shared library for embedding (JNI Invocation API) |
| `cratonvm-embed` | `cratonvm-embed/` | Curated, semver-stable Rust embedding facade |
| `cratonvm-difftest` | `difftest/` | Differential-testing harness against a reference JDK |

It is roughly **880,000 lines of Rust**. The project builds on Rust **1.80+**
(edition 2021).

## Dependency flow

```text
vm-cli → vm → {classloading, gc, jit, native-builtins, native-collections,
               native-io, native-awt, jfr}
              → {reader, types, native-api, jit-api, jit-cuda,
                 cuda-bridge, craton-gpu}

libcratonvm   → vm   (C-ABI / JNI Invocation API embedding shim)
cratonvm-embed → vm  (curated, semver-stable Rust embedding facade)
```

## Data flow: from class bytes to native code

```text
.class bytes
    │
    ▼
  reader::read_class()        parse into a ClassFile
    │
    ▼
  ClassManager::load_class()  link, verify, prepare, initialize
    │
    ▼
  interpreter::execute()      run bytecode
    │   ▲
    │   │ (hot-method threshold)
    ▼   │
  jit::compile()              emit x86-64, cache the compiled method
    │
    ▼
  compiled code               calls back into the VM for slow paths / natives
```

## Subsystem map

| Subsystem | Crate(s) | Chapter |
|-----------|----------|---------|
| Bytecode execution | `vm` (`runtime/`) | [The Interpreter](interpreter.md) |
| JIT compilation | `jit`, `jit-api` | [The JIT Compiler](jit.md) |
| Memory & GC | `gc` | [The Garbage Collector](garbage-collector.md) |
| Loading, linking, verification | `classloading` | [Class Loading & Verification](class-loading.md) |
| Threads, monitors, safepoints | `vm` (`threading/`) | [Threading & Concurrency](threading.md) |
| Standard-library natives | `native-*` | [Native Methods](native-methods.md) |

## Key design decisions

1. **Run real Java where possible.** The preferred path loads real `java.base`
   bytecode from a detected JDK; synthetic Rust stubs are the standalone
   fallback. For application-visible types, real `.class` files are preferred
   over synthetic stubs.
2. **Two-layer exception model.** One error variant wraps Java-catchable
   exceptions (handled by the interpreter at catch/finally blocks); a separate
   variant wraps internal VM errors. Keeping them distinct prevents VM bugs from
   masquerading as catchable Java exceptions.
3. **Structure-of-Arrays value layout.** Frames and operand stacks store value
   payloads and type tags in separate arrays, for better cache behavior and
   tag-based GC scanning.
4. **Direct bytecode-to-x86-64, with an optional IR.** The JIT's default path
   compiles bytecode directly to machine code in a single pass; methods that
   qualify are routed through an optional sea-of-nodes IR for broader
   optimization before instruction selection.
5. **Hashed native dispatch.** Native methods are looked up by hashing
   `"class.method:descriptor"`, giving O(1) dispatch with collision detection at
   registration time.

## In-source canonical docs

A few cross-cutting concerns are documented directly in source, where they are
authoritative — notably the global **lock acquisition order** (the `LockLevel`
hierarchy and its runtime-enforcement wrappers) in the VM runtime. When working
on the internals, treat the in-source definition as the source of truth.
