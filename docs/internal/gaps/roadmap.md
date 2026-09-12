# CratonVM — Future Roadmap

## Current State

CratonVM runs Java SE 8-25 bytecode on a custom x86-64 JIT with 26 optimization
rounds. Back-edge OSR flipped default-on 2026-07-04, closing most of the gap
seen in the March 2026 Round 26 snapshot. The 2026-07-08 `bfc26c2d` QuickBench
measurement is 3.7x slower than HotSpot C2 by default (OSR included). The
same-day Binary Trees recheck is 23.7x slower by default; Binary Trees
remains dominated by allocation/GC throughput. All Java LTS versions through
Java 25 are supported:

- **Java 11:** Nest-based access control
- **Java 17:** Records, sealed classes
- **Java 21:** Pattern matching for switch, virtual threads (simplified), sequenced collections
- **Java 25:** Stream gatherers, scoped values, structured concurrency, class file version 69
- **Panama FFI:** MemorySegment, Arena, ValueLayout, SymbolLookup, Linker (8-arg downcalls), upcall handles, struct/union layouts, string marshaling

### Current Benchmark (vs HotSpot JDK 25 C2)

| Benchmark               | JDK 25 C2    | CratonVM default | Default ratio |
|-------------------------|--------------|------------------|---------------|
| Arithmetic 300M         | 343 ms       | 692 ms           | 2.0x          |
| Fibonacci(42)           | 603 ms       | 2,944 ms         | 4.9x          |
| Sieve 100Kx500          | 70 ms        | 349 ms           | 5.0x          |
| Matrix 500x500          | 161 ms       | 370 ms           | 2.3x          |
| **QuickBench TOTAL**    | **1,177 ms** | **4,355 ms**     | **3.7x**      |
| Binary Trees (depth=18) | 347 ms       | 8,214 ms         | 23.7x         |

*Best-of-N (N=10 JDK25, N=7 CratonVM) measured 2026-07-08 on a shared Azure Linux build host, JDK 25.0.3 Temurin vs a CratonVM release build off `dev` at `bfc26c2d`. Best-of-N rather than single-run because run-to-run variance on this shared host was 2-4x (one CratonVM outlier of 50,516 ms excluded). The benchmark sources are the historical `bench/QuickBench.java` and `bench/binarytrees.java` from commit `2cea208`; `bench/` is currently untracked.*

Historical Round 26 added: loop unrolling, speculative bounds check elimination, OSR with JIT cache fast-path, graph-coloring register allocation.

### Known Limitations — Real-World App Blockers

| Limitation | Impact | Fix Phase |
|-----------|--------|-----------|
| **No `java.util.stream` collect()** | Streams-heavy code stack-overflows | Phase 16.1 (Iterative Interpreter) |
| **JIT crashes on FP math** | Any double-precision computation segfaults | Phase 16.2 (JIT FP Codegen) |
| **JIT miscompilation on complex control flow** | Wrong results on permutation/branch-heavy code | Phase 16.3 (JIT Control Flow) |
| **GC/allocation throughput gap on allocation-heavy workloads** | Binary Trees, real-world object creation (23.7x default in 2026-07-08 recheck) | Phase 16.4 (GC Fast Path) |
| **No reflection-based frameworks** | Spring, Hibernate, Jackson unusable | Phase 3 + Phase 4 |
| **No networking/sockets** | Server apps, HTTP clients unusable | Phase 9 |
| **No JDBC** | Database apps unusable | Phase 4 |
| **No classloader hierarchy for JARs** | Multi-JAR apps unusable | Phase 2 + Phase N |

---

## Next Steps (Priority Order)

### Phase 16: Interpreter & JIT Hardening (P0 — Real-World Blocker)

These are the **immediate blockers** preventing CratonVM from running any non-trivial Java application.

#### 16.1: Iterative Interpreter (Eliminate Stack Overflow on Streams)
- Replace recursive `execute_method()` call chain with an explicit `CallFrame` stack
- Unlocks Streams, Collectors, Optional chains, CompletableFuture pipelines
- **Effort:** XL (2-3 months)

#### 16.2: JIT Floating-Point Codegen ✅ DONE
- Math.sqrt wired as SQRTSD intrinsic in early-compile path
- `has_dispatch` flag: dispatch-free JIT methods skip catch_unwind overhead
- ldc/ldc_w support added to JIT (int/float constants)
- String-ldc guard prevents crash in early-compile and OSR paths
- Debug eprints removed from hot paths
- **Result:** N-Body 9.3s → 4.2s (2.2x faster), gap reduced from 20x to ~9x HotSpot
- **Remaining:** Interpreter→JIT call overhead limits further gains; needs OSR or method inlining

#### 16.3: JIT Complex Control Flow Fix
- Fix branch target calculation for nested loops, static field access in JIT code
- Unlocks complex algorithmic code (Fannkuch, parsers, state machines)
- **Effort:** L (2-4 weeks)

#### 16.4: GC Allocation Fast Path
- Bump-pointer allocation, TLABs, reduced STW frequency
- Target: 5-10x improvement on allocation-heavy workloads (Binary Trees)
- **Effort:** L (2-4 weeks)

---

### ✅ Phase J: Beyond 1.0x — Beat C2 Consistently

The March 2026 Round 26 snapshot briefly showed near-C2 loop kernels; back-edge OSR flipping default-on (2026-07-04) has since closed most of that gap again. In the 2026-07-08 snapshot the strongest loop kernels are Arithmetic (2.0x), Sieve (5.0x), and Matrix (2.3x) by default; Fibonacci (4.9x) and Binary Trees (23.7x) remain the highest-priority performance gaps.

#### J1: Method Inlining (Target: -30% total time)
- **What:** Inline small methods (< 35 bytecodes) at JIT call sites
- **How:**
  1. During `try_compile`, scan invoke targets for inlining candidates
  2. For each small callee: copy its bytecode into the caller's compilation unit
  3. Remap callee locals to caller's spill area (offset by caller's max_locals)
  4. Replace invoke+return with direct bytecode sequence
  5. Enable cross-method optimizations (constant prop across call boundary)
- **Deoptimization:** Invalidate compiled code when a class is loaded that could change a call target. Use a generation counter — compiled methods store the generation at compile time; on class load, bump generation and discard stale entries.
- **Impact:** Eliminates ~15-20% overhead from call/return in recursive code (Fibonacci) and method-heavy code (Matrix helper calls)
- **Effort:** L (2-4 weeks)

#### J2: Escape Analysis + Scalar Replacement (Target: -25% on Matrix)
- **What:** Detect objects that don't escape the method → eliminate allocation entirely
- **How:**
  1. Build a simple escape graph during JIT compilation: track `new` → field stores → returns/escapes
  2. If an object never escapes (not stored to heap, not returned, not passed to non-inlined call): mark as non-escaping
  3. For non-escaping objects: replace field accesses with local variables (scalar replacement)
  4. Eliminate the `new` + `<init>` entirely — fields become extra locals
- **Key target:** Matrix benchmark creates `int[n][n]` result array — the inner row arrays are short-lived and could be stack-allocated
- **Prerequisite:** Method inlining (J1) — escape analysis is most effective after inlining exposes the full data flow
- **Effort:** L (2-4 weeks)

#### J3: Graph-Coloring Register Allocator (Target: -15% across all)
- **What:** Replace fixed local→register mapping with interference-graph-based coloring
- **How:**
  1. Build live intervals for all values (locals + stack temporaries) via bytecode dataflow
  2. Build interference graph (two values interfere if their live ranges overlap)
  3. Color with K = 14 GPR + 16 XMM = 30 available registers
  4. Spill least-used values when coloring fails
  5. Replace current `LOCAL_REGS[0..7]` mapping with optimal assignment
- **Current state:** 5-7 locals mapped to callee-saved regs; rest spill to stack. Complex methods with many live values suffer.
- **Effort:** M (1-2 weeks)

#### J4: JIT Inline Caching (Monomorphic Guards in Compiled Code)
- **What:** Emit receiver type check directly in JIT code
- **How:**
  1. At invokevirtual in JIT: load receiver's ClassId from object header
  2. CMP with expected ClassId (recorded from first call)
  3. JE → direct CALL to target entry point (zero dispatch overhead)
  4. JNE → fall back to slow-path dispatch function
- **Current:** Interpreter-level InvokeCache already does this, but JIT-compiled code still calls `jit_invoke_dispatch` function pointer
- **Effort:** M (1-2 weeks)

#### J5: JIT Switch Compilation
- **What:** Support `tableswitch`/`lookupswitch` in JIT (currently rejected by scanner)
- **How:**
  1. Scanner: accept 0xaa/0xab, parse variable-length encoding
  2. Tableswitch: SUB to normalize, CMP bounds, jump table or CMP chain
  3. Lookupswitch: linear CMP chain (small), binary search (large)
- **Effort:** M (1-2 weeks)

#### J6: Advanced Loop Optimizations
- **What:** Loop unrolling, loop peeling, strength reduction
- **How:**
  1. **Loop unrolling:** Duplicate small loop bodies 2-4x, reducing branch overhead and enabling ILP
  2. **Loop peeling:** Execute first iteration separately to simplify guards in main loop
  3. **Strength reduction:** Replace `i * stride` with `sum += stride` in address calculations
  4. **Range check hoisting:** Single pre-loop bounds check, then run main loop without per-access checks
- **Impact:** Reduces branch misprediction, better instruction scheduling
- **Effort:** L (2-4 weeks)

#### J7: Instruction Selection Improvements
- **What:** Better x86-64 code lowering for common patterns
- **How:**
  1. **LEA for address arithmetic:** `LEA RAX, [RBX + RCX*4 + 16]` replaces MUL+ADD sequences
  2. **CMOV for simple branches:** Conditional moves avoid branch misprediction on small if/else
  3. **FMA3 instructions:** `VFMADD231SS` for float `a*b+c` in one instruction
  4. **Better constant materialization:** XOR reg,reg for zero; 32-bit MOV when upper bits are known zero
- **Effort:** M (1-2 weeks)

#### J8: Inline GC Write Barriers
- **What:** Emit card table writes directly in JIT code instead of calling Rust helpers
- **How:**
  1. After putfield of reference type: `SHR obj_addr, 9; MOV BYTE [card_table + offset], 0`
  2. Pass card table base pointer in a dedicated register or via VM context
  3. Eliminates helper call overhead on every reference store in inner loops
- **Effort:** S (1-3 days)

#### J9: Profile-Guided Optimization (PGO)
- **What:** Collect runtime profiles in the interpreter to guide JIT decisions
- **How:**
  1. **Branch profiling:** Count taken/not-taken per branch site → lay out hot path as fall-through
  2. **Receiver type profiling:** Record class of receiver at each call site → speculative devirtualization
  3. **Loop trip count profiling:** Guide unroll factor and SIMD strategy
  4. Extend existing invocation counter infrastructure with per-bytecode counters
- **Prerequisite:** J1 (inlining benefits most from profile data)
- **Effort:** L (2-4 weeks)

#### J10: Constant Propagation & Dead Code Elimination
- **What:** Fold constants through JIT compilation, eliminate unreachable code
- **How:**
  1. Track known-constant locals through bytecode dataflow
  2. Fold constant operands into instructions (e.g., `iconst_5; imul` → `IMUL RAX, 5` already done, extend to multi-step chains)
  3. Mark unreachable bytecode paths (after unconditional jumps, after throw) and skip emission
- **Effort:** M (1-2 weeks)

#### J11: Tail-Call Optimization
- **What:** Convert tail-recursive calls into loops
- **How:**
  1. Detect `invoke*` immediately followed by `*return` of same type
  2. For self-calls: replace with argument shuffle + jump to method entry
  3. Eliminates stack growth for recursive algorithms
- **Effort:** S (1-3 days)

#### J12: Sea-of-Nodes IR (Long-term)
- **What:** Intermediate SSA representation between bytecode and x86-64
- **How:**
  1. Build SSA graph from bytecode (phi nodes at merge points)
  2. Enable global value numbering, algebraic simplification, better scheduling
  3. Decouple optimization passes from instruction selection
  4. All current passes (LICM, BCE, SIMD) operate on IR instead of bytecode
- **Impact:** Unlocks optimization quality comparable to production compilers
- **Effort:** XL (2-3 months)

---

### ✅ Phase K: ARM64 JIT Backend

x86-64 only is a limitation. ARM64 (AArch64) covers Apple Silicon Macs and ARM servers.

#### K1: Instruction Emitter
- **What:** New `vm/src/jit/aarch64.rs` with ARM64 instruction encoding
- **Key instructions:** ADD/SUB/MUL, LDR/STR, CMP/B.cond, BL/RET, FMOV/FADD/FMUL
- **Register convention:** X0-X7 (args), X8 (indirect result), X9-X15 (temps), X19-X28 (callee-saved), X29 (FP), X30 (LR)
- **Reuse:** All bytecode analysis, JIT scan, optimization passes (LICM, BCE, SIMD detection) are target-independent
- **Effort:** XL (2-3 months)

#### K2: NEON Vectorization
- Map existing SIMD int-array-sum to ARM NEON instructions (LD1, ADD v-regs)
- 128-bit NEON = 4 ints per op (vs AVX2's 8)
- **Effort:** M (after K1)

#### K3: Platform Abstraction
- Extract `ExecutableBuffer` memory allocation into platform trait
- x86-64: VirtualAlloc/mmap (existing)
- ARM64: same syscalls but different page protections (W^X on macOS)
- **Effort:** S

---

### ✅ Phase L: Concurrent GC

Current stop-the-world generational GC pauses all threads during collection. For latency-sensitive workloads, concurrent marking reduces pause times.

#### L1: Concurrent Marking (Target: <10ms pauses)
- **What:** Mark phase runs concurrently with application threads
- **How:**
  1. Initial mark (STW, brief): mark roots directly reachable from thread stacks
  2. Concurrent mark: traverse object graph with tri-color marking while app runs
  3. Write barrier: SATB (snapshot-at-the-beginning) — when a reference field is overwritten, log the old value to a per-thread buffer
  4. Remark (STW, brief): process SATB buffers + re-mark roots
  5. Concurrent sweep: reclaim white (unmarked) objects
- **Data structures:**
  - `MarkBitmap`: one bit per object slot (parallel to heap)
  - `SatbBuffer`: per-thread Vec of overwritten references
  - `MarkQueue`: work-stealing queue for concurrent mark workers
- **Existing infrastructure:**
  - `GcBarrier` with STW coordination (gc_barrier.rs) — reuse for initial-mark and remark
  - Card table for old→young references — extend for SATB logging
  - Root snapshot collection from all threads — reuse for initial mark
- **Effort:** XL (2-3 months)

#### L2: Generational Concurrent (G1-style)
- Divide heap into regions (1-32 MB each)
- Collect young regions frequently (short STW)
- Collect old regions incrementally (concurrent mark + mixed GC)
- **Prerequisite:** L1
- **Effort:** XL

---

### ✅ Phase M: JNI Foundation

Java Native Interface — the standard mechanism for Java code to call C/C++ libraries. Required by virtually every real-world Java application (JDBC drivers, crypto, image processing, etc.).

#### M1: JNI Function Table (~230 functions)
- **Core struct:** `JNIEnv` — pointer to function table, one per thread
- **JavaVM:** Global VM handle for attaching/detaching threads
- **Key function families:**
  - `FindClass`, `GetMethodID`, `GetFieldID` — class/member resolution
  - `Call*Method` (CallVoidMethod, CallIntMethod, CallObjectMethod, etc.) — invoke Java from C
  - `Get*Field` / `Set*Field` — field access from C
  - `New*Array`, `Get*ArrayElements`, `Release*ArrayElements` — array access
  - `NewObject`, `NewGlobalRef`, `DeleteGlobalRef` — object lifecycle
  - `Throw`, `ExceptionCheck`, `ExceptionClear` — exception handling
  - `GetStringUTFChars`, `NewStringUTF` — string conversion
  - `MonitorEnter`, `MonitorExit` — synchronization
- **Can reuse:** existing `libloading` for library loading, `NativeContext` for method dispatch, heap for object allocation
- **Effort:** XL (2-3 months)

#### M2: System.loadLibrary Integration
- `System.loadLibrary("foo")` → search `java.library.path` for `libfoo.so` / `foo.dll`
- Load library, scan for `JNI_OnLoad` function, call it with JavaVM pointer
- Register native methods from loaded library
- **Effort:** M

#### M3: Local/Global Reference Management
- Local references: valid only within current JNI call, auto-freed on return
- Global references: prevent GC of referenced objects, explicit lifecycle
- Weak global references: allow GC but can be checked
- **Effort:** M

---

### ✅ Phase N: Full Module System

Java Platform Module System (JPMS, Java 9+). Required to load JDK 17+ standard library classes.

#### N1: Module Graph Resolution
- Parse `module-info.class` from each module JAR
- Build directed module dependency graph from `requires` directives
- Resolve transitive dependencies (`requires transitive`)
- Detect cycles and missing modules
- **Effort:** L

#### N2: Access Control Enforcement
- `exports package to module` — restrict package visibility to named modules
- `opens package to module` — allow deep reflection into package
- Enforce at field/method resolution time: check if accessing class's module can see target package
- **Effort:** L

#### N3: Module Layer API
- `ModuleLayer.boot()` — the boot layer containing platform modules
- `ModuleLayer.defineModulesWithOneLoader()` — create custom layers
- `Module.getDescriptor()`, `Module.getPackages()`, `Module.isExported()`
- **Effort:** M

#### N4: Boot Module Loading
- Load `java.base` module (contains java.lang, java.util, java.io, etc.)
- Support unnamed module mode (all classpath classes in unnamed module — current behavior)
- Gradually add module awareness without breaking existing classpath loading
- **Effort:** L

---

## Completed Tracks

### ✅ Track 1: Infrastructure (Phase F)
- ~~Full reflection~~ — already complete (Class.forName, Method.invoke, Field.get/set, Constructor.newInstance)
- ~~CONSTANT_Dynamic~~ — ldc handler + resolution cache
- ~~Classloader architecture~~ — parent delegation, loadClass, findLoadedClass, Runtime.loadLibrary
- ~~Networking~~ — Socket, ServerSocket, InetAddress, URL (phases 48-72)
- Remaining: JNI foundation (XL), user-defined classloaders

### ✅ Track 3: Hardening (Phase G)
- ~~Enhanced NPE messages (JEP 358)~~ — field/method/array context in NPE messages
- ~~Hidden classes (JEP 371)~~ — hidden flag on Class, Lookup.defineHiddenClass
- ~~Module system basics~~ — Module attribute extraction, module_name on Class
- Remaining: full module graph resolution (XL), concurrent GC → moved to Phase L

### ✅ Track 4: Testing (Phase H)
- ~~Comprehensive edge-case tests~~ — scoped values, condy, struct layouts, virtual threads
- Remaining: real-world Java program validation, TCK-like compliance

---

## Priority Matrix

| Priority | Feature | Phase | Impact | Effort |
|----------|---------|-------|--------|--------|
| **1** | **Method inlining** | J1 | -30% total, close Sieve/Matrix gaps | L |
| **2** | **Escape analysis** | J2 | -25% on Matrix, eliminate allocations | L |
| **3** | **Graph-coloring RA** | J3 | -15% across all benchmarks | M |
| **4** | **JIT inline caching** | J4 | Eliminate virtual call dispatch overhead | M |
| **5** | **JIT switch compilation** | J5 | More methods JIT-eligible | M |
| **6** | **Advanced loop opts** | J6 | Unrolling, peeling, strength reduction | L |
| **7** | **Instruction selection** | J7 | LEA, CMOV, FMA3, const materialization | M |
| **8** | **Inline GC barriers** | J8 | Eliminate helper calls on ref stores | S |
| **9** | **Profile-guided opt** | J9 | Branch/receiver profiling for JIT | L |
| **10** | **Const prop + DCE** | J10 | Fold constants, skip dead code | M |
| **11** | **Tail-call optimization** | J11 | Recursive → loop, no stack growth | S |
| **12** | **Sea-of-nodes IR** | J12 | SSA IR for production-quality opts | XL |
| 13 | JNI foundation | M1 | Unlocks native library ecosystem | XL |
| 14 | Full module system | N1-N4 | Required for JDK 17+ boot classes | L-XL |
| 15 | ARM64 emitter | K1 | Platform coverage | XL |
| 16 | Concurrent marking | L1 | GC pause reduction | XL |
| 17 | G1-style regions | L2 | Production GC | XL |

**Effort scale:** S = 1-3 days, M = 1-2 weeks, L = 2-4 weeks, XL = 2-3 months

---

## Java LTS Version Support

### Completed

| Version | Class File | Key Features Implemented |
|---------|-----------|--------------------------|
| **Java 8** | 52 | Full language support, lambdas, streams, invokedynamic |
| **Java 11** | 55 | Nest-based access control (JEP 181), private interface methods, String enhancements |
| **Java 17** | 61 | Records (JEP 395), sealed classes (JEP 409), reflection APIs |
| **Java 21** | 65 | Pattern matching for switch, virtual threads (simplified), sequenced collections |
| **Java 25** | 69 | Stream gatherers, scoped values, structured concurrency, flexible constructors |

### Remaining per Version

**Java 11 gaps:** Full `java.net.http.HttpClient` (can defer — most apps use third-party HTTP).

**Java 17 gaps:** Full `sun.misc.Unsafe` / `VarHandle`, full module system for JDK 17 boot classes.

**Java 21 gaps:** Full virtual thread continuation support (stack-slicing), complete structured concurrency.

**Java 25 gaps:** Full Panama FFI (`Linker` with arbitrary signatures), Vector API (incubating), compact object headers.

### Success Metrics

| Milestone | Metric |
|-----------|--------|
| Java 11 | Run Spring Boot "Hello World" (reflection + classloading) |
| Java 17 | Run a records-based data processing pipeline |
| Java 21 | Run 100K virtual threads serving HTTP requests |
| Java 25 | Call a C library via Panama FFI |
| All | Maintain competitive performance vs JDK C2 |
| All | Zero test regressions at each milestone |

---

## Session-by-Session Implementation Roadmap

Each step is scoped to **one implementation session** (~2-6 hours of focused work).
Steps are ordered by dependency — each builds on the previous.

### TIER 0: Fix the Foundation (Sessions 1-5) — Target: 28%

These sessions fix critical gaps that block everything else.

#### Session 1: Complete Missing Bytecodes
**Goal:** Implement all 256 JVM opcodes (currently ~123/200 used opcodes).
- Add `wide` prefix handling (wide iload, wide istore, wide iinc, wide ret)
- Add `jsr`/`ret` (deprecated but some class files still use them)
- Add `multianewarray` for multi-dimensional array creation
- Add `xxxunusedxxx` / reserved opcodes as explicit errors
- Add missing type conversion opcodes if any (d2l, f2l, etc.)
- **Test:** Write test for each new opcode using hand-crafted bytecode
- **Deliverable:** All opcodes return explicit result or explicit `UnimplementedOpcode` error (no silent fallthrough)
- **Readiness impact:** Interpreter 60% → 75%

#### Session 2: Exception Handling Hardening
**Goal:** Make try/catch/finally fully JVM-spec compliant.
- Fix `if_acmpeq`/`if_acmpne` for heap-allocated ObjectRef equality (known TODO in vm.rs)
- Implement exception table priority (first matching handler wins)
- Support `finally` via JSR/RET or modern exception ranges
- Implement `ExceptionInInitializerError` wrapping for `<clinit>` failures
- Handle stack unwinding across JNI boundaries
- **Test:** Nested try/catch, exception-in-catch, finally-with-return, cross-method unwinding
- **Deliverable:** Pass all 6 existing exception_tests + 10 new edge cases

#### Session 3: ClassLoader Parent Delegation Fix
**Goal:** Correct classloader hierarchy for bootstrap loading.
- Ensure bootstrap classloader (null) → extension classloader → app classloader chain
- `Class.getClassLoader()` returns null for bootstrap classes
- `ClassLoader.loadClass()` delegates to parent first (spec compliance)
- Implement `ClassLoader.findLoadedClass()` checking cache before delegation
- **Test:** Load same class from different loaders, verify isolation
- **Deliverable:** ClassLoader delegation chain matches JVM spec §5.3

#### Session 4: `MethodHandle` and `VarHandle` Completeness
**Goal:** Complete `MethodHandle` invocation beyond current partial support.
- Implement `MethodHandle.invokeExact()` and `MethodHandle.invoke()` with type adaptation
- Implement `MethodHandles.Lookup.findVirtual/findStatic/findConstructor`
- Implement `MethodHandles.lookup().in(targetClass)` for access checks
- Implement `VarHandle.get/set/compareAndSet` for field access
- Wire `MethodHandle` to invokedynamic bootstrap method resolution
- **Test:** MethodHandle for static, virtual, constructor; VarHandle CAS
- **Readiness impact:** invokedynamic 25% → 50%

#### Session 5: Static Initializer (`<clinit>`) Ordering
**Goal:** Correct `<clinit>` execution order per JVM spec §5.5.
- Implement "being initialized by current thread" guard (recursive init is no-op)
- Implement "being initialized by another thread" → wait until complete
- Implement `ExceptionInInitializerError` wrapping
- Track initialization state: Uninitialized → BeingInitialized → Initialized → Error
- Ensure superclass `<clinit>` runs before subclass
- **Test:** Diamond `<clinit>` dependencies, concurrent initialization from 2 threads
- **Readiness impact:** ClassLoading 50% → 55%

---

### TIER 1: JDK Bootstrap (Sessions 6-15) — Target: 40%

The **critical milestone**: load real JDK classes instead of synthetic stubs.

#### Session 6: Boot Classpath Auto-Discovery
**Goal:** Automatically find and load JDK jmod files.
- Detect `JAVA_HOME` from environment or `java` on PATH
- Enumerate `$JAVA_HOME/jmods/*.jmod` files
- Load `java.base.jmod` first (contains `java.lang.*`, `java.util.*`, `java.io.*`)
- Register boot classpath entries in `ClassManager`
- Verify JMOD magic bytes and version
- **Test:** Integration test that discovers local JDK, lists available modules
- **Deliverable:** `ClassManager::find_class("java/lang/Object")` returns real bytecode

#### Session 7: Bootstrap Class Loading — java.lang.Object
**Goal:** Load and initialize `java.lang.Object` from real JDK bytecode.
- Load `java.lang.Object` as the first class (no superclass)
- Handle the chicken-and-egg: Object's `<clinit>` uses `String`, but `String` extends `Object`
- Implement lazy resolution: mark class as "partially loaded" until dependencies resolve
- Parse Object's 11 native methods, wire to existing native implementations
- **Test:** `new Object()`, `obj.hashCode()`, `obj.equals(obj)`, `obj.toString()`
- **Deliverable:** Real JDK `java.lang.Object` loaded and functional

#### Session 8: Bootstrap — java.lang.Class and java.lang.String
**Goal:** Load the next two critical classes from JDK bytecode.
- `java.lang.Class`: mirror objects, `getName()`, `isInstance()`, `forName()`
- `java.lang.String`: backed by `byte[]` (compact strings, JDK 9+)
- Handle String's static initializer (COMPACT_STRINGS flag, LATIN1/UTF16 coders)
- Wire `String.intern()` to VM string table
- Implement `Class.getPrimitiveClass()` for int.class, long.class, etc.
- **Test:** `String.valueOf(42)`, `"hello".length()`, `Class.forName("java.lang.String")`

#### Session 9: Bootstrap — Core java.lang Classes
**Goal:** Load remaining essential `java.lang` classes from JDK.
- `System` (in/out/err streams, arraycopy, currentTimeMillis, getProperty)
- `Thread` (currentThread, sleep, getName, State enum)
- `Throwable` (stack trace, message, cause chain)
- `Number`, `Integer`, `Long`, `Double` (boxing, valueOf cache, parseXxx)
- `Boolean`, `Byte`, `Short`, `Float`, `Character`
- **Test:** `System.out.println()`, `Integer.parseInt("42")`, `Thread.currentThread().getName()`
- **Deliverable:** All 17 `java.lang` wrapper classes loaded from JDK

#### Session 10: Native Method Bridging
**Goal:** Map JDK native methods to existing Rust implementations.
- Scan loaded class for `native` flag on methods
- Auto-match `Java_java_lang_System_arraycopy` naming convention
- Fall back to registered native method table for non-standard names
- Handle `registerNatives()` calls from `<clinit>` (Object, Class, System, Thread)
- Wire `sun.misc.Unsafe` / `jdk.internal.misc.Unsafe` to existing implementations
- **Test:** `System.arraycopy()`, `Object.hashCode()`, `Thread.currentThread()`
- **Readiness impact:** java.lang.* 30% → 60%

#### Session 11: Bootstrap — java.util Core Collections
**Goal:** Load real `java.util.HashMap`, `ArrayList`, `LinkedList` from JDK.
- Load `AbstractCollection`, `AbstractList`, `AbstractMap`, `AbstractSet`
- Load `HashMap`, `ArrayList`, `LinkedList`, `HashSet`, `TreeMap`
- Handle `java.util.HashMap$Node` inner class loading
- Support `Iterator`, `Iterable`, `Spliterator` interfaces
- **Test:** `new HashMap<>().put("key", "value")`, `new ArrayList<>().add(42)`
- **Deliverable:** Remove synthetic collections stubs, use real JDK bytecode
- **Readiness impact:** java.util.* 10% → 50%

#### Session 12: Bootstrap — java.io and java.nio
**Goal:** Load I/O classes from JDK bytecode.
- `InputStream`, `OutputStream`, `Reader`, `Writer` abstract classes
- `FileInputStream`, `FileOutputStream`, `BufferedReader`, `PrintStream`
- `ByteArrayInputStream`, `ByteArrayOutputStream`
- `java.nio.ByteBuffer`, `CharBuffer` (for String encoding)
- `java.nio.charset.Charset`, `StandardCharsets`
- Wire native file I/O methods to existing `native-io` implementations
- **Test:** Read a file, write a file, BufferedReader.readLine()

#### Session 13: Bootstrap — java.util.concurrent ✅ DONE
**Goal:** Load concurrency primitives from JDK bytecode.
- `ReentrantLock`, `Condition` (backed by VM monitors)
- `AtomicInteger`, `AtomicLong`, `AtomicReference` (backed by Unsafe CAS)
- `ConcurrentHashMap` (uses Unsafe for CAS operations)
- `CountDownLatch`, `Semaphore`, `CyclicBarrier`
- `ExecutorService`, `ThreadPoolExecutor`, `ForkJoinPool`
- **Test:** `new AtomicInteger(0).incrementAndGet()`, `ConcurrentHashMap.put()`
- **Result:** 18 tests (15 structure + 3 runtime). All concurrency primitives backed by VM monitors/Unsafe CAS. Executors use synchronous dispatch (threaded pools deferred to Session 23).
- **Readiness impact:** Threading 40% → 55%

#### Session 14: Module System Wiring
**Goal:** Connect module resolution to class loading.
- Parse `module-info.class` from each loaded jmod
- Build module graph: `java.base` requires nothing, others require `java.base`
- Enforce `exports` directives at class resolution time
- Support `opens` for reflection access
- Handle unnamed module (classpath classes)
- **Test:** Load class from `java.sql` module, verify it can access `java.base` exports

#### Session 15: Remove Synthetic Stubs ✅ DONE
**Goal:** Delete synthetic Rust reimplementations now replaced by real JDK bytecode.
- Remove `native-collections` crate synthetic stubs (or gate behind `no-jdk` feature)
- Remove `util_time.rs` synthetic java.time stubs
- Remove synthetic BigInteger/BigDecimal stubs
- Keep native method implementations (these are still needed for JNI)
- Audit: any remaining `// STUB:` or `// TODO(Phase 41)` markers should be resolved
- **Test:** Full test suite still passes with real JDK classes
- **Deliverable:** Clean separation: real JDK bytecode + Rust native methods only
- **Result:** 3-layer gating: (1) `synthetic-jdk` Cargo feature propagated from vm → native-builtins + native-collections, (2) `#[cfg(feature)]` on re-exports in `vm/src/native/mod.rs`, (3) runtime `config.use_synthetic_jdk` flag. All synthetic stubs marked `DEPRECATED (Session 15)`. Zero `// STUB:` markers. JNI fully maintained (4,390 lines).
- **Readiness impact:** Bootstrap 5% → 60%, Overall ~25% → ~40%

---

### TIER 2: Bootstrap & Real JDK ✅ DELIVERED (Sessions 16-60) — Target: 65%

**T2.12 Verification (Session 60):**
- **T2.12.1** Readiness: ≥ 65%. All 12 T2 subsections (178 items) complete:
  T2.1 (4/4), T2.2 (30/30), T2.3 (20/20), T2.4 (20/20), T2.5 (15/15),
  T2.6 (20/20), T2.7 (20/20), T2.8 (16/16), T2.9 (20/20), T2.10 (6/6),
  T2.11 (5/5 smoke tests written, #[ignore] for CI), T2.12 (2/2 this entry).
- **T2.12.2** T2 ✅ DELIVERED.

### TIER 2 Detail: Interpreter Hardening (Sessions 16-22) — Target: 50%

#### Session 16: Iterative Interpreter Refactor ✅ DONE
**Goal:** Eliminate stack overflow on deep call chains (streams, recursion).
- Replace any remaining recursive `execute_method()` paths with explicit frame stack
- Implement tail-call elimination for self-recursive methods
- Set configurable max frame depth (default 1024, configurable via `-Xss`)
- Detect stack overflow and throw `StackOverflowError`
- **Test:** Fibonacci(50) iterative, 10K-deep call chain, Stream.collect() pipeline
- **Result:** Fully iterative `execute_frame()` with `frame_idx` tracking. TCE via `Frame::reset_for_tail_call()` at interpreter.rs:5504-5535. 10 tests: fib50, deep-1000, tail-sum-10K, mutual recursion, stack overflow, configurable depth, frame reset, monitor release.

#### Session 17: Reflection Completeness ✅ DONE
**Goal:** Full `java.lang.reflect` support for frameworks.
- `Method.invoke()` with proper access checks and type coercion
- `Field.get()`/`Field.set()` with accessibility override
- `Constructor.newInstance()` with varargs
- `Proxy.newProxyInstance()` for dynamic proxy creation
- `Class.getDeclaredMethods/Fields/Constructors` with proper filtering
- **Test:** Invoke private method via reflection, create dynamic proxy
- **Result:** 24 tests in interpreter_tests.rs. Full Method.invoke with access checks and type coercion. Field get/set with typed variants. Constructor.newInstance. Proxy dispatch through InvocationHandler.invoke() via `proxy_invoke_handler_shared()` in vm_exec.rs. getDeclaredMethods/Fields/Constructors with proper filtering.

#### Session 18: Annotation Processing ✅ DONE
**Goal:** Runtime annotation support for frameworks.
- Parse `RuntimeVisibleAnnotations` attribute from class files
- `Method.getAnnotation(Class)`, `Class.getAnnotations()`
- Handle annotation with values (String, int, enum, nested annotation, arrays)
- Support `@Inherited` and `@Repeatable` meta-annotations
- **Test:** `@Override`, `@FunctionalInterface`, custom annotation with values
- **Result:** 10 tests. Full RuntimeVisibleAnnotations/ParameterAnnotations parsing. Class/Method/Field getAnnotation(s), isAnnotationPresent. @Inherited walks superclass chain. Custom annotation values (String, int, enum, nested, arrays). AnnotationData + AnnotationElementValue in native-api.

#### Session 19: Generics and Type Erasure Support ✅ DONE
**Goal:** `java.lang.reflect.Type` hierarchy for generic-aware reflection.
- Parse `Signature` attribute for generic type info
- Implement `ParameterizedType`, `TypeVariable`, `WildcardType`, `GenericArrayType`
- `Method.getGenericParameterTypes()`, `Class.getTypeParameters()`
- **Test:** Generic method reflection, bounded wildcards
- **Result:** 10 tests. Full recursive descent Signature parser in generics.rs. All four Type subclasses as synthetic objects. Class/Method/Field generic type reflection methods. Tests cover single/multi type params, bounds, generic superclass, method type params, field generic type.

#### Session 20: Full `java.util.stream` Support ✅ DONE
**Goal:** Real Streams pipeline execution (not synthetic).
- Ensure `Spliterator` and `AbstractPipeline` class loading works
- Wire `Collectors.toList()`, `Collectors.groupingBy()`, etc.
- Support parallel streams via ForkJoinPool (Session 13 prerequisite)
- **Test:** `list.stream().filter().map().collect(toList())`, parallel reduction
- **Result:** 30 tests. Full pipeline: filter, map, flatMap, collect (toList/toSet/toMap), reduce, forEach, count, findFirst, anyMatch/allMatch/noneMatch, sorted, distinct, limit, skip, toArray, peek, min/max, Stream.of/empty/concat/toList, IntStream.range, mapToInt, parallel. Wrapper unboxing for distinct/groupingBy/sorted.

#### Session 21: String Concat and Formatting ✅ DONE
**Goal:** Full `java.util.Formatter` and `String.format()`.
- Ensure `StringConcatFactory` works with all recipe types
- `String.format()` delegates to `Formatter` (loaded from JDK)
- `MessageFormat`, `DecimalFormat` for localized formatting
- **Test:** `String.format("%.2f", 3.14159)`, `MessageFormat.format()`
- **Result:** 30 tests. StringConcatFactory (invokedynamic), String.format with all specifiers (%s,%d,%f,%x,%X,%o,%c,%b,%e,%E,%%,%n) + flags/width/precision, String.formatted(), Formatter object, MessageFormat.format(), DecimalFormat with grouping/precision. Wrapper unboxing in format_arg().

#### Session 22: Properties and Resource Loading ✅ DONE
**Goal:** `System.getProperties()`, `ResourceBundle`, classpath resources.
- Implement `ClassLoader.getResourceAsStream()` for loading from JARs
- `Properties.load()` from .properties files
- `System.getProperty("os.name")`, `user.dir`, `file.separator`
- `ResourceBundle.getBundle()` for i18n
- **Test:** Load .properties file, read system properties
- **Result:** 30 tests. System.getProperty for os.name, file.separator, line.separator, path.separator, user.dir, user.home, file.encoding, java.version, java.vendor, java.io.tmpdir. System.setProperty, getenv, lineSeparator. Properties: constructor, get/set/size/containsKey/remove/isEmpty/clear/overwrite, load(InputStream) with key=value/key:value/comments. ResourceBundle.getBundle with .properties auto-loading.

---

### TIER 3: Threading & GC Production Readiness (Sessions 23-30) — Target: 60%

#### Session 23: Java Memory Model Compliance ✅ DONE
**Goal:** Correct happens-before semantics.
- `volatile` read/write with proper memory fences (acquire/release)
- `synchronized` block establishes happens-before with subsequent lock acquire
- `Thread.start()` happens-before first action in started thread
- `Thread.join()` — all actions in joined thread happen-before join returns
- Fixed JIT getstatic cache that broke cross-thread static field visibility
- Thread constructor natives, Thread.run() Runnable delegation, JIT thread-safety
- **Test:** Dekker's algorithm, double-checked locking pattern — 11 tests pass
- **Result:** ThreadBasicTest + 10 MemoryModelTest methods (volatile visibility, join/start HB, synchronized HB, DCL, Dekker, volatile counter, wait/notify, synchronized counter, volatile store-load)

#### Session 24: Thread.interrupt() and Timed Waits
**Goal:** Complete thread interruption semantics.
- `Thread.interrupt()` sets interrupt flag
- Blocking ops (`Object.wait()`, `Thread.sleep()`, `LockSupport.park()`) check and throw `InterruptedException`
- `Thread.isInterrupted()` reads without clearing, `Thread.interrupted()` reads and clears
- Timed `wait(millis)`, `sleep(millis)`, `park(blocker)` with timeout
- **Test:** Interrupt sleeping thread, interrupt waiting thread

#### Session 25: Virtual Threads Integration ✅ DONE
**Goal:** Wire existing virtual thread implementation to interpreter.
- Connect `VirtualThreadManager` to `Thread.Builder.ofVirtual()`
- Implement continuation yield/resume with frame freeze/thaw
- Schedule virtual threads on carrier thread pool (VirtualThreadScheduler acquire/release)
- `Thread.isVirtual()`, `Thread.ofVirtual().start(runnable)`, `Thread.startVirtualThread()`
- Fixed gc/old_gen.rs orphaned impl block (Session 26 code outside impl)
- **Test:** 10 tests — single VT, isVirtual, 100 VTs, 1000 VTs, builder API, mixed VT+platform, HB semantics
- **Result:** VirtualThreadTest with 10 methods, all pass. 1000 concurrent virtual threads verified.

#### Session 26: GC Compaction ✅ DONE
**Goal:** Full heap compaction to eliminate fragmentation.
- Implement mark-compact for old generation (sliding compaction)
- 4-phase algorithm: compute forwarding addresses → update internal refs → slide objects → rebuild free list
- Update all references after compaction (forwarding pointers in ObjectHeader)
- Cross-gen fixup: young-gen refs to old-gen objects updated via fixup_young_old_refs()
- Root fixup: compact_map merged into GcResult pointer_map → VM update_all_roots() fixes JNI/threads/statics
- Dual reference format handling: 16-byte Value slots (object fields) + 8-byte compact pointers (reference arrays)
- OldGen helpers: free_block_count(), largest_free_block() for fragmentation metrics
- major_gc() converted from mark-sweep to mark-compact with HashMap<usize,usize> return
- **Test:** 6 new tests — compact_eliminates_fragmentation, compact_updates_internal_references, compact_recovers_fragmented_space, compact_pointer_map_in_gc_result, compact_no_move_when_contiguous, compact_forwarding_ptrs_cleared
- **Result:** All 6 S26 tests + 549 total GC tests pass. No regressions.

#### Session 27: GC Finalizer Support ✅ DONE
**Goal:** `finalize()` method support.
- Track objects with `finalize()` override via `has_finalizer` flag + `register_finalizable`
- GC resurrection: dead finalizable objects copied to to-space before from-space reset
- `collect_garbage_with_finalizers` in semispace and generational GC
- `FinalizerThread` two-stage pipeline: ref_processor → finalizer_thread → invoke finalize()
- Handle resurrection (finalizer stores `this` in static field to prevent future collection)
- JIT integration: `jit_new_object` registers finalizable objects
- **Test:** 7 tests — finalize runs, multiple finalizers, side effects, resurrection, no finalize on live/plain, System.gc collection
- **Result:** All 7 S27 tests pass. No regressions.

#### Session 28: Reference Processing (Soft/Weak/Phantom) ✅ DONE
**Goal:** `java.lang.ref.Reference` hierarchy.
- `WeakReference` — cleared when only weakly reachable, enqueued to ReferenceQueue
- `SoftReference` — cleared under memory pressure (LRU policy), enqueued to ReferenceQueue
- `PhantomReference` — enqueued after referent death (Java 9+ semantics: referent NOT cleared)
- `ReferenceQueue` — poll/remove/remove(timeout) for reference notification
- `PhantomReference.get()` always returns null per Java spec (dedicated native)
- `Reference.isEnqueued()` uses Int(1) sentinel in queue field for correct detection
- `process_references_after_gc()` now enqueues to_enqueue pairs into heap ReferenceQueue objects (linked-list protocol)
- Bootstrap classes: added PhantomReference, ReferenceQueue to preload list
- Synthetic stub fields: Reference/WeakRef/SoftRef/PhantomRef = 2 fields, ReferenceQueue = 2 fields
- **Test:** 12 tests — weak_ref_with_queue_enqueued_after_gc, soft_ref_with_queue_enqueued_under_pressure, phantom_ref_enqueued_to_queue, reference_queue_poll_empty, reference_refers_to, is_enqueued_lifecycle, multiple_refs_same_queue, enqueue_without_queue, soft_ref_not_enqueued_when_retained, weak_ref_get_null_after_clear, phantom_ref_get_always_null, rq_remove_timeout_empty
- **Result:** All 12 S28 tests pass. 549 GC tests pass. No regressions.

#### Session 29: GC Write Barrier Verification ✅ DONE
**Goal:** Prove write barriers are correct under all conditions.
- Property-based test: 50-object random graph with BFS reachability verification after GC
- Cross-gen old→young chain (old→y1→y2→y3 via dirty card), young→old references
- Old→young reference arrays: 4-element ref array in old gen pointing to young objects
- Card table: multiple dirty cards (5 old objects), overwrite-and-re-barrier correctness
- Write barrier false-positive check: old→old, young→young, non-ref stores don't dirty cards
- Promotion with cross-gen: chain A→B→C→D promoted, then D→E(young) via barrier
- Stress: 80 objects × 20 GC cycles with random link/unlink, verify all tags and links
- Stress: 1000 objects with random graph, 5 GC cycles, verify tag integrity throughout
- Edge case: promote object then immediately link to young, verify next GC preserves
- **Test:** 11 tests — random_graph_gc_never_collects_reachable, cross_gen_old_to_young_chain, cross_gen_young_to_old_survives, card_table_multiple_dirty_cards, card_table_overwrite_still_marks_new_target, stress_random_link_unlink_gc_cycles, cross_gen_old_to_young_ref_array, write_barrier_no_false_positives, gc_cycle_with_promotion_and_cross_gen_refs, stress_1000_objects_random_graph_gc, barrier_correctness_promoted_then_linked
- **Result:** All 11 S29 tests pass. 560 total GC tests pass. No regressions.

#### Session 30: Thread-Safe Class Loading ✅ DONE
**Goal:** Concurrent class loading without races.
- Per-class-name loading locks: `class_loading_locks` HashMap in SharedVm with condvar per class name
- `load_class_concurrent()` method: fast-path read lock → per-class lock → double-check → write lock
- Threads loading different classes proceed in parallel (no global lock contention)
- Threads loading the same class: second thread waits on per-class condvar, gets cached result
- Circular dependency guard via `loading_guard` HashSet in ClassManager
- Updated all production call sites: interpreter, invokedynamic, vm_exec, JNI FindClass
- Per-class lock cleanup after loading (Arc refcount check prevents unbounded growth)
- Class initialization already thread-safe via `class_init_waiters` condvar (re-entrancy handled)
- **Test:** 10 tests — concurrent_10_threads_same_class, concurrent_10_threads_different_classes, concurrent_class_for_name_returns_same_mirror, concurrent_load_and_init_stress (20 threads × 5 classes), class_loading_lock_prevents_duplicate_work, fast_path_no_lock_contention, per_class_lock_cleanup, load_class_concurrent_returns_same_id, load_class_concurrent_different_classes, circular_dependency_detected
- **Result:** All 10 S30 tests pass. 560 GC tests pass. No regressions.

---

### TIER 4: JIT Maturity (Sessions 31-38) — Target: 70%

#### Session 31: JIT Method Inlining ✅ DONE
**Goal:** Inline small methods at JIT call sites (Phase J1).
- During compilation, scan invoke targets: inline if < 35 bytecodes
- Remap callee locals to caller's spill area
- Deoptimization: invalidate compiled code on class load that changes call target
- **Test:** Benchmark getter/setter inlining, recursive method inlining

#### Session 32: JIT Escape Analysis ✅ DONE
**Goal:** Eliminate allocations for non-escaping objects (Phase J2).
- `plan_scalar_replacement()`: abstract interpretation tracks object provenance through stack/locals, identifies which putfield/getfield/invokespecial PCs operate on non-escaping objects
- `ScalarReplacedObject`: maps non-escaping NEW PCs to frame-local field storage (field slots allocated in JIT frame alongside LICM hoist slots)
- NEW (0xBB) codegen: scalar-replaced objects skip `jit_new_object` call, zero-initialize frame field slots, push dummy reference
- putfield (0xB5) codegen: scalar-replaced stores write directly to frame slot via `MOV [RBP-offset], value`
- getfield (0xB4) codegen: scalar-replaced loads read directly from frame slot via `MOV RAX, [RBP-offset]`
- invokespecial (0xB7): `<init>()V` on scalar-replaced objects skipped entirely (fields are zero-initialized)
- `non_escaping_new` wired through both interpreter.rs JIT compilation paths (early + deferred)
- Handles: multi-field objects (up to 16 fields), multiple scalar-replaced objects per method, field overwrites, local variable tracking (astore/aload provenance), escaping objects (fall through to normal allocation)
- **Result:** 11 tests (3 JIT unit + 8 VM integration) — Point sum/distance, multiple objects, 3D point, field overwrite, escaping object correctness, loop-local allocation, zero-init default

#### Session 33: JIT Inline Caching ✅ DONE
**Goal:** Monomorphic dispatch in compiled code (Phase J4).
- Extended `JitMICSlot` with `cached_entry_ptr` (AtomicU64), `cached_needs_context` (AtomicBool), hit/miss counters (AtomicU64), and methods: `update()`, `record_hit()`, `record_miss()`, `total_observations()`, `hit_rate_pct()`, `is_monomorphic()`, `is_megamorphic()`
- Enhanced `jit_invoke_virtual_mic` with three-tier dispatch: (1) cache hit + entry ptr → direct function pointer call (zero overhead), (2) cache hit + no entry → name-based dispatch + try_compile_callee for next time, (3) cache miss → full resolution + update all MIC fields
- Added `try_emit_inline()` for x64 code gen: inlines trivial 2-byte methods (iconst/iload + ireturn) directly into caller
- Fixed compilation: added `non_escaping_new` + `inline_sites` params to all `x64::compile()` call sites, fixed `CompiledMethod::new()` missing `inlined_methods` field, fixed borrow-checker issue in `resolve_inline_site`
- **Result:** 21 tests (11 JIT unit + 10 VM integration) — MIC lifecycle, hit/miss counters, monomorphic/megamorphic/polymorphic detection, concurrent stress, entry pointer caching, context flag propagation

#### Session 34: JIT Switch Compilation ✅ DONE
**Goal:** Compile `tableswitch`/`lookupswitch` (Phase J5).
- **Tableswitch**: CMP chain for ≤4 entries; O(1) jump table for >4 entries (LEA table base, MOVSXD relative offset, indirect JMP). Bounds check (CMP + JAE) guards the table. Jump table entries are i32 offsets patched post-emission via new `jump_table_patches` mechanism.
- **Lookupswitch**: Linear CMP chain for ≤6 pairs; balanced binary search tree for >6 pairs (O(log n) comparisons). `emit_binary_search_lookup()` recursively splits sorted keys: CMP mid → JE target, JL left subtree, fall through to right subtree.
- Added `try_emit_inline()` to x64 Compiler for trivial method inlining (iconst/iload + ireturn patterns)
- **Result:** 16 tests (12 JIT x64 + 4 VM integration) — tableswitch small/large/negative-low/nonzero-low/single-case/all-same/20-entry, lookupswitch linear/binary-search/negative-keys/single/two-pair, bounds-check safety, binary-search property test (170 inputs)

#### Session 35: JIT OSR (On-Stack Replacement) ✅ DONE
**Goal:** Enter JIT from running interpreter mid-method.
- Detect hot loops via back-edge counter (13 interpreter locations, OSR_THRESHOLD=1000)
- Compile method, transfer interpreter state to JIT frame via x86-64 OSR trampoline
- Resume execution from the loop header in compiled code (osr_pc_to_native mapping)
- XMM register transfer for float/double locals, Math.sqrt intrinsic support
- **Tests:** 30 OSR tests (simple sum, accumulator, nested loops, fibonacci, etc.)

#### Session 36: JIT Deoptimization ✅ DONE
**Goal:** Safe fallback from JIT to interpreter.
- When speculative optimization fails (wrong receiver type, uncommon trap)
- Reconstruct interpreter frame from JIT frame (FrameState, ReconstructedFrame)
- Invalidate compiled method, recompile with updated profile (DeoptimizationController)
- Speculative BCE deopt stubs call jit_uncommon_trap, return i64::MIN sentinel
- Interpreter detects deopt sentinel and falls through to re-execute in interpreter
- DeoptimizationPoint metadata on CompiledMethod for frame reconstruction
- InvalidationManager tracks class hierarchy assumptions (deleted 2026-09-12: never fed; `inlined_methods` plus the cache invalidation log replace it)
- **Tests:** 15 deopt tests (controller, escalation, invalidation, end-to-end, frame state)

#### Session 37: JIT Floating-Point Completeness ✅ DONE
**Goal:** All FP operations in JIT.
- IEEE 754 NaN/overflow fixup for f2i, f2l, d2i, d2l (NaN→0, +overflow→MAX, -overflow→MIN)
- SSE4.1 detection via CPUID + ROUNDSD intrinsics for Math.floor/ceil/rint
- Math.abs intrinsics: ANDPD (double), ANDPS (float), branchless SAR/XOR/SUB (int/long)
- Math.rint native added to native-builtins (round_ties_even)
- Regalloc bounds fix (idx < n instead of idx < 64)
- 27 new tests: 9 NaN/overflow, 4 normal conversions, 6 floor/ceil/rint, 7 abs, 1 N-Body style
- **Test:** All 27 pass, 421 interpreter (14 pre-existing failures), 542 JIT, 774 native-builtins (4 pre-existing)

#### Session 38: JIT Profile-Guided Optimization ✅ DONE
**Goal:** Use interpreter profiles to guide JIT (Phase J9).
- LoopTripProfile: back-edge counting, trip-complete recording, avg trip count, suggests_unroll_factor()
- Back-edge profiling in interpreter: 14 call sites across goto and all conditional backward branches
- PGO loop unrolling: profiled trip counts guide unroll factor (≤8 avg → 4x, ≤32 → 2x), extends eligibility to 100-byte bodies
- Branch prediction hints: 0x3E/0x2E prefixes on conditional branches from profile data
- MIC prepopulation from receiver type profiles (pre-existing, verified working)
- Regalloc bounds fix from S37 also applied
- 10 new integration tests + 7 profile unit tests: branch profiling, loop trip counts, biased branches, monomorphic/bimorphic dispatch, combined PGO, nested loops
- **Test:** All 10 pass, 456 interpreter (14 pre-existing), 561 JIT, 774 native-builtins (4 pre-existing)

---

### TIER 5: Ecosystem & Serviceability (Sessions 39-45) — Target: 75%

#### Session 39: JDWP Debugger Protocol ✅ DONE
**Goal:** Attach IDE debugger (IntelliJ, Eclipse) to CratonVM.
- JDWP transport with TCP socket, 14-byte handshake, packet protocol
- Breakpoint set/clear, single-step, resume via EventRequest commands
- Stack frame inspection (TR_FRAMES, TR_FRAME_COUNT with real frame data)
- Local variable reading (SF_GET_VALUES from frame snapshots)
- Thread listing and suspension (VM_ALL_THREADS, TR_SUSPEND/RESUME)
- Event firing: breakpoint/single-step events with location data sent as composite packets
- Event channel (mpsc) from interpreter threads to JDWP server for async event delivery
- Thread start/death event notifications on spawn/exit
- Command sets: VirtualMachine(1), ReferenceType(2), ThreadReference(11), EventRequest(15), StackFrame(16)
- **Tests:** 58 debug tests (protocol, transport, events, commands, frame inspection, event channel)

#### Session 40: JVMTI Completeness ✅ DONE
**Goal:** Support profilers and monitoring agents.
- Agent lifecycle (Agent_OnLoad, Agent_OnUnload)
- Event callbacks (MethodEntry, MethodExit, ClassLoad, GarbageCollectionStart)
- Heap iteration and object tagging
- GetClassMethods, GetLocalVariable
- VM wiring: notify_class_load in load_class_concurrent, notify_thread_end in thread exit,
  notify_exception in athrow, notify_monitor_contended_enter in monitorenter,
  notify_monitor_wait in Object.wait(), notify_method_exit on method return,
  GC tag sweep + update_after_gc + notify_object_free in both ST/MT GC paths
- Real ClassMethodProvider backed by ClassManager (VmClassMethodProvider)
- Real LocalVariableProvider backed by JvmThread frames (VmLocalVariableProvider)
- **Tests:** 53 unit tests (agent, capabilities, events, tags, heap walk, providers, notifications)

#### Session 41: JFR Event Completeness ✅ DONE
**Goal:** Flight Recorder with full event set.
- GC events (collection start/end, pause duration, heap usage)
- Thread events (start, end, sleep, park, contention)
- Class loading events (load, unload)
- JIT compilation events (compile start/end, deoptimization)
- File I/O and socket events

#### Session 42: Heap Dump Support ✅ DONE
**Goal:** `jmap -dump:format=b` compatible HPROF output.
- Full HPROF 1.0.2 binary format writer with all record types:
  - UTF-8 string records, LOAD_CLASS records, STACK_TRACE/STACK_FRAME records
  - HEAP_DUMP_SEGMENT with GC_CLASS_DUMP, GC_INSTANCE_DUMP, GC_OBJ_ARRAY_DUMP, GC_PRIM_ARRAY_DUMP sub-records
  - GC_ROOT_THREAD_OBJ and GC_ROOT_JNI_GLOBAL root records, HEAP_DUMP_END marker
- HprofBasicType enum mapping JVM field descriptors to HPROF type tags with size computation
- HprofClassInfo/HprofObjectInfo data structures for dump generation
- write_full_heap_dump() generates complete HPROF binary from classes, objects, threads
- Real heap_dump() on SharedVm: walks heap via walk_objects(), builds class hierarchy from ClassManager, reads field values from raw heap memory, writes binary file
- GC.heap_dump jcmd command wired to real heap_dump() implementation (was previously stub-only)
- Segment streaming with 64 MB max per HEAP_DUMP_SEGMENT for large heaps
- Class hierarchy traversal for correct INSTANCE_DUMP field ordering (superclass fields first)
- **Tests:** 53 HPROF tests + 4 new SharedVm heap dump integration tests, all passing

#### Session 43: Diagnostic Commands ✅ DONE
**Goal:** Runtime inspection like `jcmd` / `jinfo`.
- Thread dump (all threads with stack traces)
- Class histogram (object count and size by class)
- GC.run (trigger explicit GC)
- VM.flags (print current VM configuration)
- Real `VmDiagnosticState` implementation on SharedVm backed by live VM data:
  - thread_snapshots: reads from ThreadRegistry.all_thread_names()
  - heap_summary: reads young/old gen stats from VmHeap with new young_gen_stats()/old_gen_stats() methods
  - class_histogram: walks all heap objects via walk_objects(), groups by ClassId from ObjectHeader
  - trigger_gc: sets gc_requested AtomicBool, checked in interpreter's maybe_gc
  - vm_flags: reads VmConfig fields (GC algorithm, heap sizes, compressed oops, etc.)
  - system_properties: reads live HashMap from SharedVm
  - uptime_secs: reads from DiagnosticCounters start_time
  - command_line: reconstructed from VmConfig
- Added DiagnosticCounters + gc_requested fields to SharedVm
- Added heap_capacity(), young_gen_stats(), old_gen_stats() to VmHeap (with gen_heap/G1 backends)
- Fixed pre-existing jni.rs Send safety issue for DIRECT_BUFFERS global
- **Tests:** 83 total (60 serviceability + 12 diagnostics + 11 new SharedVm diagnostic state tests)

#### Session 44: JNI Completeness — Remaining 71 Functions ✅ DONE
**Goal:** Implement remaining JNI functions (currently 129/200).
- 201/234 function table slots implemented (86% — all achievable in Rust)
- `AttachCurrentThread` / `DetachCurrentThread` ✓
- `GetDirectBufferAddress` / `GetDirectBufferCapacity` / `NewDirectByteBuffer` ✓
- `DefineClass` / `RegisterNatives` ✓
- Critical sections (GetPrimitiveArrayCritical/ReleasePrimitiveArrayCritical) ✓
- 4 reserved slots (0-3, unused per spec) + 30 varargs slots (not implementable in stable Rust;
  V and A variants fully implemented as alternatives)
- **Readiness impact:** JNI 40% → 86%

#### Session 45: Run a Real Application ✅ DONE
**Goal:** JAR manifest parsing, `-jar` and `-cp` launch modes, Main-Class attribute.
- Made `ManifestInfo::parse` public, added `class_path: Option<String>` field and `Class-Path` parsing
- Added `resolve_class_path(&self, jar_path)` to resolve Class-Path entries relative to JAR parent dir
- Added `ClassPath::read_jar_manifest(jar_path)` static method for standalone JAR manifest reading
- Exported `ManifestInfo` and `ClassPath` from vm crate's public API
- CLI `--jar <FILE>` flag: reads manifest Main-Class, builds classpath from JAR + Class-Path entries
- CLI warns when `-cp` is used with `--jar` (ignored per JVM spec)
- Binary named `cratonvm` via `[[bin]]` in Cargo.toml
- Created `HelloWorld.java` test class with `main(String[])` and `check()` method
- 8 integration tests: manifest parsing, Class-Path resolution, JAR creation/execution, edge cases
- **Deliverable:** `cratonvm --jar hello.jar` reads Main-Class from manifest and launches

---

### TIER 6: Conformance (Sessions 46-55) — Target: 90%+

#### Session 46: TCK — java.lang Tests ✅ DONE
**Goal:** Comprehensive TCK tests for java.lang core classes.
- Created `TckLang.java` with 96 test methods covering Object, String, Integer, Long,
  Double, Float, Boolean, Byte, Short, Character, Math, System, StringBuilder,
  Exceptions, Class, Runtime, Thread, type casting, and autoboxing
- Object: hashCode consistency, equals identity/different, getClass, toString
- String: length, charAt, equals, compareTo, substring, indexOf, contains, isEmpty,
  trim, toLowerCase/toUpperCase, startsWith/endsWith, replace, toCharArray,
  valueOf(int/bool), concat operator (17 tests)
- Wrapper types: parseInt/parseLong/parseDouble/parseFloat/parseBoolean, valueOf,
  toString, constants, autobox cache, compareTo, isNaN, isInfinite, bits roundtrip
- Character: isDigit, isLetter, case conversion, char↔int, isWhitespace
- Math: abs, max/min, sqrt, pow, floor/ceil, round, constants, sin/cos, log/exp
- System: currentTimeMillis, nanoTime, arraycopy, identityHashCode
- StringBuilder: basic, appendInt, chain, length, reverse, delete
- Exceptions: getMessage, getCause, tryCatch, hierarchy, NPE class, finally
- Class: getName, isInterface, isPrimitive, isArray, getSuperclass
- Runtime: availableProcessors, memory
- Thread: currentThread, isAlive
- Type casting: int↔long, int↔float, double→int, char→int
- Autoboxing: int, double, boolean
- Registered 96 TCK test entries in `tck.rs` registry + `run_class_tests` in `run_all()`
- 96/96 tests via s46_test! macro in interpreter_tests.rs
#### Session 47: TCK — java.util Tests ✅ DONE
**Goal:** Comprehensive TCK tests for java.util collections and utilities.
- Created `TckUtil.java` with 28 test methods covering ArrayList, HashMap, HashSet, Arrays, Collections, Optional
- ArrayList: basic ops, mutations (set/remove/clear), grow, iterator, insert, lastIndexOf, capacity, toArray
- HashMap: basic ops, mutations, Integer keys (boxing/hashCode), getOrDefault, putIfAbsent, null key, keySet iteration, capacity
- HashSet: basic ops (add/contains/remove/clear, duplicate rejection), iterator
- Arrays: sort (int[]), copyOf (longer/shorter), asList
- Collections: emptyList, singletonList, reverse
- Optional: of/empty/isPresent/isEmpty/get, ofNullable/orElse
- Integration tests: frequency map (HashMap+ArrayList+Iterator), deduplication (HashSet+ArrayList+Iterator)
- Registered all 28 TCK test entries in `tck.rs` registry + `run_class_tests` in `run_all()`
- 28/28 integration tests pass, 0 regressions
#### Session 48: TCK — java.io / java.nio Tests ✅ DONE
**Goal:** Comprehensive TCK tests for java.io and java.nio APIs.
- Created `TckIo.java` with 48 test methods covering File (6), FileInputStream/
  FileOutputStream (7), ByteArrayStreams (7), StringReader/Writer (2), ByteBuffer (18),
  CharBuffer (2), IntBuffer (2), LongBuffer (1), and end-to-end integration (3)
- File: createNewFile/delete/exists, isFile/isDirectory, mkdir, length, absolutePath,
  canRead/canWrite
- Streams: write/read single byte and bulk, append mode, EOF detection (-1), available,
  skip, close idempotency, ByteArrayOutputStream toByteArray/size/reset/toString,
  ByteArrayInputStream readAll/available/skip
- ByteBuffer: allocate/capacity, put/get relative and absolute, wrap, flip, clear,
  rewind, mark/reset, putInt/getInt, putLong/getLong, putShort/getShort,
  putFloat/getFloat, putDouble/getDouble, putChar/getChar, hasArray, array,
  remaining, compact, slice, duplicate
- CharBuffer: allocate/put/get, wrap(CharSequence)/length/charAt
- IntBuffer: allocate/put/get, wrap(int[])
- LongBuffer: allocate/put/get
- End-to-end: file write-read roundtrip, ByteBuffer big-endian to byte array,
  BAOS→BAIS pipeline
- Registered 48 s48_test! macros in interpreter_tests.rs + run_class_tests in run_all()
- Existing Rust-side conformance infrastructure: 70 registry entries, IoNioConformanceSuite
  (40+ semantic checks), CompatibilityChecker (7 I/O-specific checks)
- 48/48 integration tests pass
#### Session 49: TCK — java.util.concurrent Tests ✅ DONE
Comprehensive conformance test suite for all j.u.c. classes — 70 tests covering
atomics (AtomicInteger, AtomicLong, AtomicBoolean, AtomicReference), locks
(ReentrantLock, ReentrantReadWriteLock, Condition), synchronizers (CountDownLatch,
Semaphore, CyclicBarrier), concurrent collections (ConcurrentHashMap,
CopyOnWriteArrayList), blocking queues (LinkedBlockingQueue, ArrayBlockingQueue),
and CompletableFuture (complete, cancel, exceptionally, thenApply, thenAccept).
Includes 11 multi-threaded stress tests. Fixed: field layout entries for ~30 j.u.c.
classes in synthetic_stub_fields(), LBQ/ABQ offer() capacity enforcement, TimeUnit
enum static fields with <clinit>, CF cancel/isCancelled/completeExceptionally/
isCompletedExceptionally with done-state encoding (0=pending, 1=normal,
2=exceptional, 3=cancelled), deferred exceptionally() handler evaluation via
source+handler fields. Registered 70 TCK entries in `tck.rs` + `run_class_tests`
in `run_all()`. 70/70 tests pass.
#### Session 50: TCK — Reflection and Annotation Tests ✅ DONE
**Goal:** Comprehensive TCK tests for java.lang.reflect and annotation APIs.
- Created `TckReflect.java` with 93 test methods covering Class metadata (17),
  Method reflection (10), Field reflection (10), Constructor reflection (8),
  Annotations at class/method/field level (13), Array reflection (6), Proxy (4),
  Modifier (7), Class hierarchy (3), and miscellaneous integration (15)
- Class: forName, getName, getSimpleName, getSuperclass, isInterface, isPrimitive,
  isArray, isEnum, isAnnotation, getModifiers, isAssignableFrom, isInstance,
  getInterfaces, getComponentType, cast, newInstance
- Method: getDeclaredMethod, invoke (instance/static/private), getReturnType,
  getParameterTypes, getParameterCount, getModifiers, getDeclaringClass, getDeclaredMethods
- Field: getDeclaredField, get/set, getPrivate, getInt/setInt, getType, getModifiers,
  getDeclaringClass, getDeclaredFields
- Constructor: getDeclaredConstructor, newInstance (noArgs/withArgs/private),
  getParameterTypes, getModifiers, getDeclaringClass, getDeclaredConstructors
- Annotations: class present/absent/value, inherited/inheritedValue,
  declaredExcludesInherited, getAnnotationsIncludesInherited, method present/value/
  default/absent, field present/value
- Array: newInstance, getLength, get/set, getObject/setObject, newInstanceRef
- Proxy: create, isProxyClass, getHandler, objectMethods
- Modifier: isPublic/isStatic/isFinal/isAbstract/isInterface/isPrivate, toString
- Misc: invokeReturnBoxed, multiFieldRead, ctorThenInvoke, getMethodInherited,
  noSuchField/noSuchMethod, invocationTargetException, getPublicFields/Methods/
  Constructors, primitiveClass, voidClass
- Registered 93 TCK entries in `tck.rs` + `run_class_tests` in `run_all()`
- 93/93 tests pass
#### Session 51: JDK 25 — Scoped Values (JEP 487) ✅ DONE
Comprehensive ScopedValue (JEP 487) conformance tests: 30 tests covering basic
operations (newInstance, where/run, where/call, get, isBound), orElse/orElseThrow,
nested rebinding with restore, multiple ScopedValues with chained where, call with
return values, exception handling (unbind on exception), hashCode stability, null
binding, thread inheritance (child visibility, child rebind isolation), and carrier
operations (reuse, reuse after exception). Fixes: registered Carrier.call with
JDK 25 CallableOp descriptor, changed ScopedValue.get() to throw NoSuchElementException
(matching JDK spec), added synthetic field entries for ScopedValue (3), Carrier (3),
Snapshot (2), StructuredTaskScope (8), Subtask (4). All 30/30 tests passing.
#### Session 52: JDK 25 — Structured Concurrency (JEP 480) ✅ DONE
Complete JEP 505 (JDK 25 final) structured concurrency implementation. Added
Joiner API with 4 built-in joiners: allSuccessfulOrThrow(), anySuccessfulResultOrThrow(),
awaitAllSuccessfulOrThrow(), awaitAll(). Joiner.onComplete() notifies on subtask
completion with policy-aware short-circuiting. Joiner.result() produces final
result or throws stored exceptions. Config API for scope configuration:
withName(), withThreadFactory(), withTimeout(). Scope owner thread validation
via register/check/unregister pattern — join() and close() verify caller is
the owner thread. Enhanced open(Joiner) factory tracks joiner-scope association
and notifies joiner on fork completion. Synthetic field registration for Joiner (4 fields)
and Config (3 fields) in class_manager.rs. 43 unit tests + 6 VM integration tests,
all passing.
#### Session 53: JDK 25 — Pattern Matching Completeness ✅ DONE
Fixed 4 core bugs in pattern matching implementation: (1) null pattern returns -1 per JDK
spec in execute_type_switch/execute_enum_switch, (2) removed primitive narrowing fallback
in type_switch_match — JEP 441 uses pure instanceof semantics, (3) added stack padding to
new_pooled_cached in frame.rs, (4) added SwitchLabel::PrimitiveClass for JEP 507 Dynamic
constant pool entries (ConstantBootstraps.primitiveClass). Created PatternComplete.java
with 22 test methods covering type patterns (5), guard expressions (3), record patterns (5),
sealed class patterns (3), instanceof patterns (4), and mixed integration (2). All 22 tests
passing. 22 TCK registry entries added.
#### Session 54: JDK 25 — Compact Object Headers (JEP 450) ✅ DONE
Implemented JEP 450/519 Compact Object Headers in `gc/src/compact_header.rs`:
- **CompactHeader**: 8-byte headers (vs 32-byte legacy), saving 24 bytes per object
- **NarrowKlassTable**: bidirectional ClassId ↔ u32 compressed class pointer mapping
- **HeaderView**: unified enum abstracting Legacy and Compact header formats
- **CompactAllocator**: bump-pointer allocator producing 8-byte-header objects and arrays
- **HashCodeTable**: side table for lazy identity hash codes (most objects never need hash)
- **CompactHeaderSavingsReport**: memory accounting with human-readable format output
- **element_byte_size**: helper for array element sizing across all primitive types
- Bit layout: NKlass(32) | GC age(7) | Lock(2) | Hash flag(1) | Array flag(1) | Varied(17) | Flags(4)
- 32 s54_ unit tests in gc crate, 8 s54_ integration tests in vm crate — all 40 passing
#### Session 55: Full TCK Passage and Release

---

### Progress Tracking

| Session | Component | Readiness Before | Readiness After | Cumulative |
|---------|-----------|-----------------|-----------------|------------|
| 1-5 | Foundation fixes | 25% | 28% | 28% |
| 6-10 | JDK bootstrap core | 28% | 35% | 35% |
| 11-15 | JDK bootstrap libs + stub removal | 35% | 40% | 40% |
| 16-22 | Interpreter hardening | 40% | 50% | 50% |
| 23-30 | Threading + GC production | 50% | 60% | 60% |
| 31-38 | JIT maturity | 60% | 70% | 70% |
| 39-45 | Ecosystem + serviceability | 70% | 75% | 75% |
| 46-55 | Conformance + JDK 25 features | 75% | 95% | 95% |

---

## JVM 25 Readiness Assessment (2026-04-14)

Overall readiness: **~28%** ("can this run *any* OpenJDK 25 application correctly").

**Why ~28% and not higher:** Sessions 11–50 (real-JDK bootstrap of `java.util` collections, reflection, annotations, streams, formatting, Properties, JMM, interrupt/timed waits, JFR/JDWP wiring, JNI 234-slot completeness, TCK lang/reflect own-test suites) and Phase R (crypto/http2/tls/serialization stub correctness) closed most *intra-class* gaps for trivial-to-moderate Java programs. The remaining 72% is dominated by **subsystems that simply don't exist yet** (real Berkeley sockets exposed to Java, NIO Selector on epoll/kqueue/IOCP, real TLS/crypto outside the experimental feature flag, JDBC, virtual threads, jimage, JCK pass), plus **latent JIT correctness debt** (the static skip list still bans `java/util/*`, `java/lang/*`, `cratonvm/*`, `<init>`, `<clinit>`, interface defaults, and finalizer classes — see `vm/src/jit/skip_list.rs`).

**Why not lower:** the bootstrap of real JDK bytecode against ~150 Rust-side essential natives works end-to-end for HashMap/ArrayList/Stream/Reflection/Properties; JDWP server runs against IntelliJ-class clients; AOT/CDS round-trip works; OSR + escape analysis + method inlining are wired in the JIT; 5,100+ workspace tests pass.

**Concrete evidence (sampled 2026-04-14):**
- 140 bare `native_noop` registrations remain across `native-builtins/src/*` (Phase R closed crypto/http2/tls/serialization; `lang_*`, `util_time`, `panama`, `vector_api`, `phases_*` still hold the rest).
- 166 `unwrap()` / `expect()` / `panic!()` sites in the three hottest files (`interpreter.rs`, `vm_exec.rs`, `x64.rs`) — A3 gate still open.
- `synthetic-jdk` feature is still ON by default in `vm/Cargo.toml` — K1.1 not done.
- No `vm/src/net` module; no epoll/kqueue/IOCP selector in `native-io`. Real `TcpListener::bind` only appears inside the JDWP transport (debug-only).
- No `jimage` reader; class loading goes through JMOD only — Phase B1.3 still open.
- JIT static skip list (`vm/src/jit/skip_list.rs`) still ships with `java/util/*`, `java/lang/*`, `cratonvm/*`, `cratonvm/Tck*`, `<init>`, `<clinit>`, interface-default, and `FinalizerTest` blanket bans — A1.1, A1.2, A1.4 unfinished.

The biggest blocker is no longer "synthetic stdlib" (Phase 70/71 fixed that on demand); it is **the absence of real I/O, networking, and crypto subsystems exposed to Java code**, plus the **JIT skip list** that forces interpretation for the bulk of `java.*`.

## JVM 25 Readiness Assessment (2026-04-14, after NEW-1..NEW-10)

Overall readiness: **~33%** (up from ~28% on the first 2026-04-14 snapshot).

### NEW-1..NEW-10 scorecard

| Phase | Delivered | Closed roadmap item | Lines added |
|-------|-----------|---------------------|-------------|
| **NEW-1** | JIT instanceof/checkcast fixed (load-on-demand + lambda-proxy fallback); `java/lang/*` + `TckClass` + `FinalizerTest` blanket bans removed; conservative GC root scan for active JIT frames via `JitEntryGuard` + `gc_quiescence`; `CRATONVM_JIT_ALLOW_PACKAGES` env-override for remaining `java/util/*` + `cratonvm/*` bans. CI gate in `jit/skip_list.rs` forbids reintroduction of the removed bans. | A1.2 partial; A1.1 stop-gap | ~900 |
| **NEW-2** | `InetAddress.getLocalHost` uses real gethostname; `getByAddress` IPv6 fixed; `isLoopbackAddress` / `isAnyLocalAddress` / `isLinkLocalAddress` / `isSiteLocalAddress` / `isMulticastAddress` all real; `InetAddress.getAddress()` real; `isReachable` via TCP echo probe; socket exception classes store their message; `NetworkInterface` portable hostname; e2e TCP loopback test. | D2.1, D2.6 partial | ~350 |
| **NEW-3** | Selector `wakeup()` via self-connected UDP channel; cross-platform poll (`libc::poll` on Unix, direct `WSAPoll` FFI on Windows); `select(0)` spec fix (infinite wait); `DatagramChannel` rewritten against a persistent `UdpSocket` in `s2_registry`; UDP channels registerable with a Selector. 9 new tests including end-to-end blocking-select-then-wakeup across threads. | D3.1, D3.2 | ~800 |
| **NEW-5** | Full jimage v1 reader (`reader/src/jimage.rs`) with header parse, perfect-hash lookup, location-record decoder, real-`$JAVA_HOME/lib/modules` smoke test. `ClassPathEntry::JImageFile` wired into `class_path.rs` with `class_to_module`/`resource_to_modules` indexes, auto-detection by file name. 29 tests including real-JDK round-trip. | B1.3 | ~1400 |
| **NEW-6** | 93 bare `native_noop` sites triaged and converted to `native_noop_with_this` (instance methods) or `native_return_null` (non-void returns); 2 type-correctness bugs in `crypto.rs` fixed. Every remaining `native_noop` is a true static (`registerNatives`, `<clinit>`, etc.). | Phase R continuation | ~500 edits |
| **NEW-7** | `#![cfg_attr(not(test), deny(clippy::unwrap_used, expect_used, panic))]` gate added to `interpreter.rs`, `vm_exec.rs`, `x64.rs`. Compile-time regression detection. `hot_files_have_no_production_panics` test enforces at `cargo test` level. Discovered the original 166-count was test-code noise; actual production count was 0. | A3.1–A3.3 for the 3 hot files | ~100 |
| **NEW-8** | `Lookup.defineHiddenClass` now validates magic, extracts HotSpot-style mangled name, honors `initialize` flag and `NESTMATE` ClassOption, propagates nest info via new `NativeContext::copy_nest_info` / `initialize_class`, raises typed exceptions on failure. `Class.isHidden()` reads the real flag (was stubbed `return_false`). 9 tests. | C2.2 (hidden classes) | ~600 |
| **NEW-9** | `Ret` verification now reads the ReturnAddress from the local slot; `Jsr` / `JsrW` push `pc + 3` / `pc + 5` (was incorrectly pushing the subroutine entry); worklist verifier rejects out-of-range branch targets instead of silently ignoring them. 10 differential VerifyError tests. | A4.3 | ~300 |
| **NEW-10** | `--dump-missing-natives <FILE>` CLI flag in `vm-cli`; `SharedVm::dump_missing_natives_json` writes a stable, diff-friendly JSON schema with `(class, name, descriptor, sample_call_site)`; deduped at record time so two runs on the same program produce byte-identical output; call-site capture records the first caller frame. 4 tests. | NEW-10 | ~350 |

### Concrete evidence (sampled 2026-04-14, post-NEW-1..10)

- **JIT skip list**: `java/lang/*`, `cratonvm/Tck*`, `FinalizerTest` bans **removed**. Remaining bans (`<init>`, `<clinit>`, interface defaults, `java/util/*`, `cratonvm/*`) are lifted per-package via `CRATONVM_JIT_ALLOW_PACKAGES` env var and tracked for real fix.
- **`native_noop` sites**: 140 → 47, all remaining are true statics. Net change: 93 sites gained proper documentation + 2 type-correctness bugs fixed.
- **`unwrap`/`expect`/`panic` in 3 hot files**: 166 → 0 production (was test-code noise). Compile-time gate prevents regression.
- **jimage**: `reader/src/jimage.rs` loads real `$JAVA_HOME/lib/modules` — demonstrated via smoke test that passes on the development host.
- **Sockets**: `java.net.*` was already backed by real `TcpStream` / `TcpListener` / `UdpSocket` in `native-builtins`; NEW-2 fixed the 7 actual gaps (DNS, IPv6 predicates, exception messages, reachability probe).
- **Selector**: real `libc::poll` on Unix, real `WSAPoll` on Windows; `select(0)` now blocks forever (was non-blocking); `wakeup()` works cross-thread.
- **Hidden classes** (JEP 371): actually functional end-to-end including NESTMATE semantics and `initialize` flag.
- **Bytecode verifier**: runs on every class load by default, correctly traces `jsr`/`ret` for pre-Java-7 classes, rejects out-of-range branches.
- **Missing-natives harness**: `vm-cli --dump-missing-natives FILE.json` produces a committable census file.
- **Synthetic-jdk default**: OFF (NEW-4 ✅ DELIVERED — `synthetic-jdk` removed from default features, all missing natives implemented across T2.2–T2.9).

The biggest remaining blockers are now **NEW-4** (flipping the synthetic-jdk default, which requires implementing the ~200 missing natives revealed by the NEW-10 census), **real JDBC** (Phase F), **real TLS/crypto outside the experimental feature flag** (Phase E), and the **JCK-level conformance** that comes with passing real stress tests from `jdk/test/java/*`.

### Original (2026-04-05) snapshot
The tables below remain useful as a per-component breakdown but the headline number above is the current honest assessment.

### Component Readiness Matrix

| Component | Status | Readiness | Blocking Issues |
|-----------|--------|-----------|-----------------|
| Class file reader | Supports up to class file version 69 | 80% | — |
| Bytecode interpreter | Core opcodes working, all standard bytecodes | 60% | Missing some wide/rare opcodes |
| JIT compiler (x86-64/AArch64) | Tiered compilation, IR optimizer, register allocator | 70% | Deopt coverage, OSR entry |
| Garbage collection | G1, ZGC, generational heap, concurrent mark | 50% | Write barrier verification, compaction |
| Threading | Virtual threads, monitors, VarHandles | 40% | Full JMM compliance, biased locking |
| java.lang.* natives | Many synthetics, not real JDK bytecode | 30% | Phase 41 bootstrap required |
| java.util.* collections | 100% synthetic stubs | 10% | Phase 41 bootstrap required |
| java.time / java.math | Synthetic Rust implementations | 10% | Phase 41 bootstrap required |
| ClassLoading / Verification | Bytecode verifier, access control, modules | 50% | Full sealed-class verification |
| JNI | Global/local refs, native method dispatch | 40% | Critical sections, AttachCurrentThread |
| JVMTI / JFR | Basic framework | 20% | Event completeness, agent lifecycle |
| Module system (JPMS) | Basic module support | 30% | Full readability/accessibility |
| invokedynamic / MethodHandle | Partial support | 25% | Full LambdaMetafactory, StringConcatFactory |
| Crypto / TLS | Experimental, feature-gated | 15% | Provider framework, key management |
| JDK 25 specific features | Patterns, concurrency, language | 15% | Scoped values, structured concurrency |
| Bootstrap (real JDK .class files) | Not yet — synthetic stubs replace stdlib | 5% | Phase 41 is the critical milestone |

### Path to 50% (Next Major Milestone)

1. **Phase 41 — JDK Bootstrap**: Load and execute real `java.*` classes from JDK jmod files instead of synthetic stubs. This single milestone would raise readiness from ~25% to ~40%.
2. **Full bytecode coverage**: Implement remaining rare/wide opcodes and complete `invokedynamic` support.
3. **GC hardening**: Write barrier verification tests, concurrent compaction, finalizer support.
4. **Threading compliance**: Full Java Memory Model (JMM) compliance, `Thread.interrupt()` semantics.
5. **JNI completeness**: `AttachCurrentThread`, critical sections, exception handling in native code.

### Path to 75% (Production Alpha)

1. **TCK subset passage**: Pass core TCK tests for `java.lang`, `java.util`, `java.io`.
2. **Real application execution**: Run Spring Boot hello-world, simple Gradle builds.
3. **JIT maturity**: OSR, full deoptimization, inline caches for virtual dispatch.
4. **Serviceability**: Complete JVMTI, JFR event coverage, JDWP debugger protocol.
5. **Security manager**: Sandbox support, class loader isolation, permission checks.

### Path to 100% (Full JVM 25 Conformance)

1. **Full TCK passage** for Java SE 25.
2. **All JEPs implemented** for JDK 25 (scoped values, structured concurrency, pattern matching, etc.).
3. **Performance parity** with HotSpot C2 on standard benchmarks.
4. **Production hardening**: Crash recovery, diagnostic dumps, heap dump support.
5. **Ecosystem compatibility**: Maven/Gradle plugins, IDE debugger integration, profiler support.

---

# Path to 100% Production Readiness — Definitive Plan (2026-04-08)

This section is the **complete, exhaustive list** of work remaining to make CratonVM able to run *any* OpenJDK 25 application correctly and competitively. Implementing every step below = 100% production readiness. Items are ordered by dependency, not by difficulty.

**Definition of "100% production-ready":** Passes the Java SE 25 TCK in full; runs Spring Boot, Hibernate, Tomcat, Netty, Maven, Gradle, Kafka, Cassandra, Elasticsearch, Jenkins, IntelliJ IDEA, javac, JShell unmodified; within 1.5x of HotSpot C2 on SPECjvm2008 / Renaissance / DaCapo; passes JCK conformance for `java.base`, `java.desktop`, `java.net.http`, `java.sql`, `java.security`, `java.management`, `jdk.jfr`, `jdk.jdi`.

---

## PHASE A — Correctness Foundations (must finish before anything else)

### A1. Eliminate the JIT skip list and fix latent JIT correctness bugs
- A1.1 Implement **GC stack maps for JIT frames** (oop-map per safepoint). Required for: precise root scanning, finalizer-test re-enable, JIT+GC coexistence.
- A1.2 Fix the JIT `instanceof` miscompilation that forced `TckLang` onto the skip list.
- A1.3 Fix JIT `tableswitch` / `lookupswitch` codegen (currently rejected → interpreter fallback).
- A1.4 Fix JIT control-flow miscompilations on permutation/branch-heavy code (Phase 16.3 carryover).
- A1.5 Fix JIT FP codegen edge cases (NaN propagation, subnormals, FMA, strict vs non-strict).
- A1.6 **Delete `jit_skip_classes` entirely**; CI gate forbids re-introduction.

### A2. Iterative interpreter (kill recursion-depth limit)
- A2.1 Replace recursive `execute_method` with an explicit Java call-frame stack.
- A2.2 Handle exception unwinding across the explicit stack.
- A2.3 Wire OSR / deoptimization to the iterative model.
- A2.4 Test: streams ≥ 1000 levels deep, mutual recursion 100k frames.

### A3. Remove all production `unwrap`/`expect`/`panic!` from hot paths
- A3.1 Audit `vm/src/runtime/interpreter.rs` (19 unwraps), `vm/src/vm/vm_exec.rs`, `jit/src/x64.rs` bytecode slicing.
- A3.2 Convert to typed `VmError` with proper Java exception conversion.
- A3.3 CI lint: `clippy::unwrap_used`, `clippy::expect_used` deny-by-default in non-test code.

### A4. Bytecode verifier (JVMS §4.10)
- A4.1 Stack-map frame verification (StackMapTable parsing already exists in reader).
- A4.2 Type inference verifier for pre-50 class files.
- A4.3 Subroutine (`jsr`/`ret`) handling for legacy classes.
- A4.4 Reject malformed class files with `VerifyError` matching HotSpot messages.

### A5. Class file completeness
- A5.1 All attributes: `BootstrapMethods`, `NestHost`, `NestMembers`, `PermittedSubclasses`, `Record`, `Module`, `ModulePackages`, `ModuleMainClass`, `MethodParameters`, `RuntimeVisibleTypeAnnotations`, `RuntimeInvisibleTypeAnnotations`.
- A5.2 Verify all 200 standard opcodes round-trip; add any missing.
- A5.3 `LoadableDescriptors` attribute (Valhalla preview) — at least parse and ignore.

---

## PHASE B — Java Module System (JPMS, JEP 261)

### B1. Module graph
- B1.1 Parse `module-info.class` (`Module`, `ModulePackages`, `ModuleMainClass`).
- B1.2 Build module graph from boot layer (`java.base`, `java.desktop`, …, all 70+ JDK modules).
- B1.3 Read modules from `lib/modules` (jimage / jmod) — implement jimage reader.
- B1.4 `requires`, `requires transitive`, `requires static` resolution.
- B1.5 Cycle detection, version conflict handling.

### B2. Access enforcement
- B2.1 `exports` / `exports to` qualified-export checks.
- B2.2 `opens` / `opens to` for deep reflection.
- B2.3 `IllegalAccessError` on cross-module access violations.
- B2.4 `--add-exports`, `--add-opens`, `--add-modules`, `--add-reads` CLI flags.

### B3. Layer API
- B3.1 `java.lang.ModuleLayer`, `Configuration`, `ModuleFinder`, `ModuleReference`, `ModuleReader` real implementations.
- B3.2 Custom module layers (used by app servers, Jigsaw apps).
- B3.3 `ServiceLoader` walks module `provides` declarations.

---

## PHASE C — ClassLoader Hierarchy

### C1. Real classloader model
- C1.1 Bootstrap, platform, system loaders as distinct objects with delegation.
- C1.2 `ClassLoader.defineClass` from arbitrary byte arrays (subclassable from Java).
- C1.3 `URLClassLoader` real implementation reading from JAR URLs.
- C1.4 Per-loader class namespace (same FQN under different loaders = different classes).
- C1.5 Parent-delegation override (custom loaders).

### C2. Bytecode generation libraries
- C2.1 ASM, ByteBuddy, CGLIB classloader interaction (defineClass + linking).
- C2.2 Hidden classes (`Lookup.defineHiddenClass`, JEP 371) — required by Lambda/Spring/Hibernate.
- C2.3 `Unsafe.defineAnonymousClass` legacy fallback.

### C3. Resource loading
- C3.1 `getResource` / `getResourceAsStream` walking classpath/module path.
- C3.2 `Class.getResource` relative resolution.
- C3.3 Multi-release JAR support (`../../../apps/META-INF/versions/N/`).

### C4. JAR/ZIP infrastructure
- C4.1 Real ZIP64 reader (already partial via miniz_oxide).
- C4.2 Signed JAR verification (`../../../apps/META-INF/MANIFEST.MF`, `*.SF`, `*.RSA/*.DSA/*.EC`).
- C4.3 Sealed packages enforcement.
- C4.4 Spring Boot fat-JAR `BOOT-INF/lib/*.jar` nested loading.
- C4.5 WAR/EAR support (Tomcat compatibility).

---

## PHASE D — Networking (`java.net`, `java.net.http`, `java.nio.channels`)

### D1. BSD sockets primitives
- D1.1 Native `socket()`, `bind()`, `listen()`, `accept()`, `connect()`, `send()`, `recv()`, `close()` via `libc` / Winsock2.
- D1.2 IPv4 + IPv6 dual stack.
- D1.3 Socket options: `SO_REUSEADDR`, `SO_KEEPALIVE`, `TCP_NODELAY`, `SO_LINGER`, `SO_RCVBUF`, `SO_SNDBUF`.
- D1.4 Non-blocking mode, `O_NONBLOCK`.

### D2. `java.net` API
- D2.1 `InetAddress`, `Inet4Address`, `Inet6Address` with real DNS resolution.
- D2.2 `Socket`, `ServerSocket`, `DatagramSocket`, `MulticastSocket`.
- D2.3 `URL`, `URLConnection`, `HttpURLConnection` (legacy but still used).
- D2.4 `URI` parser (already partial).
- D2.5 `Proxy`, `ProxySelector`, `CookieManager`.
- D2.6 `NetworkInterface` enumeration.

### D3. NIO channels & selectors
- D3.1 `SocketChannel`, `ServerSocketChannel`, `DatagramChannel`.
- D3.2 `Selector` backed by `epoll` (Linux), `kqueue` (BSD/macOS), `IOCP` (Windows).
- D3.3 `Pipe`, `FileChannel.transferTo/transferFrom` (zero-copy `sendfile`).
- D3.4 `AsynchronousSocketChannel`, `AsynchronousServerSocketChannel`, `AsynchronousFileChannel` with completion handlers.
- D3.5 `MemoryMappedByteBuffer` via `mmap`/`MapViewOfFile`.

### D4. HTTP/2 client (`java.net.http`, JEP 321)
- D4.1 `HttpClient`, `HttpRequest`, `HttpResponse`, `BodyPublishers`, `BodySubscribers`.
- D4.2 HTTP/1.1 framing.
- D4.3 HTTP/2 framing (HPACK, stream multiplexing).
- D4.4 WebSocket upgrade.
- D4.5 Connection pooling, redirects, auth.

### D5. Unix domain sockets (JEP 380)
- D5.1 `UnixDomainSocketAddress`, `SocketChannel.open(UNIX)`.

---

## PHASE E — Real Cryptography & TLS (`java.security`, `javax.crypto`, `javax.net.ssl`)

### E1. Provider architecture
- E1.1 `Provider`, `Security.getProviders()`, `Security.addProvider`.
- E1.2 SUN, SunJCE, SunRsaSign, SunEC, SunJSSE provider equivalents.
- E1.3 `KeyStore` (PKCS12, JKS) — real load/store.

### E2. Real algorithms (delegate to vetted Rust crates: `ring`, `rustls`, `aws-lc-rs`)
- E2.1 Hashes: MD5, SHA-1, SHA-2 family, SHA-3, BLAKE2, HMAC.
- E2.2 Ciphers: AES (CBC/CTR/GCM/CCM), ChaCha20-Poly1305, RSA-OAEP, RSA-PSS.
- E2.3 Asymmetric: RSA, DSA, ECDSA (P-256/384/521), EdDSA (Ed25519/Ed448), X25519/X448.
- E2.4 Post-quantum: ML-KEM (JEP 496), ML-DSA (JEP 497) — JDK 25 additions.
- E2.5 Key derivation: PBKDF2, HKDF, Argon2 (via JCE provider).
- E2.6 `SecureRandom` from OS RNG (`getrandom`, `BCryptGenRandom`).

### E3. TLS / DTLS (`javax.net.ssl`)
- E3.1 `SSLContext`, `SSLSocket`, `SSLEngine`, `SSLSession`.
- E3.2 TLS 1.2 + TLS 1.3 (delegate to `rustls`).
- E3.3 Cert chain validation, hostname verification, OCSP stapling.
- E3.4 SNI, ALPN, session resumption.
- E3.5 mTLS, client auth.
- E3.6 `KeyManagerFactory`, `TrustManagerFactory`, system trust store.

### E4. PKI
- E4.1 X.509 cert parsing (`CertificateFactory`).
- E4.2 `CertPathValidator`, `PKIXParameters`, CRL, OCSP.
- E4.3 `KeyPairGenerator`, `KeyAgreement` (DH, ECDH, X25519).

### E5. Signature & MessageDigest API surface
- E5.1 `Signature.getInstance` for all standard names.
- E5.2 `Mac.getInstance` for all HMAC names.
- E5.3 `Cipher.getInstance` with full transformation strings.

---

## PHASE F — JDBC & Database Connectivity

### F1. JDBC API (`java.sql`, `javax.sql`)
- F1.1 `Driver`, `DriverManager`, `Connection`, `Statement`, `PreparedStatement`, `ResultSet`, `ResultSetMetaData`, `DatabaseMetaData`.
- F1.2 `CallableStatement`, stored procedures.
- F1.3 `Blob`, `Clob`, `NClob`, `SQLXML`, `Array`, `Struct`.
- F1.4 Transaction isolation, savepoints, batch updates.
- F1.5 `DataSource`, `ConnectionPoolDataSource`, `RowSet`.
- F1.6 `ServiceLoader` discovery of `java.sql.Driver` implementations.

### F2. Driver loading test
- F2.1 PostgreSQL JDBC (pgjdbc) loads and connects.
- F2.2 MySQL Connector/J loads and connects.
- F2.3 H2, SQLite, MariaDB drivers smoke-tested.
- F2.4 HikariCP connection pool runs unmodified.

---

## PHASE G — Concurrency Completeness

### G1. Real virtual threads (JEP 444, Project Loom)
- G1.1 Continuation primitive (`jdk.internal.vm.Continuation`) — stackful coroutine.
- G1.2 Continuation freeze/thaw against the iterative interpreter (Phase A2).
- G1.3 ForkJoinPool-based scheduler (default carrier thread pool).
- G1.4 Yield on blocking I/O — integrate with NIO/HTTP client (Phase D).
- G1.5 Yield on `LockSupport.park`, `synchronized`, monitor `wait`, `Thread.sleep`.
- G1.6 Pinning detection + `VirtualThreadPinned` JFR event.
- G1.7 Test: 1M virtual threads on a 4-core box.

### G2. Structured concurrency (JEP 505)
- G2.1 `StructuredTaskScope.ShutdownOnFailure`, `ShutdownOnSuccess`, custom joiners.
- G2.2 Scope tree, cancellation propagation.

### G3. Scoped values (JEP 506) — real implementation, not stub.

### G4. `java.util.concurrent` audit
- G4.1 Complete `register_m18_concurrent_fixes` (TODO at `native-builtins/src/lib.rs:2527`).
- G4.2 `ConcurrentHashMap` real lock-free CAS-based segments (drop synthetic).
- G4.3 `LinkedBlockingQueue`, `ArrayBlockingQueue`, `LinkedTransferQueue`, `SynchronousQueue` real impls.
- G4.4 `ForkJoinPool` work-stealing (used by parallel streams + virtual threads).
- G4.5 `CompletableFuture` chain semantics, async executors.
- G4.6 `Phaser`, `CyclicBarrier`, `Exchanger`, `Semaphore` correctness audit.
- G4.7 `Atomic*` types via real CAS intrinsics.
- G4.8 `VarHandle` full API (already partial).

### G5. Java Memory Model
- G5.1 Acquire/release semantics for `volatile`.
- G5.2 `final` field freeze semantics.
- G5.3 Fences (`VarHandle.fullFence`, `acquireFence`, `releaseFence`, `loadLoadFence`, `storeStoreFence`).
- G5.4 JIT respects all of the above (no reordering past barriers).

---

## PHASE H — File System & I/O Completeness

### H1. `java.nio.file` (NIO.2)
- H1.1 `Path`, `Paths`, `Files` complete API.
- H1.2 `FileSystem`, `FileSystems`, `FileStore`.
- H1.3 `WatchService` (inotify/FSEvents/ReadDirectoryChangesW).
- H1.4 Symlinks, hard links, attributes (POSIX, DOS, ACL).
- H1.5 `DirectoryStream`, `Files.walk`, `Files.find`.
- H1.6 ZIP filesystem provider (`jar:file:...`).

### H2. Fix `native-io` simplifications
- H2.1 `FileChannel.size()` real impl (`native-io/src/lib.rs:4444`).
- H2.2 `RandomAccessFile.seek/getFilePointer` with fd_table (`6376/6381`).
- H2.3 `Scanner.nextToken` edge cases (`1683..1747`).
- H2.4 `FileLock`, advisory locks via `flock`/`LockFileEx`.

### H3. Console & process
- H3.1 `java.lang.ProcessBuilder` (already partial) — argv quoting on Windows.
- H3.2 `System.console()` real terminal interaction.
- H3.3 stdin redirection.

---

## PHASE I — Serviceability (JVMTI / JFR / JDWP / JMX)

### I1. JVMTI complete (JSR 163)
- I1.1 All ~150 JVMTI functions in `jvmti.h`.
- I1.2 Event firing hooks in interpreter + JIT for: ClassLoad, ClassPrepare, MethodEntry, MethodExit, Exception, ExceptionCatch, FieldAccess, FieldModification, MonitorContendedEnter/Entered, MonitorWait/Waited, GarbageCollectionStart/Finish, ObjectFree, VMStart, VMInit, VMDeath, ThreadStart, ThreadEnd.
- I1.3 Tag map, heap iteration callbacks (`IterateThroughHeap`, `FollowReferences`).
- I1.4 Bytecode instrumentation (`RetransformClasses`, `RedefineClasses`).
- I1.5 Used by: Java agents (`-javaagent:`), profilers (async-profiler, JProfiler, YourKit), debuggers.

### I2. JDWP complete (JSR 45)
- I2.1 Implement all 11 command sets, all sub-commands (currently many return "unimplemented").
- I2.2 Breakpoints, single-step, watchpoints (field access/modification).
- I2.3 Stack frame inspection, local variable read/write.
- I2.4 Method invocation in suspended thread.
- I2.5 ClassFileVersion, AllClasses, AllThreads, VirtualMachine.Version reply matching HotSpot.
- I2.6 Verify with IntelliJ IDEA, Eclipse, VS Code Java debuggers.

### I3. JFR complete
- I3.1 `.jfr` binary file writer (currently events emit but no export).
- I3.2 All ~170 built-in event types (allocation, GC, JIT, IO, exceptions, threading, network, file).
- I3.3 Custom event API (`jdk.jfr.Event` subclasses with `@Label`, `@Description`).
- I3.4 `jcmd JFR.start/stop/dump/check`.
- I3.5 JFR streaming API (`RecordingStream`).
- I3.6 Verify with JDK Mission Control.

### I4. JMX (`java.lang.management`, `javax.management`)
- I4.1 Replace synthetic data in `native-builtins/src/jmx.rs` with real beans.
- I4.2 `MemoryMXBean`, `ThreadMXBean`, `GarbageCollectorMXBean`, `ClassLoadingMXBean`, `RuntimeMXBean`, `OperatingSystemMXBean`, `CompilationMXBean` — real values from VM internals.
- I4.3 MBeanServer, dynamic MBeans, notifications.
- I4.4 RMI connector for remote JMX (depends on Phase D networking).
- I4.5 `jconsole`, `jvisualvm`, `jcmd`, `jstat`, `jmap`, `jstack`, `jinfo` compatibility.

### I5. Diagnostic commands (`jcmd`)
- I5.1 `Thread.print`, `GC.heap_dump`, `GC.run`, `VM.flags`, `VM.system_properties`, `VM.uptime`, `Compiler.codecache`.
- I5.2 hprof heap dump format writer.

---

## PHASE J — Native Interface (JNI) Completeness

### J1. JNI 1.6+ full conformance
- J1.1 All 234 functions non-stubbed.
- J1.2 `RegisterNatives` / `UnregisterNatives` for dynamic native binding.
- J1.3 Critical sections (`GetPrimitiveArrayCritical`, `GetStringCritical`).
- J1.4 Weak global refs lifecycle.
- J1.5 Local ref frame management (`PushLocalFrame`/`PopLocalFrame`).
- J1.6 Exception transition rules (pending exception checks).

### J2. JNI invocation API
- J2.1 `JNI_CreateJavaVM`, `JNI_GetCreatedJavaVMs`, `AttachCurrentThread`, `DetachCurrentThread`.
- J2.2 Allow C/C++ apps to embed CratonVM.

### J3. `System.loadLibrary` / `System.load`
- J3.1 Real `dlopen`/`LoadLibrary`, symbol lookup.
- J3.2 `JNI_OnLoad` / `JNI_OnUnload`.
- J3.3 Smoke test: load `libsqlite-jdbc`, `lwjgl`, `netty-tcnative`.

### J4. Panama FFI (`java.lang.foreign`) completeness
- J4.1 Downcalls > 8 args (currently capped).
- J4.2 Variadic downcalls.
- J4.3 Struct-by-value passing (System V + Win64 ABIs).
- J4.4 Upcalls with all signature types.
- J4.5 `MemorySegment` slicing, alignment, native arena lifetimes.
- J4.6 `Linker.nativeLinker()` real symbol resolution.

---

## PHASE K — Standard Library Coverage (real bytecode mode)

### K1. Drop synthetic stubs entirely
- K1.1 Remove `synthetic-jdk` feature flag default; default to real JDK.
- K1.2 Delete `create_synthetic_stub` path in `class_manager.rs:1034`.
- K1.3 Delete `crypto.rs` (156 KB synthetic) → replaced by Phase E real crypto.
- K1.4 Delete `jmx.rs` synthetic (80 KB) → replaced by Phase I4.
- K1.5 Delete `util_time.rs` (4,760 lines) → load real `java.time` from JDK.
- K1.6 Delete synthetic collections overrides → use real JDK bytecode.

### K2. Pass JDK regression-test subset
- K2.1 Run `jdk/test/java/lang` from openjdk/jdk repo against CratonVM.
- K2.2 Run `jdk/test/java/util`.
- K2.3 Run `jdk/test/java/io`, `java/nio`, `java/net`, `java/security`.
- K2.4 Run `jdk/test/java/util/concurrent`.
- K2.5 Track pass-rate weekly; gate releases on no regressions.

---

## PHASE L — GUI & Desktop (only if "any Java app" includes desktop)

### L1. AWT
- L1.1 `java.awt.Toolkit`, `Graphics2D`, `Font`, `Image`, `BufferedImage`.
- L1.2 Native peer for Win32 (GDI/Direct2D), X11, Cocoa.
- L1.3 Event dispatch thread.

### L2. Swing
- L2.1 All Swing components (renderer chain on top of AWT).
- L2.2 Look-and-feel (Metal, Nimbus, system).

### L3. JavaFX (out of scope — separate distribution).

### L4. java2d acceleration (OpenGL/Metal/Direct3D pipelines).

*Note: many server-side Java apps run headless; this phase can be deferred but is required for "any Java app".*

---

## PHASE M — Performance Parity with HotSpot C2

### M1. Interpreter optimizations
- M1.1 Resolve constant pool entries once at link time into `ResolvedConstantPool`; remove `class_manager.read()` from per-instruction path (eliminates 94+ lock acquisitions per execution path). **Expected: 30–60% interpreter speedup.**
- M1.2 Threaded / computed-goto dispatch (function-per-opcode + tail calls) instead of giant `match`.
- M1.3 Per-thread method-cache (mcache) for `invokevirtual` / `invokeinterface`.
- M1.4 Cache native `fn` pointer in CP entry — eliminate per-call FNV-1a hashing.

### M2. JIT optimizations
- M2.1 Polymorphic inline caches (PIC, 2–4 entries) at JIT level.
- M2.2 Wire profile data from `tiered.rs` into the C2 emitter (currently collected, unused).
- M2.3 Constant folding, local CSE, GVN.
- M2.4 Graph-coloring register allocator (replace greedy fixed-locals scheme).
- M2.5 Method inlining heuristics tuned with PGO.
- M2.6 Loop unrolling driven by profile counts.
- M2.7 Vectorization (auto-SIMD beyond array-sum pattern).
- M2.8 Lock elision for thread-local objects (already partial via escape analysis).
- M2.9 Range-check elimination beyond constant indexes.
- M2.10 Devirtualization via class hierarchy analysis (CHA) + dependency invalidation.
- M2.11 String concatenation peephole.
- M2.12 Math intrinsics: `Math.fma`, `Math.exp`, `Math.log`, `Math.sin/cos` (currently only `sqrt`).

### M3. GC parity
- M3.1 Real **G1** (currently has 98 KB skeleton): SATB, refinement threads, mixed GC, concurrent root scan, string dedup.
- M3.2 Real **ZGC** (currently 45 KB skeleton): colored pointers, load barriers, concurrent reloc.
- M3.3 **Shenandoah** option.
- M3.4 **Parallel GC** for batch workloads.
- M3.5 TLAB per-core free lists (drop shared lock on refill).
- M3.6 Compressed oops (32-bit refs in young gen, 8 GB heap with shift=3).
- M3.7 Right-size field slots: 4-byte for int/float/short/byte/bool/char, 8-byte for long/double/oop. (Currently uniform 16 bytes — ~50% memory waste.)
- M3.8 Object header → 12 bytes (mark word + klass ptr) like HotSpot.
- M3.9 Card table coarsening, SATB queues per thread.
- M3.10 Bias-locking removal (already removed in HotSpot 21+); use lightweight locking.

### M4. Startup & footprint
- M4.1 AppCDS (already partial via Phase 94) — extend to dynamic CDS, archived heap objects.
- M4.2 GraalVM-style **AOT** with full method compilation (already partial).
- M4.3 Class data sharing across JVM instances.
- M4.4 Lazy class init (already done) + lazy method linking.

### M5. Benchmark gates
- M5.1 SPECjvm2008: within 1.5x HotSpot C2 geomean.
- M5.2 Renaissance: within 1.5x.
- M5.3 DaCapo: within 1.5x.
- M5.4 Binary Trees: from 23x → ≤2x.
- M5.5 N-Body: from 9x → ≤1.5x.
- M5.6 CI runs full suite per release; regression alerts.

---

## PHASE N — Tooling & Ecosystem

### N1. Command-line tools
- N1.1 `java`, `javac`, `javap`, `jar`, `jlink`, `jmod`, `jdeps`, `jdeprscan`, `jpackage`.
- N1.2 `jshell` (REPL).
- N1.3 `jcmd`, `jinfo`, `jstat`, `jmap`, `jstack`, `jhsdb`.
- N1.4 `keytool`, `jarsigner`, `rmiregistry`.

### N2. Build tool compatibility
- N2.1 Maven 3.9+ runs unmodified.
- N2.2 Gradle 8.x runs unmodified (Kotlin DSL too).
- N2.3 Bazel rules_jvm compatibility.
- N2.4 sbt smoke test.

### N3. IDE integration
- N3.1 IntelliJ IDEA debugger via JDWP (Phase I2).
- N3.2 Eclipse JDT debugger.
- N3.3 VS Code Java extension.
- N3.4 NetBeans.

### N4. Profiler compatibility
- N4.1 async-profiler (uses JVMTI + AsyncGetCallTrace).
- N4.2 `AsyncGetCallTrace` / `AsyncGetStackTrace` (JEP 435).
- N4.3 JProfiler, YourKit (JVMTI agents).
- N4.4 perf-map agent for Linux `perf`.

### N5. Containers
- N5.1 cgroup v1/v2 awareness (`-XX:+UseContainerSupport` semantics).
- N5.2 Detect container memory/CPU limits.
- N5.3 jlink minimal images run in distroless containers.

---

## PHASE O — Security & Sandboxing

### O1. SecurityManager (deprecated but still required for legacy)
- O1.1 `AccessController.doPrivileged` semantics.
- O1.2 `Permission`, `PermissionCollection`, `Policy`.
- O1.3 `ProtectionDomain`, code source.
- O1.4 Note: SM is removed for removal (JEP 486); minimal compat layer only.

### O2. Module-based encapsulation enforcement (Phase B2 already covers this).

### O3. Signed code verification (Phase C4.2).

### O4. Address-space layout
- O4.1 W^X for JIT code cache.
- O4.2 ASLR-friendly relocations.
- O4.3 Stack canaries / shadow stack on supported HW.

---

## PHASE P — Conformance & Quality Gates

### P1. TCK / JCK
- P1.1 Acquire JCK 25 license (Oracle / OpenJDK partnership).
- P1.2 Pass `java.base` JCK in full.
- P1.3 Pass `java.desktop`, `java.net.http`, `java.sql`, `java.security`, `java.management`, `jdk.jfr`, `jdk.jdi` JCK suites.
- P1.4 Pass JVM TCK (instruction-level conformance).
- P1.5 Publish conformance report.

### P2. Real-app smoke matrix (CI gate)
- P2.1 Spring Boot 3.x petclinic boots, serves HTTP, connects to PostgreSQL.
- P2.2 Hibernate ORM tutorial passes.
- P2.3 Tomcat 10 serves a WAR.
- P2.4 Netty echo server.
- P2.5 Jackson databind round-trip on 100 representative POJOs.
- P2.6 Kafka client producer/consumer.
- P2.7 Cassandra Java driver smoke.
- P2.8 Elasticsearch client.
- P2.9 Jenkins boots far enough to show login page.
- P2.10 IntelliJ IDEA Community boots to project window.
- P2.11 javac (the OpenJDK compiler) compiles itself under CratonVM.
- P2.12 jshell REPL session.
- P2.13 Maven `mvn clean package` on a multi-module project.
- P2.14 Gradle `./gradlew build` on a Kotlin project.

### P3. Fuzzing
- P3.1 Class-file fuzzer (already partial in `fuzz/`).
- P3.2 Bytecode fuzzer.
- P3.3 JIT differential fuzzer (interpreter vs JIT must agree).
- P3.4 Native API fuzzer.
- P3.5 OSS-Fuzz integration.

### P4. Code quality CI gates
- P4.1 `clippy::unwrap_used` / `expect_used` deny outside tests.
- P4.2 `cargo deny` for license / advisory checks.
- P4.3 `cargo audit` security advisories.
- P4.4 `miri` on `unsafe` blocks.
- P4.5 No file > 3000 lines (split `interpreter.rs` 8,871 → modules; `x64.rs` 15,497 → modules; `vm_exec.rs` 3,061).
- P4.6 No `panic!` in non-test code outside the 3 documented sites.

### P5. Documentation
- P5.1 Per-crate rustdoc complete.
- P5.2 User guide: CLI flags, performance tuning, container support.
- P5.3 Embedder guide: JNI invocation API.
- P5.4 Migration guide: HotSpot → CratonVM.

---

## PHASE Q — Hardening & Operations

### Q1. Crash diagnostics
- Q1.1 hs_err-style crash log on panic / segv.
- Q1.2 Core dump support, GDB/LLDB pretty-printers.
- Q1.3 Java stack trace on native crash.

### Q2. Heap dump
- Q2.1 hprof binary writer.
- Q2.2 `-XX:+HeapDumpOnOutOfMemoryError`.
- Q2.3 Eclipse MAT compatibility.

### Q3. Signal handling
- Q3.1 SIGQUIT → thread dump (Linux/macOS), Ctrl+Break (Windows).
- Q3.2 SIGSEGV → recover from JIT NPE, dispatch to Java exception handler (HotSpot does this).
- Q3.3 SIGBUS, SIGILL → diagnostic.

### Q4. Resource limits
- Q4.1 `-Xmx`, `-Xms`, `-Xss` honored.
- Q4.2 `-XX:MaxMetaspaceSize`, `-XX:ReservedCodeCacheSize`.
- Q4.3 OOM handling without VM crash.

### Q5. Long-running stability
- Q5.1 30-day soak test on real workloads.
- Q5.2 No memory leaks (`valgrind`, leak sanitizer).
- Q5.3 No FD leaks.
- Q5.4 No code-cache exhaustion.

---

## PHASE R — Workspace Hygiene (parallelizable, do anytime)

- R1. Delete stray build dirs at repo root: `target2`, `target3`, `target4`, `target5`, `target_bench`, `target_m6`, `target_p86`, `target_p90`, `target_p94`, `target_p96`, `target_p97`, `target_p97a`, `target_p97b`.
- R2. Delete stray logs: `nbody_debug.txt`, `nbody_stderr.txt`, `aot_test.txt`, `test_output.txt`, `C:cratoncratonvmtest_output.txt`.
- R3. `.gitignore` covers all `target*` patterns.
- R4. Split monolithic files (P4.5).
- R5. Re-enable any tests currently in skip lists.
- R6. Replace `panic!` on hash collision in `native-api/src/registry.rs:601` with rehash-on-startup.

---

## Dependency Order (TL;DR)

```
A (Correctness Foundations)
 ├─> B (JPMS) ──> C (ClassLoaders) ──> K (drop synthetic stdlib)
 ├─> D (Networking) ──> E (Crypto/TLS) ──> F (JDBC) ──> P2 (real apps)
 ├─> G (Concurrency) ──> needs A2 (iterative interpreter)
 ├─> H (NIO.2) ──> needed by D3, G1.4
 ├─> I (Serviceability) ──> needs A1 (GC maps for JIT)
 ├─> J (JNI/Panama) ──> independent
 ├─> M (Performance) ──> needs A1, A2
 ├─> L (GUI) ──> optional, needed for "any app"
 ├─> N (Tooling) ──> needs B, C, I
 ├─> O (Security) ──> needs B, C, E
 └─> Q (Hardening) ──> continuous

P (Conformance) is the final gate.
R (Hygiene) runs in parallel throughout.
```

## Newly Identified Gaps (added 2026-04-14, after NEW-1..NEW-10)

These items were surfaced during the NEW-1..NEW-10 execution and are not
covered (or are materially understated) by the A..R phases. Ordered by
impact on the "any Java app" definition.

### NEW-11. Finish flipping `synthetic-jdk` default OFF ✅ DELIVERED

The synthetic-jdk feature is **no longer in the default feature set** of
`vm/Cargo.toml` as of NEW-11's execution. The default build boots
against real JDK bytecode loaded from `$JAVA_HOME/lib/modules` (via the
NEW-5 jimage reader) with the ~150 essential natives as the Rust
support surface. To run the legacy synthetic test suite or to debug
against Rust-side stubs, pass `--features synthetic-jdk` explicitly.

**Results:**
- Default `cargo test -p cratonvm-vm --lib`: **1237 passed, 0 failed,
  110 ignored** (was 931 failed before the flip).
- Opt-in `cargo check -p cratonvm-vm --features synthetic-jdk`: clean.
- The inline `mod tests` in `vm.rs` (the 2857-test synthetic-era
  module) is gated behind `#[cfg(all(test, feature = "synthetic-jdk"))]`
  so it runs only when the feature is explicitly enabled.
- 4 self-inconsistent tests in `vm_init.rs` and `native/jni.rs` that
  relied on the synthetic mod's side effects were either feature-gated
  (two `PrintStream`/`native-methods` count tests) or fixed to be
  self-contained (two JNI tests that wrongly used a detached
  `JniGlobalRefs` instead of `shared.jni_global_refs`).
- One pre-existing race in `jit::conservative_roots::tests::global_depth_tracks_pushes`
  that asserted equality on the process-wide `GLOBAL_JIT_DEPTH` counter
  was fixed by switching to the race-free thread-local counter.

The NEW-11 deliverable is binary — the default test run is green with
the feature off. The follow-up work (reducing the gated test count by
promoting synthetic tests to feature-independent tests) is tracked
separately and does not block any other roadmap item.

### NEW-12. Precise JIT oop maps — infrastructure delivered, compiler populator pending

NEW-1 shipped a conservative JIT-frame root scanner that pins every
plausible heap-pointer-looking qword in a JIT spill region during GC.
Production-safe but prevents compaction while any JIT frame is on the
stack.

**This session delivered the full NEW-12 infrastructure:**

1. **`cratonvm_jit::OopMapEntry`** — a `{ native_pc_offset: u32,
   frame_slot_offsets: Vec<i16> }` record enumerating the frame slots
   that hold live oops at a specific safepoint. Stored on
   `CompiledMethod::oop_maps: Vec<OopMapEntry>` with
   `push_oop_map` and `find_oop_map_for_pc` helpers. The latter
   lazy-sorts on first lookup so out-of-order pushes work. New
   `has_precise_oop_maps()` method reports coverage status.

2. **Per-thread `JIT_ENTRY_CHAIN`** extended from a bare `Vec<usize>`
   to `Vec<JitFrameChainEntry>`, where each entry carries both the
   conservative `entry_sp` and an optional `PreciseFrameInfo
   { compiled_method, frame_base, entry_ptr }` pointing at the active
   `CompiledMethod`. The conservative API surface (`push_jit_entry_at`,
   `pop_jit_entry`, `JitEntryGuard::enter`) is preserved bit-for-bit.

3. **`JitEntryGuard::enter_with_compiled(&CompiledMethod)`** — new
   opt-in API that registers precise-frame metadata alongside the SP
   capture. Falls back transparently to the conservative guard when
   `cm.has_precise_oop_maps() == false`, so callers can always use
   this entry point regardless of whether the compiler has populated
   maps for the target method yet.

4. **`scan_one_frame_precise`** — new root walker path that reads
   each oop slot listed in every recorded map (validated via
   `heap.is_object_address` for safety), complementing the NEW-1
   conservative fallback. `scan_active_jit_frames_with_sp` dispatches
   per-entry: precise if `entry.precise.is_some()`, conservative
   otherwise.

5. **`scan_oop_slots(frame_base, offsets, heap, out)`** — the
   low-level oop-enumeration helper. Validates alignment defensively
   and runs each slot through `heap.is_object_address` before adding
   to the root set. False positives (a non-oop i64 happening to match
   a header address) are filtered; false negatives are impossible so
   long as the compiler emits accurate maps.

**Tests (6, all passing):**
- `new12_oop_map_entry_basics` — constructor + slot count
- `new12_find_oop_map_for_pc_handles_unsorted_input` — out-of-order
  push + binary-search lookup
- `new12_enter_with_compiled_empty_maps_falls_back_to_conservative`
- `new12_enter_with_compiled_with_maps_registers_precise`
- `new12_scan_oop_slots_filters_via_heap_validation` — real
  `VmHeap::alloc_object` round-trip through a hand-built frame layout
- `new12_scan_oop_slots_skips_unaligned_offsets`

**What's NOT done (the populator):**
The JIT compiler itself does not yet call `cm.push_oop_map(...)`
during codegen. For that, `x64.rs::SimulatedStack` needs to track
`SlotKind::{Reference, NonReference}` in parallel with the existing
slot state and snapshot the live reference set at every safepoint
(new, newarray, anewarray, and any invoke of a callee that may
allocate). That is a ~500-call-site mechanical refactor that did not
fit the single-session scope; it is the sole remaining piece of
NEW-12 and is tracked as NEW-12.bis below.

**With the infrastructure in place**, compaction-while-JIT-active
remains blocked on the populator landing — `gc_quiescence::is_active()`
still defers compaction whenever any JIT frame is on the stack. Once
every active frame has `precise = Some(_)`, a follow-up change can
lift the defer by checking `all_entries_precise()` in the GC
preamble. The infrastructure for that check is already in place.

### NEW-12.bis. Populate oop maps from the JIT compiler

- NEW-12.bis.1 Add `SlotKind::{Int, Long, Float, Double, Reference,
  ReturnAddress}` tracking to `x64.rs::SimulatedStack`.
- NEW-12.bis.2 Tag every stack-producing instruction with its slot
  kind (pop/push wrappers auto-derive from descriptor types for
  method return values).
- NEW-12.bis.3 At every safepoint (new, newarray, anewarray, and every
  invoke that may allocate), snapshot the active `Reference` slots
  into an `OopMapEntry` with the current `native_pc_offset` and
  attach via `cm.push_oop_map`.
- NEW-12.bis.4 Lift the `gc_quiescence::is_active()` defer once every
  active chain entry reports precise coverage.
- **Definition of done**: a benchmark that allocates ≥ 1 GB from
  inside a single JIT-compiled method completes without OOM with
  compaction observably running.

### NEW-12. Precise JIT oop maps (replace NEW-1's conservative root scan)

NEW-1 shipped a conservative JIT-frame root scanner that pins every
plausible heap-pointer-looking qword in a JIT spill region during GC.
This is production-safe but prevents compaction for the duration of any
JIT call, which limits allocation-heavy workloads.

- NEW-12.1 Emit per-safepoint oop maps at JIT compile time (one bitmap
  per safepoint, N bits where N = spill slot count; bit i set ⇔ slot i
  holds a live oop).
- NEW-12.2 GC root walker consults the oop map instead of the
  conservative range-scan when the frame in question is a JIT frame.
- NEW-12.3 Allow compaction to proceed with JIT frames on the stack —
  relocate oops in spill slots as the relocation table dictates.
- NEW-12.4 Remove the `gc_quiescence::is_active()` check from
  `gen_heap::collect_garbage_inner`. Remove `JitEntryGuard`'s fallback
  comment about deferring GC.
- **Definition of done**: a benchmark that allocates ~1 GB inside a
  single JIT-compiled method completes without OOM under the default
  heap, with compaction observably running.

### NEW-13. Real `javax.net.ssl` (deshadow the TLS feature flag)

Phase E and NEW-2 established that TLS works via `native-tls` but was
gated behind `experimental-tls` and shadowed by `phases_late.rs::register_p68_ssl`.

- NEW-13.1 **DONE.** `native-tls` is compiled and registered unconditionally,
  so the flag gates nothing. Renamed `deprecated-noop-tls` on 2026-07-30 to say
  so out loud, with `experimental-tls` kept as a back-compat alias; both are
  slated for removal in 0.4.
- NEW-13.2 Implement `SSLContext.init` to actually wire a
  `TrustManagerFactory` / `KeyManagerFactory` into a real `native_tls::TlsConnector`
  (not the current placeholder that stores field values).
- NEW-13.3 `SSLSocketFactory.createSocket` returns a real wrapped
  `SSLSocket` backed by the same `s2_registry` as plain `TcpStream`.
- NEW-13.4 `SSLSession.getPeerCertificates` returns a real `X509Certificate`
  chain extracted from the `native-tls` handshake.
- **Definition of done**: `curl`-style HTTPS GET against `httpbin.org`
  via `HttpClient.send` returns a 200 with the actual body.

### NEW-14. JDBC `java.sql` real impl (Phase F completion) ✅ DELIVERED (subset)

**What's real now (2026-04-15 session):**

- `java.sql.DriverManager.getConnection(url)` opens real
  `rusqlite::Connection` instances for every `jdbc:sqlite:*` URL,
  with path-traversal rejection and error-message sanitization.
- `registerDriver` / `deregisterDriver` / `getDrivers` track a
  process-wide registry of driver class names (NEW-14.1 — native
  fallback path; the full JDBC 4.0 `ServiceLoader` path kicks in
  when a pure-Java driver registers itself from its static
  initializer).
- `Connection.createStatement`, `prepareStatement`, `prepareCall`,
  `setAutoCommit`, `commit`, `rollback`, `rollback(Savepoint)`,
  `setTransactionIsolation`, `getMetaData`, `createBlob`,
  `createClob`, `setSavepoint`, `setSavepoint(String)`,
  `releaseSavepoint`, `close`, `isClosed`, `isValid`.
- `Statement.executeUpdate`, `executeQuery`, `execute`, `close`,
  `getResultSet`, `setMaxRows`, `setQueryTimeout`.
- `PreparedStatement` with param binding for String / Int / Long /
  Double / Float / Boolean / Null / Bytes / Object;
  `executeUpdate` / `executeQuery` / `execute` / `addBatch` /
  `executeBatch` / `clearBatch` / `clearParameters` / `close`.
- `CallableStatement` (NEW-14 additions): allocated with the same
  3-field shape as `PreparedStatement` so every PS method dispatches
  via the new `NativeMethodRegistry::alias_class` helper that copies
  PreparedStatement's entire registration table under the
  CallableStatement class name; `registerOutParameter` variants
  (int-int, int-int-int, int-String, String-int) recorded as no-ops
  (spec-legal) since the rusqlite backend does not expose true
  stored-procedure OUT params.
- `ResultSet.next`, `getString`/`getInt`/`getLong`/`getDouble`/`getFloat`/
  `getBoolean`/`getObject`/`getBytes` by column index and name,
  `wasNull`, metadata.
- `ResultSetMetaData.getColumnCount`/`getColumnName`/`getColumnLabel`/
  `getColumnType`/`getColumnTypeName`/`isNullable`.
- **`java.sql.Blob` (NEW-14.N2)**: real storage in
  `jdbc_registry::new14::blobs` keyed by opaque id. Methods
  `length`, `getBytes(pos, len)`, `setBytes(pos, [B)`, `truncate`,
  `free`. All bounds-checked; out-of-range accesses clip, never panic.
- **`java.sql.Clob` (NEW-14.N2)**: same treatment backed by
  `String`. `length`, `getSubString(pos, len)`, `setString(pos, String)`,
  `truncate`, `free`.
- **`java.sql.Savepoint` (NEW-14.N3)**: real SQL
  `SAVEPOINT name` / `ROLLBACK TO SAVEPOINT name` / `RELEASE SAVEPOINT name`.
  Names are validated (`[A-Za-z0-9_]+`) to block SQL injection; the
  `savepoint_rejects_injected_name` test confirms `"sp; DROP TABLE t; --"`
  is rejected with a typed error.
- **`java.sql.DatabaseMetaData` (NEW-14.N4)**: real
  `getDatabaseProductName` → `"SQLite"`,
  `getDatabaseProductVersion` → `rusqlite::version()`,
  `getDriverName` → `"CratonVM JDBC"`, `getDriverVersion`,
  `getDriverMajorVersion`/`MinorVersion`, `getURL`, `getUserName`,
  `supportsTransactions`, `supportsSavepoints`, `supportsBatchUpdates`.

**Tests**: 10 end-to-end tests in `phases_late::new14_jdbc_tests`
covering open/close, DDL+DML+SELECT round-trips on real in-memory
SQLite, prepared-statement binding, rollback-discards-changes,
commit-preserves-changes, Blob round-trip, Clob round-trip,
Savepoint rollback-and-release, Savepoint name-injection rejection,
driver registry idempotence, database-metadata values. All 10 pass.

**Out of scope for this session (honestly tracked):**
- **NEW-14.4 "H2 loads and runs"**: the H2 JDBC driver is ~2 MB of
  pure-Java bytecode that exercises hundreds of JDK classes
  transitively. Loading H2 end-to-end requires NEW-11 to be fully
  green (already done — synthetic-jdk is OFF by default), NEW-5's
  jimage reader to serve every `java.*` class correctly (done), AND
  every `java.util.concurrent.*` / `java.nio.*` / `java.lang.reflect.*`
  corner case H2 touches to work in non-synthetic mode. That is a
  cross-cutting integration task that cannot be delivered as a
  single JDBC session. The infrastructure is now in place; the
  remaining gap is the non-synthetic JDK completeness.
- **True stored-procedure OUT parameters**: rusqlite/SQLite don't
  expose the underlying facility. `CallableStatement.registerOutParameter`
  is recorded but `CallableStatement.getXxx(int)` falls through to
  `PreparedStatement`'s generic column getter.
- **`java.sql.Types.REF_CURSOR`**, `SQLXML`, `Array`, `Struct`,
  `RowId`, `NClob` — not backed by any real storage yet. Registering
  them is mechanical and can be added on demand.

### NEW-15. Virtual threads (JEP 444 / Loom)

The roadmap's Phase G1 mentions virtual threads. Current state: none
of the Loom continuation primitives are implemented; `Thread.startVirtualThread`
falls back to a platform thread.

- NEW-15.1 Continuation primitive (`jdk.internal.vm.Continuation.yield`
  freezes the current stack into a growable buffer; `run` thaws it).
- NEW-15.2 Integrate with the iterative interpreter (Phase A2) so a
  yielded continuation can be resumed on a different carrier thread.
- NEW-15.3 ForkJoinPool as the default virtual-thread scheduler.
- NEW-15.4 Yield on blocking I/O (NIO / HTTP client) — integrate with
  NEW-3's Selector via `LockSupport.park` hooks.
- NEW-15.5 Pinning detection + `VirtualThreadPinned` JFR event.
- **Definition of done**: spawn 100k virtual threads each doing a
  `Thread.sleep(100ms)` and confirm they all complete in < 2s on a
  4-core machine (would take 100k seconds on platform threads).

### NEW-16. JCK `java.base` conformance pass

Phase P1 lists JCK acquisition as a dependency. Even before acquiring
a license, we can run the openjdk/jdk `test/jdk/java/base/` regression
test corpus (which is public) against CratonVM and diff the failures.

- NEW-16.1 Clone openjdk/jdk at the JDK 25 tag, extract the
  `test/jdk/java/*` tree, and port the test runner (`jtreg`) glue to
  invoke `vm-cli` as the JVM-under-test.
- NEW-16.2 Run the subset `java.base/java.lang`, `java.util`,
  `java.io`, `java.nio`, `java.net`, `java.security`.
- NEW-16.3 Commit a baseline pass-rate report to `docs/jdk-regression-baseline.md`.
- NEW-16.4 Gate release candidates on "pass rate does not regress".
- **Definition of done**: baseline report committed, first pass-rate
  floor set at whatever the first run produces (no specific target
  number — we're measuring and improving from there).

### NEW-17. `Cleaner` / `PhantomReference` / `finalize` modernization ✅ DELIVERED

- NEW-17.1 `Cleaner.create()` returns a real Cleaner with a backing array
  pinning Cleanables alive (`native-builtins/src/phases_late.rs::register_p69_cleaner`).
- NEW-17.2 `Cleaner.register(obj, Runnable)` allocates a `Cleaner$Cleanable`,
  appends it into the Cleaner's backing array, and registers it as a
  `ReferenceType::Cleaner` phantom of `obj` via
  `NativeContext::discover_reference(3, cleanable, obj, None)`.
- NEW-17.3 GC integration: `process_references_after_gc` already submits
  cleared cleaner actions to `SharedVm::cleaner_thread`; new
  `interpreter::run_cleaner_actions` drains them and dispatches `run()V`
  on each Runnable via `invoke_shared`. Hooked into `force_gc_from_native`.
- **Definition of done — met**: `DirectByteBuffer.allocateDirect(int)` now
  takes a real allocation from `NativeMemoryTable`, registers a
  `DirectBufferDeallocator.run()V` cleaner that calls `free_native_memory`,
  and the test `new17_direct_byte_buffer_releases_native_memory_on_cleaner_drain`
  proves that simulated GC + cleaner drain returns `live_count` to its
  pre-allocation value.

5 unit tests (`vm/src/vm.rs::tests::new17_*`) cover Cleaner.create init,
register-discovers-phantom, native memory accounting, GC-driven release,
and `Cleanable.clean()` idempotency. All pass under `--features synthetic-jdk`.

### NEW-18. `java.lang.foreign.Linker` upcall completeness (Panama) ✅ DELIVERED

The previous `pe_downcall_invoke` was a hand-rolled cascade of
`extern "C" fn(u64, u64, ...)` transmutes capped at 8 args, and it
treated every argument as a 64-bit integer — meaning floats and
doubles landed in integer registers and any signature past 8 args
threw `IllegalStateException`. Upcalls technically worked but only
through the Java-side `UpcallStub.invoke([Object])` path; the
`_upcall_t00..63` trampoline pool unconditionally returned 0 when
called from real C code.

NEW-18 replaces the entire dispatcher with a `libffi`-backed pipeline:

- **NEW-18.1 — Arbitrary arity.** `pe_downcall_invoke` builds a
  `libffi::middle::Cif` from the FunctionDescriptor and calls
  `libffi::raw::ffi_call` directly. No more 8-arg cap. Verified by
  `new18_downcall_arity_12_ints`.
- **NEW-18.2 — Variadic.** A new field 2 on the DowncallHandle
  synthetic carries the first-variadic-arg index (-1 = non-variadic).
  `Linker.downcallHandle(addr, desc, Linker$Option[])` reads
  `Linker.Option.firstVariadicArg(int)` from the option array. When
  set, the dispatcher uses `low::prep_cif_var` instead of
  `Cif::new`. Verified by `new18_downcall_variadic_snprintf` calling
  real libc `snprintf("%d", 42)` and asserting both the return value
  (2) and buffer contents ("42\0").
- **NEW-18.3 — Mixed int/float + struct-by-value.** libffi handles
  both register classification (System V vs Win64) and struct
  packing automatically. Floats land in `xmm`, ints in integer
  registers. Verified by `new18_downcall_mixed_int_float` calling
  `int + double + int + float` and getting the correct sum.
  Struct-by-value flows through `panama_libffi::layout_to_ffi_type`
  which recursively builds `FfiType::structure(fields)` from
  `StructLayout` synthetics, including padding materialization.
- **NEW-18.4 — Real upcalls.** The `_upcall_tNN` trampoline pool is
  deleted. `pe_upcall_handle` now builds a `libffi::middle::Closure`
  whose extern "C" code pointer dispatches to `upcall_dispatch`,
  which fetches the active `NativeContext` from a per-thread
  `ActiveContextGuard` (installed automatically at the start of every
  downcall) and calls `invoke_virtual` on the Java target. Closures
  are kept alive in a global registry keyed by trampoline address.
  Verified by `new18_upcall_libffi_closure_dispatches_to_java` which
  calls a real libffi-generated extern "C" function pointer with
  `(11, 22)` from Rust and observes the mock VM's `invoke_virtual`
  return value (123) propagate back as the C return.
- **NEW-18.5 — MemorySegment / arena.** Already complete in
  Phase J4: arenas, slicing, and the `NativeMemoryTable` lifecycle
  remain unchanged.

**Definition of done — met:** every original DoD failure mode
(arity > 8, mixed int/float, variadic, real C-callable upcall) now
has a passing test driving the real libffi pipeline.

Files: `native-builtins/src/panama_libffi.rs` (NEW — layout↔Type
bridge, marshaling, thread-local active-context guard),
`native-builtins/src/panama.rs` (rewrote `pe_downcall_invoke`,
`pe_upcall_handle`, deleted trampoline pool, added Linker.Option
plumbing), `native-builtins/Cargo.toml` (`libffi = "3.2"`).
Tests: `panama::tests::new18_*` (5/5 passing), all 40 pre-existing
panama tests still pass.

### NEW-19. Module-layer `opens` / `exports` enforcement

Phase B2 exists. Current: module info is parsed but `exports`/`opens`
accessibility is not enforced for the majority of reflection sites.

- NEW-19.1 Thread the lookup class's module through every `Method.invoke`,
  `Field.get/set`, `Constructor.newInstance` call.
- NEW-19.2 Enforce `IllegalAccessException` when reflecting into a
  non-`opens` package from a different module.
- NEW-19.3 `--add-exports` / `--add-opens` CLI flags (already parsed)
  actually grant the access.
- **Definition of done**: a test that tries to `setAccessible(true)` on
  `java.lang.String.value` from an unnamed-module caller fails with
  `InaccessibleObjectException` on modular JDK 25, matching HotSpot.

### NEW-20. Benchmark gates for the JIT ✅ DELIVERED

A complete, framework-light regression gate now sits in front of every
push and every PR that touches `vm/`, `jit/`, `gc/`, or
`native-builtins/`.

- **NEW-20.2 — Comparator + threshold.** `vm/src/bin/bench_gate.rs` is
  a real CLI binary that reads `target/criterion/<bench>/<id>/new/estimates.json`,
  loads `bench/baseline.json`, computes per-metric `Δ%` against the
  configured threshold (default 15%), prints a table, and exits
  non-zero on regression. Bootstraps cleanly when the baseline is
  zero. Geometric mean of all per-metric ratios is also gated. 20
  unit tests cover the comparator, parser, CLI, and baseline I/O —
  no criterion run required to test the gate.
- **NEW-20.4 — N-Body + Binary Trees.** Two new criterion bench
  groups (`shootout_nbody`, `shootout_binary_trees`) added to
  `vm/benches/vm_benchmarks.rs`. Both kernels are hand-built integer
  bytecode (no `javac` dependency) and execute through the real
  CratonVM interpreter, so they reflect actual JVM throughput.
- **CI gate.** `.github/workflows/bench-gate.yml` runs
  `cargo bench --bench vm_benchmarks --no-fail-fast -- --quick`,
  invokes `bench-gate --report bench/last_run.json`, and uploads the
  report as a workflow artifact. Triggered on push to
  `main`/`release/*` and on PRs touching the perf-sensitive crates.
- **Documentation.** `bench/README.md` covers bootstrapping a new
  baseline, tuning the threshold, adding a benchmark, and exit codes.

Per the comparator's exit-code contract: 0 = pass, 1 = regression, 2
= baseline missing/corrupt, 3 = no criterion data, 4 = bad CLI args.

**NEW-20.1 / NEW-20.3 — SPECjvm2008 / DaCapo hookup deferred.** Those
external suites depend on the full JDK class loader, the JIT skip
list being empty (NEW-1.1–1.5), and ~100 MB of binary data we should
not vendor. The *gate framework* delivered here generalizes to any
criterion bench, so wiring SPECjvm2008 and DaCapo becomes a 20-line
addition once those prerequisites land. Tracked under NEW-1.6.

**Definition of done — met:** the gate publishes a stable, machine
readable JSON report per run, prints a human-readable table with
geomean delta, and is enforced by CI. Try it locally:

```bash
cargo bench --bench vm_benchmarks
cargo run --release --bin bench-gate
```

---

## JVM 25 Readiness Assessment (2026-04-15, after NEW-14..NEW-18 + NEW-20)

**Overall readiness: ~38%** (up from ~33% post NEW-1..10).

### What moved the number

| Phase | Delivered | Readiness contribution |
|---|---|---|
| **NEW-14** ✅ | Real JDBC against `rusqlite`: Statement, PreparedStatement, CallableStatement, ResultSet, Blob, Clob, Savepoint, DatabaseMetaData, DriverManager. End-to-end TckJdbc bytecode tests (11/11). Fixed Enumeration natives + DriverManager class-name resolution as collateral. | +1.5% — closes the "no real database" gap for any app that talks to SQLite-shaped storage. Also unblocks every Spring Boot starter that pulls in JDBC. |
| **NEW-17** ✅ | Real `Cleaner` / `PhantomReference` / `Cleaner.register` wired through `ReferenceProcessor` → `CleanerThread` → `invoke_shared`. `DirectByteBuffer.allocateDirect` now allocates real native memory and registers a deallocator that runs on phantom death. 5 unit tests including the DoD: native-memory accounting returns to baseline after GC + cleaner drain. | +1.5% — Netty, MapDB, every off-heap framework needs this. |
| **NEW-18** ✅ | Panama Linker rewritten on `libffi`. Arbitrary-arity downcalls (was capped at 8), real ABI-correct mixed int/float register classification, variadic via `prep_cif_var` (validated with libc `snprintf("%d", 42)`), real upcalls via `libffi::middle::Closure` dispatching back into Java through a per-thread active-context guard. Old `_upcall_t00..63` trampoline pool deleted. 5 NEW-18 tests + all 40 pre-existing panama tests pass. | +1.5% — JEP 454 (the JNI replacement for new code) is now functional. |
| **NEW-20** ✅ | Real benchmark gate: `bench-gate` binary parses criterion JSON, compares vs `bench/baseline.json`, fails CI on > 15% regression. Geometric mean gating, bootstrap mode, JSON report. 20 unit tests. NBody + BinaryTrees integer kernels through the real interpreter. CI workflow `bench-gate.yml`. | +0.5% — does not enable new Java code paths but locks in everything else against silent regression. |
| **(NEW-19 deferred)** | Module accessibility enforcement is the only Phase B item between NEW-18 and the readiness target. Not yet started. | — |
| **Cumulative** | 33% → ~38% | +5% |

### What is still blocking the number

Ranked by how many "any Java app" failure modes each unblocks:

| Gap | Impact | What lands when fixed |
|---|---|---|
| **NEW-4 — flip `synthetic-jdk` off by default** | Highest. Real Spring Boot bootstrapping is gated on this. | Booting any app from a real JAR against real JDK 25 modules. |
| **NEW-1 — kill the residual JIT skip list** | High. JIT currently can't compile `java/util/*` or `<init>`/`<clinit>`, which is most of any real app. | 5–20× speedup on real workloads, unblocks SPECjvm2008/DaCapo via the gate framework already in place. |
| **NEW-19 — JPMS `opens`/`exports` enforcement** | Medium. Anything that does reflective `setAccessible(true)` against `java.base` private fields breaks correctly. | Real JDK access controls — required by Hibernate, Lombok, Mockito, Spring AOP. |
| **Phase E — real TLS/crypto outside the experimental flag** | Medium. HTTPS, JWT, signed JARs, X.509 chain validation. | Anything that talks to a TLS endpoint or verifies a signature. |
| **JCK conformance** (Phase P) | Medium. The gold-standard compatibility test. | "Officially Java SE 25" claim + every TCK-driven edge case. |
| **Phase L — AWT/Swing** | Low for headless apps, total blocker for desktop. | IntelliJ, NetBeans, Swing-based tools. |

### Component readiness matrix (refreshed 2026-04-15)

| Component | Status | Readiness |
|---|---|---|
| Class file reader (versions ≤ 69) | Complete | 90% |
| Bytecode interpreter | All standard opcodes, jsr/ret verifier-correct | 80% |
| JIT compiler (x86-64/AArch64) | Tiered, deopt, OSR, oop maps planned | 70% |
| GC (G1 / ZGC / generational) | NEW-17 cleaner integration done; finalize+phantom complete | 70% |
| Threading + JMM | Virtual threads (NEW-15), monitors, VarHandles, JMM partial | 55% |
| `java.lang.*` natives | Real JDK bytecode mode covers most; ~200 missing per NEW-10 census | 65% |
| `java.util.*` collections | 100% real bytecode mode under jimage; synthetic stubs deletable post NEW-4 | 60% |
| `java.time` / `java.math` | Real JDK bytecode mode | 60% |
| `java.io` / `java.nio` | Real Berkeley sockets (NEW-2), real Selector (NEW-3), real arenas (Phase J4 + NEW-17) | 70% |
| ClassLoading / Verification | jimage reader, hidden classes (NEW-8), jsr/ret verifier (NEW-9) | 65% |
| JNI | Globals, locals, native dispatch, AttachCurrentThread; missing: critical sections | 60% |
| Panama FFI (`java.lang.foreign`) | libffi-backed, downcalls/upcalls/variadic/struct-by-value (NEW-18) | 85% |
| `java.sql` JDBC | Real `rusqlite`-backed (NEW-14) | 65% |
| Module system (JPMS) | Parsed, but `opens`/`exports` not enforced (NEW-19 pending) | 35% |
| `invokedynamic` / MethodHandle | LambdaMetafactory, StringConcatFactory, condy | 55% |
| Crypto / TLS | TLS always compiled (its feature flag gates nothing); crypto partial | 30% |
| `java.lang.ref.*` | Soft/Weak/Phantom/Cleaner real (NEW-17), finalize integrated | 80% |
| JVMTI / JFR / JDWP | Frameworks present, event coverage partial | 35% |
| Bench / perf gates | NEW-20 delivered; SPECjvm hookup pending NEW-1 | 50% |
| **Bootstrap (real JDK class files)** | Default build boots against real JDK 25 class files (NEW-4 ✅) | 65% |
| AWT / Swing / GUI | Not started | 0% |
| JCK conformance (Phase P) | Not started | 0% |

The headline number is **~38%** because the unweighted average is now
~52%, but the weighting is dominated by the four bootstrap blockers
above (NEW-4, NEW-1 finish, real crypto, JCK), each of which
disqualifies entire classes of "any Java app" from running unmodified.

### Path to 50% (next major milestone)

1. **Finish NEW-4** — flip `synthetic-jdk` off by default. The
   missing-natives census from NEW-10 is the punch list.
2. **Finish NEW-1** — delete the residual JIT skip list. NEW-1.1 (oop
   maps) is the long pole.
3. **NEW-19** — JPMS accessibility enforcement.
4. **Phase E.1** — real `java.security.MessageDigest` / KeyStore /
   Provider out of the experimental flag.

These four together would close most of the bootstrap-blocker and
correctness-debt items and bring readiness into the high-40s.

### Path to 75% (Production Alpha)

1. **NEW-1.6 — SPECjvm2008 + DaCapo hookup** through the existing
   `bench-gate` framework.
2. **Real Spring Boot petclinic** boots and serves a request under
   the default feature set.
3. **JCK subset** for `java.base` and `java.net.http` passes.
4. **Phase E.2-E.4** — full TLS handshake, X.509 chain validation,
   PKCS#12 keystore.
5. **JFR event completeness** — every event class on the JDK 25 list
   emits real data instead of a placeholder.

### Path to 100% (Full JVM 25 Conformance)

1. **Complete JCK passage** for `java.base`, `java.desktop`,
   `java.net.http`, `java.sql`, `java.security`, `java.management`,
   `jdk.jfr`, `jdk.jdi`.
2. **Performance parity** — within 1.5× HotSpot C2 on SPECjvm2008
   geomean (the bench gate enforces no regressions during the climb).
3. **All JEPs** for JDK 25 implemented end-to-end.
4. **Phase L** — AWT/Swing if "any Java app" includes desktop.
5. **Phase Z** — deprecated-API cleanup (see below).

---

## Newly Identified Gaps (added 2026-04-14)

These are concrete, blocker-level items that surfaced when grounding the readiness assessment against the current code. Each one is *not* covered by the existing A–R phases or is materially understated there. They are ordered by impact on the "any Java app" definition.

### NEW-1. Static JIT skip list — ✅ DELIVERED (subset-complete)

After the T1 third+fourth closure passes:

- **NEW-1.1** — conservative JIT-frame root scanner + precise oop
  map infrastructure in `jit/src/lib.rs::OopMapEntry` + emission at
  every x64 safepoint (`new`, `newarray`, `anewarray`, `multianewarray`,
  `checkcast`, `instanceof`, `invoke*`, direct JIT-call dispatch).
  AArch64 backend carries the `oop_maps` field on
  `Arm64CompileResult` with the same data shape. The GC walker
  (`scan_one_frame_precise`) uses the precise map when present and
  backstops with a conservative sweep for gaps, so partial
  population is always correctness-safe. **✅**
- **NEW-1.2** — JIT instanceof / checkcast miscompilation fixed
  (load-on-demand class resolution + lambda-proxy fallback). CI
  gate in `vm/src/jit/skip_list.rs` forbids reintroduction. **✅**
- **NEW-1.3** — hash-table loop miscompile narrowed to three
  specific methods (`java/util/HashMap.put`/`get`/`resize`) on the
  `is_known_miscompile` targeted list. Every other `java/util/*`
  method is now JIT-eligible under the default Conservative
  policy. **✅ narrowed** (full fix tracked as a follow-up since
  it needs interactive JIT tracing).
- **NEW-1.4** — `<init>` / `<clinit>` blanket bans replaced with
  profile-driven exclusion via `classify_init_complexity` +
  `should_skip_jit_with_init`. Trivial constructors (the ~5-byte
  `aload_0; invokespecial; return` javac default) are now
  JIT-eligible. **✅**
- **NEW-1.5** — blanket `java/util/*` and `cratonvm/*` bans
  **deleted**. Only per-method entries remain in `is_known_miscompile`.
  CI gate tests (`java_util_unrelated_methods_now_jit_eligible_under_conservative`,
  `cratonvm_unrelated_methods_now_jit_eligible_under_conservative`)
  forbid reintroduction. **✅**
- **NEW-1.6** — SPECjvm2008 / DaCapo hookup tracked as a
  perf-gate follow-up. The bench-gate framework from NEW-20 can
  pick up external suites transparently once the miscompile on
  `exc_hierarchy` is fixed (committed reproducer at
  `vm/tests/tier1_tests.rs::t1_known_regalloc_miscompile_exc_hierarchy_reproducer`).
  **⏸ follow-up** — not blocking any correctness goal.

**Definition of done — substantially met:** the blanket bans are
gone, the targeted list covers exactly the documented miscompiles,
and the CI gate prevents regression. The remaining `exc_hierarchy`
reproducer is committed (as `#[ignore]`) so any future fix unblocks
the last two targeted entries automatically.

### NEW-2. Real Berkeley sockets exposed to `java.net.*`
There is no `vm/src/net` module and no real socket-backed natives in `native-io`. The only real `TcpListener::bind` calls live in `vm/src/debug/transport.rs` for JDWP. `java.net.Socket`, `ServerSocket`, `DatagramSocket` still go through synthetic stubs. Phase D1/D2 lists this but no work has started.
- NEW-2.1 New crate-internal module `native-io/src/net.rs` wrapping `std::net::{TcpListener, TcpStream, UdpSocket}` with `fd_table` integration (use the existing FD allocator).
- NEW-2.2 Native bindings: `Net.socket0`, `Net.bind0`, `Net.listen`, `Net.accept0`, `Net.connect0`, `Net.send0`, `Net.recv0`, `Net.close0`, `Net.setOption0`, `Net.getOption0`, `Net.localAddress`, `Net.remoteAddress`. Match the JDK's `sun.nio.ch.Net` surface so the real `java.net.Socket` JDK class can run on top unchanged.
- NEW-2.3 IPv4 + IPv6 dual stack via `SocketAddr::V4`/`V6`.
- NEW-2.4 Non-blocking mode, `SO_REUSEADDR`, `TCP_NODELAY`, `SO_KEEPALIVE`, `SO_LINGER`, `SO_RCVBUF`/`SO_SNDBUF` — all map onto the platform-native `setsockopt`.
- NEW-2.5 `InetAddress.getByName` real DNS via `std::net::ToSocketAddrs`.
- **Definition of done:** the existing `vm.rs` `socket_basics`, `server_socket_basics`, `socket_get_inet_address` tests pass against real OS sockets (currently they exercise the synthetic path). A new `nettcat`-style test connects two CratonVM processes over loopback.

### NEW-3. Real NIO Selector
No epoll/kqueue/IOCP backend exists. `native-io/src/lib.rs:9320` literally documents "On Linux/macOS a real implementation would use epoll/kqueue." Phase D3.2 lists this but no progress.
- NEW-3.1 Pick a portable layer: `mio` (de-facto Rust selector crate) or hand-rolled `polling`. Recommendation: `mio` — proven, BSD-licensed, used by `tokio`.
- NEW-3.2 Implement `sun.nio.ch.EPollSelectorImpl` / `KQueueSelectorImpl` / `WindowsSelectorImpl` natives backed by `mio::Poll`.
- NEW-3.3 Wire `SelectionKey`, `SelectableChannel.register`, `Selector.select`, `Selector.wakeup` through.
- NEW-3.4 Async file I/O (`AsynchronousFileChannel`) via `tokio_uring` on Linux, `io_uring` only when available; fall back to thread-pool emulation.
- **Definition of done:** a Java echo server using `ServerSocketChannel` + `Selector` accepts and echoes 1000 concurrent connections under CratonVM.

### NEW-4. ✅ DELIVERED — `synthetic-jdk` feature OFF by default
`vm/Cargo.toml` no longer includes `synthetic-jdk` in its default feature set (landed in session 54, NEW-11). The feature remains available as `--features synthetic-jdk` for hermetic test scenarios that need byte-identical synthetic outputs, but production builds boot against real OpenJDK 25 class files.

- NEW-4.1 ✅ CI job exists (T2.1.1, session 54).
- NEW-4.2 ✅ Census complete (T2.1.2–T2.1.4, session 54). Missing natives grouped by module in `docs/t2-census.md`.
- NEW-4.3 ✅ All missing natives implemented across T2.2–T2.9 (sessions 54–60). `java.lang.*` (30 items), `java.util.*` (20), `java.io/nio.*` (20), `java.time.*` (15), `java.security.*` (20), `javax.net.ssl.*` (20), `java.lang.invoke.*` (16), JNI (20).
- NEW-4.4 ✅ `default` in `vm/Cargo.toml` omits `synthetic-jdk` (NEW-11, session 43).
- NEW-4.5 Deferred — `util_time.rs` and `crypto.rs` retained as fallback paths gated behind `synthetic-jdk` / `legacy-synthetic-crypto`. Deletion is safe but not urgent; the feature-gate ensures they never compile on the default path.
- **Status:** The `synthetic-jdk` feature is documented in `vm/Cargo.toml` comments (lines 14–20) as "available for hermetic test scenarios; not for production use". `cargo test --workspace` passes without `synthetic-jdk` in the default feature set.

### NEW-5. jimage reader for `lib/modules`
Current class loading reads JMOD only (`grep "jmod"` hits class_manager / class_path / vm_init). The JDK distribution's runtime image at `$JAVA_HOME/lib/modules` is a **jimage** file, not a directory of jmods. Phase B1.3 lists this; no progress.
- NEW-5.1 New module `reader/src/jimage.rs` implementing the jimage v1 binary format (header, redirect table, attribute offsets, location string table, resource bytes).
- NEW-5.2 `JImageReader::open(path)` → memory-map the file, build the resource index.
- NEW-5.3 `JImageReader::read_class(module, name)` → returns the class bytes for `java/lang/String`, etc.
- NEW-5.4 Wire into `class_path.rs` so `--module-path` and the boot layer can resolve from `lib/modules`.
- NEW-5.5 Smoke test: run `vm-cli HelloWorld` against an unmodified `$JAVA_HOME/lib/modules` and confirm `java.lang.String` loads from jimage rather than from a jmod or synthetic path.
- **Definition of done:** a real Temurin/Adoptium 25 install boots the VM end-to-end through jimage with no jmod fallback.

### NEW-6. Bare `native_noop` audit completion (Phase R continuation)
Phase R closed crypto/http2/tls/serialization. 140 bare `native_noop` registrations remain in `lang_*`, `util_time`, `panama`, `vector_api`, `phases_early`, `phases_late`. Each one is either:
  (a) a method that genuinely should be a no-op for VM-managed objects → convert to `native_noop_with_this` with a one-line comment;
  (b) a method whose return type is non-void but `native_noop` returns `Ok(None)` → convert to `native_return_null` for typed null, or to a real return-value closure;
  (c) a method that should have a real implementation → fix it.
- NEW-6.1 Run `grep -n "native_noop\b" native-builtins/src/*.rs` and triage every hit into category (a/b/c).
- NEW-6.2 Apply (a) / (b) sweeps as Phase R already did for the four covered modules.
- NEW-6.3 Track (c) items as separate tickets — many will fall out of NEW-4 (real-JDK census).
- **Definition of done:** `grep -c "native_noop\b" native-builtins/src/*.rs` returns 0 for all category-(a)/(b) cases; remaining hits are intentional and documented.

### NEW-7. A3: panic/unwrap audit on the three hot files
`interpreter.rs`, `vm_exec.rs`, and `x64.rs` together still hold 166 `unwrap`/`expect`/`panic!` sites. A3 lists this but only points at 19 sites. The real surface is ~9× larger.
- NEW-7.1 `cargo clippy -p cratonvm-vm -p cratonvm-jit -- -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic` — capture full report.
- NEW-7.2 Convert each site to `?` propagation against the existing `VmError` enum (or add a new variant if the case is novel).
- NEW-7.3 Add a deny-by-default clippy gate to CI for non-test code.
- **Definition of done:** the three files together hold ≤ 5 `unwrap()` calls and each one has a `// SAFETY:` comment justifying it. CI fails on regression.

### NEW-8. Hidden classes (`Lookup.defineHiddenClass`, JEP 371)
Not currently covered by Phase C2.2 with enough detail to be actionable. Spring, Hibernate, ByteBuddy, every framework that synthesizes lambdas at runtime, depends on this.
- NEW-8.1 Implement `MethodHandles.Lookup.defineHiddenClass(byte[], boolean, ClassOption...)` returning a fresh `Lookup` whose lookup class is the new hidden class.
- NEW-8.2 Hidden class identity rules: not in any package's runtime package list, no JVMTI ClassPrepare event, GC-collectable independently of its defining loader.
- NEW-8.3 Nest membership: hidden classes can be `NESTMATE` of their lookup class.
- NEW-8.4 `Class.isHidden()` returns true.
- **Definition of done:** ByteBuddy's "hello world" agent loads and runs.

### NEW-9. Bytecode verifier — actually wire it
`classloading/src/bytecode_verifier.rs` and `verify_frame.rs`/`verify_insn.rs` exist, but A4 is not closed. Need to confirm whether they run on every loaded class and reject malformed input matching HotSpot's `VerifyError` messages.
- NEW-9.1 Audit: is `verify_method` called on every class load, or only when `-Xverify:all`?
- NEW-9.2 If gated, change default to "verify all non-bootstrap classes" (matches HotSpot).
- NEW-9.3 Differential test: feed 100 hand-malformed class files (truncated CP, bad stack height, type mismatch in `iadd`, NPE-prone `getfield` on `null` on stack) and confirm CratonVM's `VerifyError` message text matches HotSpot byte-for-byte (the JCK depends on this).
- NEW-9.4 Subroutine (`jsr`/`ret`) handling for class file < 50 — currently TODO per A4.3.
- **Definition of done:** all four A4 sub-items show ✅.

### NEW-10. Real-JDK boot census harness
There is no automated way to discover which natives are missing when `synthetic-jdk` is off. Today the only signal is "test panics with NoSuchNativeMethod". This blocks NEW-4 from being driven empirically.
- NEW-10.1 Add `vm-cli --dump-missing-natives <main-class>` flag that runs the program with `synthetic-jdk` off, catches every missing-native error, and writes them to a JSON file (`{class, name, descriptor, sample_call_site}`).
- NEW-10.2 Run the harness against: HelloWorld, a Stream pipeline, a HashMap stress test, a String.format stress test, a Properties.load round-trip, a `java.time.LocalDate.now()` call.
- NEW-10.3 Commit the resulting `docs/missing-natives.json` and update it on every release.
- **Definition of done:** the JSON is empty for the six baseline programs above.

---

## Updated Dependency Order

The original Dependency Order block is still correct. The new items slot in as:

```
NEW-1 (JIT skip list)        ─── unblocks M (Performance) and ~50% of "real-world readiness"
NEW-2/3 (sockets + selector) ─── prerequisite for D, F (JDBC), P2 (real-app smoke)
NEW-4 (drop synthetic-jdk)   ─── needs NEW-10 first; then closes K1.1
NEW-5 (jimage)               ─── prerequisite for booting unmodified JDK distros
NEW-6 (Phase R continuation) ─── parallelizable, low-risk, high-signal cleanup
NEW-7 (A3 hardening)         ─── parallelizable, gate before any 1.0 release
NEW-8 (hidden classes)       ─── unblocks ByteBuddy/Spring/Hibernate test matrix (P2)
NEW-9 (verifier wiring)      ─── prerequisite for JCK pass (P1)
NEW-10 (boot census)         ─── tooling prerequisite for NEW-4
```

## PHASE Z — Implement remaining deprecated APIs (run LAST)

**Important correction:** this phase is *not* about deleting code. It
is about implementing any deprecated-but-still-spec'd JDK 25 API that
CratonVM hasn't shipped yet. Real applications routinely call
deprecated APIs through old libraries; until they migrate, CratonVM
must execute these calls correctly, not throw
`UnsupportedOperationException`. We do this last because (a) the
incentive to skip them is high during earlier phases and (b)
correctness of the non-deprecated surface comes first.

Nothing in this phase removes existing code. Every step is "make
sure the deprecated API X works end-to-end".

### Z1. Deprecated `java.lang.*`
- Z1.1 `Thread.stop()` / `stop(Throwable)` — implement the documented
  `ThreadDeath`-throwing semantics under a single global "allow
  deprecated thread control" VM flag. Default off. Test under both.
- Z1.2 `Thread.suspend()` / `resume()` — implement the JVMTI-style
  cooperative pause via a per-thread suspend flag checked at every
  safepoint. Real workloads (debuggers, profilers) still call these.
- Z1.3 `Thread.destroy()` — must throw `NoSuchMethodError` per spec
  but the runtime call site needs to exist for old reflection paths.
- Z1.4 `Object.finalize()` — already wired through the finalizer
  thread (Session 27 + NEW-17). Verify the ordering invariants from
  JLS §12.6 (finalize before resurrection re-enqueue) under stress.
- Z1.5 `Runtime.runFinalization()` / `System.runFinalization()` —
  drain `shared.finalizer_thread` synchronously and return when empty.
- Z1.6 `System.runFinalizersOnExit(boolean)` — accept the call,
  store the flag, honor it on `Runtime.exit()`. Documented bad-idea
  but apps still set it.
- Z1.7 `SecurityManager`, `AccessController.doPrivileged` and the
  whole `java.security.AccessControlContext` machinery (deprecated
  for removal by JEP 411 but still in JDK 25). Implement them
  *correctly* — they're load-bearing for any app that calls
  `doPrivileged`. The JEP only removes them in some future JDK.

### Z2. Deprecated `java.io` / `java.util` / `java.text`
- Z2.1 `Date(int year, int month, int date, ...)` constructors
  (deprecated since JDK 1.1, still required by `java.text.DateFormat`).
- Z2.2 `Date.getYear/getMonth/...` accessors — implement as documented
  even though `Calendar` is preferred.
- Z2.3 `String(byte[], int hibyte, int offset, int count)`.
- Z2.4 `String.getBytes(int srcBegin, int srcEnd, byte[] dst, int dstBegin)`.
- Z2.5 `Character.isJavaLetter`/`isJavaLetterOrDigit`/`isSpace` —
  documented redirects but apps via `JavaCC`-generated parsers
  still call them.
- Z2.6 `Class.newInstance()` — must call the no-arg ctor and rethrow
  the original checked exceptions exactly as specified.
- Z2.7 `Number.byteValue()` / `shortValue()` default delegations.
- Z2.8 `Properties.save(...)` (deprecated alias of `store(...)`).
- Z2.9 `Hashtable.elements()` / `keys()` Enumeration variants.
- Z2.10 `StringBufferInputStream` — entire deprecated class still on
  the JDK 25 module list.

### Z3. Deprecated `java.beans`, `java.rmi`, `javax.security`
- Z3.1 `java.beans.Beans.instantiate(ClassLoader, String)` — old
  serialization path used by Swing apps.
- Z3.2 `java.rmi.activation.*` — deprecated for removal but still in
  the spec; load and run without throwing.
- Z3.3 `javax.security.cert.X509Certificate` (deprecated alias of
  `java.security.cert.X509Certificate`) — implement as a thin
  wrapper.

### Z4. Internal sun.* / jdk.* deprecated reflection shims
- Z4.1 `sun.misc.Unsafe.defineClass` — deprecated, still called by
  ByteBuddy ≤ 1.10. Implement via `Lookup.defineHiddenClass`.
- Z4.2 `sun.misc.Unsafe.allocateMemory` / `freeMemory` /
  `reallocateMemory` — deprecated wrappers around Panama's
  NativeMemoryTable; expose for backward compat.
- Z4.3 `sun.reflect.Reflection.getCallerClass(int depth)` — old
  variant Hadoop and friends still call.
- Z4.4 `sun.misc.SignalHandler` — replaced by `jdk.internal.misc.Signal`
  but the older API name still gets reflected by some libraries.

### Z5. Verification
- Z5.1 Add a CI test `deprecated_apis_round_trip.java` that calls
  every API from Z1–Z4 and asserts each returns the spec-compliant
  result (or throws the spec-compliant exception).
- Z5.2 Run the test under both `JIT_AGGRESSIVE=1` and the
  interpreter to catch JIT-only deprecated-path bugs.
- Z5.3 Cross-check the JDK 25 javadoc `@deprecated` tag set against
  our implemented natives; any tagged API without an implementation
  becomes a Z6 follow-up.

### Z6. Backstop
- Z6.1 For any JDK 25 deprecated API still missing after Z1–Z4, add
  a documented `register_deprecated_unimplemented` call that throws
  `UnsupportedOperationException("CratonVM Z6: <api> not implemented")`
  with a stable error code so test failures point straight at this
  ticket.

**Why this phase runs last:** because doing it earlier would tempt us
to mark deprecated APIs as "out of scope" and never come back. Putting
it after the conformance and performance phases means we *have* to
implement them — there is nothing else left on the list.

---

## Definition of Done

CratonVM is **100% production-ready** when:

1. **All phases A–Q complete** (L conditional on desktop scope).
2. **JCK 25 passes** for the modules listed in P1.
3. **All P2 real-app smoke tests pass** unmodified.
4. **All M5 performance gates met.**
5. **No items in any JIT skip list.**
6. **No `synthetic-jdk` feature flag** (deleted).
7. **No `unwrap`/`panic!` in non-test code** outside the 3 documented sites.
8. **30-day soak (Q5) passes** on representative production workloads.
9. **Conformance report published.**
10. **Phase Z complete** — every deprecated-for-removal API is gone or properly shimmed.

At that point CratonVM can replace HotSpot as the JVM for any Java 25 application.

---

# Per-Crate Code Audit — 2026-05-16

A workspace-wide code review run as 16 parallel agents (one per crate). Findings are organized by crate and tagged: **(0)** code review, **(1)** vulnerabilities, **(2)** stubs/unimplemented, **(3)** performance, **(4)** tests.

Cross-cutting summary at the end.

---

## reader

### (0) Code review
- `class_reader.rs:36-45` — `validate_count` is effectively dead: every `MAX_*` cap equals `u16::MAX`, which a `u16` count can never exceed.
- `class_reader_error.rs:34` — `UnknownAttribute` variant is unused (unknown attrs flow through `Attribute::Unknown`).
- `class_reader.rs:488` — `let _base_pc = base_pc;` no-op marker left for "future offset validation".
- `class_reader.rs:825-893` — `read_target_info` returns `Vec<u8>` raw bytes that are immediately re-encoded byte-by-byte downstream; the right model is a typed enum once verification lands.
- `class_reader.rs:707-714` — `name.to_string()` when the value already lives as `Arc<str>` in the CP.
- `instruction.rs:239-262` — `*pc + 1`, `*pc + 3` arithmetic can overflow `usize` on adversarial input.

### (1) Vulnerabilities
- **`tableswitch` OOM (`instruction.rs:464-487`)** — `count = (high - low + 1) as usize` is bounded only by `i32::MAX`; `Vec::with_capacity(count)` allocates ~16 GB on crafted bytecode. Cap against remaining `code.len()`.
- **`lookupswitch` OOM (`instruction.rs:496-519`)** — same shape: `Vec::with_capacity(npairs)` with `npairs <= i32::MAX`. Bound by `(code.len() - next) / 8`.
- **`buffer.rs:36-104`** — every `read_*` does `position + N > len`, which can wrap. Pattern should be `len - position >= N`.
- **Unknown attribute / StackMapTable body copy (`class_reader.rs:459, 709`)** — up to 4 GB allocated verbatim into a `Vec` on huge files; bounded by buffer remaining but no policy cap.
- **`class_reader.rs:483-485`** — `EnclosingMethod`, `NestHost`, `ModuleMainClass`, `ConstantValue` don't validate the attribute_length matches the fixed payload size; a malformed file desyncs parsing.
- **`jimage.rs:687-688`** — `start as usize..end as usize` on `u64` indices truncates on 32-bit targets before bound-check; add `start <= usize::MAX` guard.

### (2) Stubs / Unimplemented
- `attribute.rs:31-32, 262-268` — `StackMapTable { entries: Vec<u8> }` raw bytes, parsed-but-not-wired into the `Attribute` enum.
- `TypeAnnotation::target_info: Vec<u8>` deferred until verification (Phase 3 marker).
- `jimage.rs:188-189, 669-671` — `Compressed` resources error out instead of decompressing.

### (3) Performance
- `class_reader.rs:395-720` — `read_attributes` dispatch is a 30-arm `match` on `&str`; use `Arc<str>` pointer identity or pre-interned `AttributeId` enum.
- `read_bytes(length)?.to_vec()` at lines 459, 709, 926 — every Code body / StackMapTable / Unknown attribute copies the entire payload; switch the source slice to `Arc<[u8]>` for zero-copy.
- `jimage.rs:687` — `resources[...].to_vec()` copies every class file out of jimage per lookup; thousands of class loads at boot duplicate the entire boot module.
- `intern.rs:35-36` — `StringPool` keeps both `HashSet<&'static str>` and `HashMap<&'static str, Arc<str>>` over the same keys; consolidate.
- `signature.rs:152, 165` — `String::from_utf8_lossy(...).into_owned()` allocates on ASCII data; signatures are already validated UTF-8.

### (4) Tests
- **No fuzz harness** for `read_class` — anecdotal truncated/invalid-magic tests only.
- **No OOM tests** for `tableswitch` / `lookupswitch` paths flagged above.
- **No malformed Code attribute** tests (`code_length=0`, `code_length > attribute_length - header`, `end_pc < start_pc` in exception table).
- **`Record` / `PermittedSubclasses` / `NestMembers` / `LoadableDescriptors`** parsed but not end-to-end tested against real class files.
- `tests/wp1_7_attrs.rs` silently no-ops if the fixture is missing — failure mode is invisible in CI.

---

## types

### (0) Code review
- `value.rs:11` — `Value` enum size invariant (16 bytes) is asserted in a *different* crate (`jit/src/lib.rs`); move the `static_assert` into `types`.
- `value.rs:150` — `Value::as_object` returns `Option<Option<ObjectRef>>` — confusing nested-Option API. Split into `as_object_ref` + `is_object_or_null`.
- `value.rs:60` — `ObjectRef::from_raw` panics on null/unaligned; add `try_from_raw`.
- `heap_types.rs:113` — `ObjectHeader` exposes `_padding`, `_gc_reserved`, `forwarding_ptr` as `pub`. Mutating these bypasses GC invariants.
- `compact_value.rs:219` — `CompactValue::tag()` returns `Double` for `Long`; documented footgun.
- `error.rs:171` — `RuntimeError` has 30+ variants — borderline oversized. Carve out `JavaIoError`.

### (1) Vulnerabilities
- **47-bit pointer truncation in `CompactValue::object` (`compact_value.rs:179`)** — silently masks pointers in release; only `debug_assert` in debug. Breaks on AArch64 52-bit VA, x86-64 5-level paging (57-bit), and `MAP_FIXED` allocations above `0x0000_7FFF_FFFF_FFFF`. Real portability hazard. Gate with `cfg` and use checked constructor.
- **`Send + Sync` on `ObjectRef` (`value.rs:103`)** — assumes single-threaded model; comment says so but the impl ships unconditionally.
- **`decode_value` (`value.rs:198-211`)** — silently degrades unaligned/null `VTAG_OBJECT` to `Object(None)`; hides real bugs writing non-ref bits through a ref accessor.
- **`NANBOX_BITS` (`compact_value.rs:42`)** — no `cfg(target_pointer_width = "64")` gate; builds and silently misbehaves on 32-bit.

### (2) Stubs / Unimplemented
- `RuntimeError::NotImplemented` (`error.rs:256`) used as a fallback variant — proliferates and is misused as "internal error" downstream.
- `value.rs:37` — doc-comment marker "will be replaced with a proper GC-managed pointer in Phase 6".

### (3) Performance
- `intern.rs:30` — `StringPool` single `Mutex` is contended in multi-threaded class loading; shard or use `DashMap`/`RwLock`.
- `error.rs:179` — many variants carry `String` payloads, allocating on the success-of-error path. Use `Cow<'static, str>` or `Arc<str>` (the interner is already a dep!).
- `Value::Object(Option<ObjectRef>)` adds a discriminant for null since the raw pointer has no niche; flatten by using a sentinel.

### (4) Tests
- Strong coverage of `MIN/MAX`, `-0.0`, NaN, alignment edges, array overflow.
- **No test for `CompactValue::object` with pointers > 47 bits** — release-mode silent truncation is uncovered.
- **No offset-asserts on `ObjectHeader`** for `class_id`, `kind`, `element_type`, `forwarding_ptr` — JIT-consumed fields can silently drift.
- **No test verifies the canonical NaN bit pattern** is actually produced (only `is_nan()` is checked).
- `intern_arc` cross-mode concurrency test missing.

---

## native-api

### (0) Code review
- **Trait bloat**: `NativeContext` (`registry.rs:104-1236`) is a god-trait with 100+ methods spanning threading, JPMS, FFI, JFR, reflection, modules, classloading, GC, scoped values. Split into focused sub-traits.
- **Default impls drift fail-open**: `is_package_exported_unqualified` defaults `true` (`registry.rs:1072`), `check_deep_reflection_access` defaults `Ok(())` (`registry.rs:1181`). Security-relevant fallbacks must fail-closed.
- **Dead imports**: `HashMap` unused in `registry.rs:6`, `ffi.rs:7`, `fd_table.rs:5`.

### (1) Vulnerabilities
- **Hash-collision panic on `register` (`registry.rs:1330-1335`)** — would abort the VM. Promote to per-key `Vec` chaining.
- **Raw `*mut u8` exposure**: `allocate_native_memory` returns raw pointers (`registry.rs:791`), `find_native_symbol -> Option<usize>` (`registry.rs:801`). No use-after-free guard at the API layer.
- **`NativeAllocation: Send + Sync` claimed at `ffi.rs:32-33`** under the assumption every caller wraps in a `Mutex`. Fragile.
- **`init_level` mutex `.expect()` (`init_level.rs:77, 91, 93`)** panics on poison during bootstrap; use `into_inner` recovery.
- **`poll_ready` non-atomic blocking-mode toggle (`fd_table.rs:741-800`)** — concurrent reader on the same socket can observe non-blocking state.
- **`tcp_accept` TOCTOU (`fd_table.rs:678`)** — drops read-lock before re-acquiring write-lock.

### (2) Stubs / Unimplemented
- `redefine_class` default returns `Err("not implemented")` (`registry.rs:1011`).
- `class_num_total_fields` default returns 0 (`registry.rs:275`) — docstring warns this causes OOB.

### (3) Performance
- **Native dispatch lock-traffic**: every call goes through `record_enter` + `record_exit` on a process-global `parking_lot::Mutex` ring. Under heavy native traffic (`Unsafe` hot loop) this is global serialization. Move to per-thread ring or atomic ring with relaxed ordering.
- `name_map` (`native_ring.rs:46`) uses `Mutex` despite being read-only post-startup; switch to `OnceLock<FxHashMap>` after registration.
- `find` fallback descriptor variants allocate 1-4 `String`s per miss (`registry.rs:1363-1400`).
- `FileDescriptorTable` outer `RwLock` + per-entry `Mutex` (`fd_table.rs:55`) — switch to `DashMap` or `ArcSwap<FxHashMap>`.
- `available()` (`fd_table.rs:333-355`) does three `seek` syscalls; use `metadata().len() - stream_position()`.

### (4) Tests
- **No tests for `alias_class`** (`registry.rs:1424`) parsing logic.
- **No tests for the descriptor-fallback path in `find`** (`registry.rs:1363-1400`).
- **`native_ring.rs` has no tests** despite 64-entry ring with wraparound and ordering invariants.
- **No tests** for `TlsStream`, `UdpSocket`, `TcpStream`, `TcpListener`, `Pipe*`, `Child*Pipe`, `clone_file`, `pread_at`/`pwrite_at`, `poll_ready` — networking and subprocess paths entirely uncovered at unit level.

---

## native-collections

> **Status**: Crate is `#[deprecated]` (Session 15) and gated behind `synthetic-jdk` feature in `Cargo.toml:14`. Still compiles and ships unconditionally. The "Definition of Done" already requires deleting this — the findings below are urgency-ordering for that removal.

### (0) Code review
- **One 19 576-line `lib.rs`**. No submodules.
- `lib.rs:95` — orphan duplicate function body collapsed under a `///` line that makes it look like documentation. Dead/confusing.
- Stray `eprintln!` debug spam in hot paths: `lib.rs:1150` (`[HM-EQ]` — even runs `std::env::var` per key compare), `1240`, `1254`, `1786`, `1818`, `2860`, `2868`.
- `unbox_wrapper` (`lib.rs:187`) treats ANY object with one primitive field as a wrapper — user classes with a single `int` field are silently unboxed during HashMap key compare.
- Vector methods (`native_vec_*`) delegate to ArrayList code with no monitor, violating Vector's synchronization contract.

### (1) Vulnerabilities
- **OOB returns null instead of throwing** — `native_al_get` (`:500`), `native_al_set` (`:522`), `native_al_add_at` (`:561`). Comments admit "IndexOutOfBoundsException (simplified)".
- **No `ConcurrentModificationException` anywhere** — grep returns zero matches. Fail-fast iterators are absent.
- **Hash collision DoS** — `map_hash_key` (`:1086`) is unsalted; `CHAIN_WALK_LIMIT = 4096` (`:1782`) silently drops dedup checks beyond 4096 collisions, allowing duplicate-key inserts.
- **Capacity-doubling without overflow guards** — `pq_ensure_capacity` (`:10759`), `ad_ensure_capacity` (`:10302`), `tm_ensure_capacity` (`:12022`), `ts_ensure_capacity` (`:12091`).
- **`al_ensure_capacity` (`:330`)** — silently returns the OLD buffer when cap exceeded, dropping `add()` writes with no `OutOfMemoryError`.
- **CHM lock leak on inner errors** — `native_chm_put` (`:14337`), `_remove` (`:14354`), `_put_if_absent` (`:14375`) skip `monitor_exit` on `Err`.
- **`native_lbq_take_blocking` (`:16053-16068`)** — spins 10 000 times then **returns null**, violating `BlockingQueue.take` contract.
- **LinkedHashMap overlay keyed by raw pointer** (`:9574-9595`) — process-global `Mutex<HashMap<usize, ...>>` keyed on `this.as_ptr() as usize`. Stale entries leak into new objects when GC reuses addresses.

### (2) Stubs / Unimplemented
- HashMap missing `<init>(I, F)` (capacity, loadFactor) constructor (`:1390-1395`).
- `map_resize` "cannot grow further" at `MAP_MAX_CAPACITY` silently keeps inserting into an over-loaded table (`:1226-1228`).
- No `TODO`/`unimplemented!()` markers — code "completes" silently with `Ok(Some(Value::Object(None)))` where Java would throw (`:493, :497, :517, :551, :555, :585, :1751`).

### (3) Performance
- **TreeMap.put / TreeSet.add are O(n)** — `tm_insert_at` and `tm_remove_at` shift the whole flat array (`:12042, :12055`). Named "TreeMap"; implemented as sorted vector.
- **LinkedBlockingQueue.poll shifts the entire array left on every dequeue** (`:16090-16092`). Should be linked list or ring buffer.
- ArrayList iteration uses `ctx.get_array_element` in a Rust loop (`:594-597, :619-623, :1234-1273`). Expose a bulk-copy intrinsic on `NativeContext`.
- No capacity hint propagation; `addAll` / `init_from_map` rehashes repeatedly.

### (4) Tests
- ~60 `#[test]`s but almost all are **registry-presence checks** (`r.find(c, "put", ...).is_some()`) — they verify method strings are registered but exercise zero behaviour.
- **No JDK-semantics tests** for: null keys/values in HashMap, CME on iterator after add/remove, IOOBE on `List.get(-1)`, ClassCastException in TreeMap with incomparable keys, identity-vs-equals contract.

---

## native-io

### (0) Code review
- `lib.rs` is **12 960 lines** of god-module.
- **Parallel registries for the same kernel resource** — `dgram_registry` (`datagram.rs:57`) vs existing `udp_registry`; `aio_registry` (`async_socket.rs:59`) vs `tcp_registry` (`socket_channel.rs:93`). Bytecode crossing the two APIs sees diverging fd state.
- Silent option no-ops in `apply_option` (`socket_channel.rs:890`, `net.rs:577`) — SO_REUSEADDR, SO_RCVBUF, SO_SNDBUF, SO_KEEPALIVE silently `Ok(())`.
- `random_access_file.rs:188` — `O_SYNC`/`O_DSYNC` silently dropped via `_ = (O_SYNC, O_DSYNC);`. Java spec requires durable writes for `"rws"`/`"rwd"`.
- 487 `unwrap()`/`expect()` across the crate.

### (1) Vulnerabilities (multiple serious)
- **Naive path-traversal check (`lib.rs:96`)** — rejects `foo..bar.txt` while bypassable via UNC `\\server\share` or post-validation `\\?\C:\..\target`.
- **Path validation is globally toggleable** (`set_path_validation_enabled`, `lib.rs:70`) — any in-process code disables it for everyone.
- **RandomAccessFile bypasses path validation** (`random_access_file.rs:190`); same for `zip_real_jar.rs:128`.
- **Zip-slip not handled** — `zip_real_jar.rs` uses `f.name()` as-is (`:229, :346, :428`).
- **Process spawn accepts arbitrary `program`/`args`/`current_dir`** with no validation (`process.rs:154, 170`).
- **TOCTOU**: `validate_path` canonicalizes (`lib.rs:105`) then returns a string; subsequent `File::open` re-resolves — symlink swap escapes the check.
- **Unbounded allocations keyed on attacker-controlled `len`** — `net.rs:486, 527`, `socket_channel.rs:830`, `random_access_file.rs:245`, `process.rs:345, 454`, `zip_real_jar.rs:364`. A peer sending multi-GiB `len` triggers immediate OOM.
- **Raw native pointer trust** — `net.rs:507-509, 530-531` `copy_nonoverlapping` to/from `addr` taken from Java `Long` with only `addr != 0` check. Java-side overflow or forged DirectByteBuffer address corrupts arbitrary process memory. Same in `async_socket.rs:911-913`, `socket_channel.rs:417-419, 453-455`.
- **`pipe.rs:47-58`** — `closed` flag is non-atomic; concurrent close races leak/double-close OS handles.

### (2) Stubs / Unimplemented
- `net.rs:338` — `net_listen` no-op (accept-queue backlog ignored).
- `net.rs:579-580` — SO_KEEPALIVE ignored.
- `socket_channel.rs:893-895` — four TCP options accepted but no-op.
- `file_channel.rs:282-289` — `maxDirectTransferSize0` returns constant; Windows `TransmitFile` not wired.
- `direct_buffer.rs:42` — Cleaner integration "best-effort"; not driven by the GC.
- `nio_native.rs:7` module docstring acknowledges its functions "work only as long as" callers don't observe.

### (3) Performance
- **Sync-blocking I/O on the user thread** — `bind`, `accept`, `connect`, `read`, `write` (`net.rs:321, 371, 419, 496, 538`). One global per-fd `RwLock` around `net_sockets()` means one slow client blocks every other Net op needing the write lock.
- **No buffered I/O** — `read0`/`write0` go straight to the kernel; combined with byte-per-syscall in `random_access_file.rs:206-211, 266`, this is **syscall-per-byte** on the slow path.
- `socket_channel.rs:863` — `buffer_read_bytes` copies the whole `ByteBuffer` into a `Vec<u8>` even for direct buffers.
- `async_socket.rs:33` — worker pool uses `Mutex<VecDeque>` + `Condvar`; thundering-herd on every notification.
- `random_access_file.rs:54` — global `Mutex<HashMap>` serializes **all** RAF ops process-wide.
- `direct_buffer.rs:65` — 256 MiB hard cap on direct memory hardcoded; not wired to `-XX:MaxDirectMemorySize`.

### (4) Tests
- **No `tests/` directory**.
- Path-validation tests only cover `/etc/../passwd`, null-byte, and the toggle. **Missing**: Windows `\\?\C:\..`, UNC, `%2e%2e`, legitimate `..` substrings (false positives), symlink TOCTOU.
- **No tests for zip-slip rejection, malformed zip, or large-entry behavior** in `zip_real_jar.rs`.
- **Windows-specific FS quirks** uncovered: trailing-dot, reserved names (`CON`, `PRN`), long paths > 260, case-insensitive collisions.
- **No fuzz / property tests** anywhere in `native-io/src/`.

---

## native-builtins

> Crate is enormous (`lib.rs` 34 736 lines, `phases_late.rs` 21 000+, `lang_class.rs` 10 839). Far exceeds its name — pulls in `rusqlite`, `zip`, `rustls`, `p12`, `libffi`. **Splitting recommended.**

### (0) Code review
- **`lang_system.rs:117`** — `arraycopy` same-array check uses pointer equality on `ObjectRef`. Works only because of ObjectRef's wrapping shape; will silently break if ObjectRef ever has multiple representations.
- **`arraycopy` ArrayStoreException semantics wrong** (`lang_system.rs:40-133`) — no per-element type check; reference-array writes succeed silently regardless of component type.
- **`lang_misc.rs:1001`** — registered `<init>()V` for 34 exception classes as `native_noop_with_this`, omitting the `write_throwable_cause(this, this)` that the proper helper does. `initCause` afterwards behaves asymmetrically vs the `(String)` constructor.
- **`security_manager.rs:148, 187`** — when no policy file is loaded, `policy_allows` returns `true` (allow-all). Real JDK with SM installed denies most operations.

### (1) Vulnerabilities
- **Arena `read::<N>` bug (`lib.rs:13139-13148`)** — returns from offset 0, ignoring offset semantics. Every `get_byte(addr)` returns the same byte regardless of where in the arena `addr` was allocated. **Real correctness bug** for any caller computing `base + i*sizeof(T)` (e.g. `java.nio.Bits`).
- **Arena `write` (`lib.rs:13150-13158`)** silently extends the arena beyond the originally requested size, breaking the buffer-overflow safety contract.
- **Arena allocation race (`lib.rs:13083-13092`)** — `next_addr` lock released before `inner.write()` lock taken; saturating `next_addr` at `i64::MAX` causes all subsequent allocations to alias the same address.
- **`unsafe_natives.rs:399-403, 498-504`** — `Unsafe.defineClass` / `defineAnonymousClass` set `skip_verification: true`. Gate behind a hardening flag.
- **`panama.rs:23-47`** — `validated_fn_ptr` does null-and-alignment-check then `transmute_copy` to call arbitrary native code. If a JVM-side allocator hands an attacker-controlled address to a downcall, this is an arbitrary-code-execution primitive.
- **`String.toLowerCase`/`toUpperCase`** (`lang_string.rs:1851, 1862`) uses Rust default case mapping; Turkish locale (`İ`/`ı`) diverges from JDK.

### (2) Stubs / Unimplemented
- `Unsafe.park` ignores `blocker` (`unsafe_natives.rs:89-108`) — `LockSupport.getBlocker` returns stale.
- `Unsafe.invokeCleaner` does not actually free the buffer (`unsafe_natives.rs:126-148`) — DirectByteBuffer memory leaks per process.
- `graalvm_compat.rs` has **99** `Ok(None)` returns — almost entirely stubs.
- `vector_api.rs` (`jdk.incubator.vector`) — 56 `Ok(None)` patterns.

### (3) Performance
- **`arraycopy` is per-element through `NativeContext`** (`lang_system.rs:118-130`) — no bulk copy. For large primitive arrays this is the single biggest perf win in the crate.
- `lang_string.rs:1564-1611` — `StringBuilder.indexOf` allocates `String::from_utf16_lossy` per call, then `find`, then `encode_utf16().count()`. Twice the string size allocated per call.
- `lib.rs:13062-13165` — arena `HashMap<i64, Arena>` under `RwLock`; every `get_byte` takes the read lock. `java.nio.Bits` hot path serializes.
- `lang_misc.rs:484-499` — `Throwable.addSuppressed` is O(N^2) — allocates a new array per call.

### (4) Tests
- 1 215 `#[test]` occurrences across 32 files — heavy unit-test density.
- **Missing corner cases**: NaN handling for `Math.min/max` (Rust's `min`/`max` is impl-defined for NaN; JDK requires NaN propagation), surrogate-pair handling for `String.codePointAt`/`toLowerCase` on `İ`/`ı`, `Integer.MIN_VALUE` overflow in `absExact`/`negateExact`, `String.format` locale edges, `arraycopy` ArrayStoreException scenarios.
- **`lang_class.rs` (10 839 lines)** has 158 `unwrap`/`panic` markers — many reflection helpers look unguarded.

---

## native-awt

### (0) Code review
- 91 native bindings registered (`natives.rs:21-31`), but ~42 (>45%) are pure no-op closures returning `void_ok()`. The entire Graphics2D family (`natives.rs:305-339`) never reaches `SoftwareRenderer` or `Graphics2DContext`.
- `cocoa.rs:355-413` mixes `objc2-app-kit` typed APIs with raw `msg_send![class!("CGImage"), ...]` — but `CGImage` is a CF C function, not an Obj-C class; `class!("CGImage")` is null and **the blit silently no-ops on macOS**.
- `cocoa.rs:519-546` (`rasterize_text`) explicitly returns an all-zero pixel buffer.
- `Win32Backend::measure_text` (`win32.rs:565-578`) uses a hard-coded `0.6 * font_size` heuristic, ignoring the cached `DirectWriteRenderer` (the cached `dwrite` field at `win32.rs:117` is dead code).

### (1) Vulnerabilities
- `x11.rs:438-453` — casts `&[u32]` to `&[u8]` for `put_image` without checking the X server's `image_byte_order`; pixels swap channels on big-endian servers.
- `image.rs:107-114` — `assert!` in `index()`; `natives.rs:370-381` casts negative coords to `u32`. A buggy Java caller panics the VM via `BufferedImage.getRGB`.
- `win32.rs:765-775` — clipboard reader does manual null-terminator scan with no upper bound. Malformed clipboard payload missing a terminator walks unmapped memory.
- `cocoa.rs:364-370` — passes raw `pixels.as_ptr()` into `CGDataProvider` with null `releaseData` callback while the `Vec<u32>` is on the Rust stack — lifetime is undefined.
- `clipboard_set_text` (`win32.rs:797-806`) leaks `hmem` on `OpenClipboard` failure or ignored `SetClipboardData` result.

### (2) Stubs (substantial)
- Every Graphics2D drawing native is a no-op.
- `drawString` no-op; `FontMetrics` uses crude `len() * size * 0.55` (wrong for non-ASCII).
- `Frame::toFront`/`toBack`/`setIconImage`/`setMenuBar` no-ops.
- `Clipboard` top-level natives no-op even though Win32/Cocoa backends implement them.
- `JFileChooser` always returns CANCEL; `JOptionPane` only logs.
- `EventQueue::getNextEvent` discards the event; `peekEvent` always null.
- X11 clipboard stubbed; X11/Win32 file dialogs stubbed; Cocoa `rasterize_text` returns blank bitmap.

### (3) Performance
- `SoftwareRenderer::fill_rect_raw` (`renderer.rs:433-441`) — per-pixel `put_pixel` even for identity transforms. `slice::fill` per row would be ~20x faster.
- `blit_image`/`blit_image_scaled` (`renderer.rs:662-706`) — scalar loops; no SIMD or row memcpy fast-path even when src/dst formats match.
- Win32 `blit_buffer` (`win32.rs:421-499`) — `CreateDIBSection`/`DeleteObject` every paint; converts ARGB→BGRA pixel-by-pixel.
- `rasterize_text` (`win32.rs:597-744`) allocates a fresh `IDWriteFactory` + `HFONT` + `DIBSection` per call.
- `EDT::invoke_and_wait` uses 100 us sleep-poll (`edt.rs:150`) instead of the `Condvar` it allocates and immediately drops.

### (4) Tests
- 198 unit tests; **no `tests/` integration directory**.
- Renderer has solid headless coverage (32 tests); image module has 19.
- **`natives.rs` has exactly 1 test** — asserts `count >= 50` only. None of the 91 registered closures are exercised, so the no-op stubs above are invisible to CI.
- **Zero pixel-equality / golden-image tests** — no end-to-end "render a `JButton` and compare hash" coverage.
- **Platform backends (`win32.rs`, `x11.rs`, `cocoa.rs`) have no tests at all**, not even compile-time smoke tests with a mock `PlatformBackend`.

---

## jit-api

### (0) Code review
- **`JitRuntimeHelpersBuilder` is unused dead code** — `vm/src/jit/helpers.rs:1916` constructs `JitRuntimeHelpers` via a direct struct literal; the builder (`lib.rs:191-296`) is only exercised by in-crate tests.
- **Stringly-typed builder API** — `Builder::set(&mut self, name: &str, addr: usize)` (`lib.rs:239-278`) accepts arbitrary strings and silently logs to stderr on typos. No compile-time safety, side-effecting on misuse.
- **Triple duplication**: the 32-field list appears in the struct, `all_pointers`, `field_names`, `Builder::new`, and `Builder::set` (5 places). Adding a helper requires touching all five.
- **Missing `#[non_exhaustive]`** on `JitRuntimeHelpers`, `CachedBytecodeMethod`, `LoweringError`.

### (1) Vulnerabilities
- **`usize`-typed function pointers with no type safety** — every field is `pub usize` representing a raw code address dereferenced as `extern "C"` with different signatures (`lib.rs:42-85`). A wrong-field-name `set()` (silently logged) wires the wrong signature with no compile-time check.
- **`validate()` only checks non-zero** (`lib.rs:94-97`) — does not catch wrong-pointer-in-right-slot. Docstring mentions alignment but no impl.

### (2) Stubs / Unimplemented
- None. Closest is the `eprintln!("...unknown field...")` silent-degrade path.

### (3) Performance
- `null_pointers` and `validate` both allocate `Vec<&'static str>` for what should be a scalar bool.
- `Arc<dyn GpuLowering>` indirection (`gpu_lowering.rs:3-7`) when there is exactly one implementor today.

### (4) Tests
- **No tests for `gpu_lowering.rs` at all** — `LoweringError` `Display` impl is untested.
- `test_helpers_all_fields_distinct` (`lib.rs:480-501`) iterates only **29** of 32 fields — stale; `uncommon_trap`, `math_fma_double`, `math_fma_float` are missing.
- Builder tests assert raw magic numbers (`assert_eq!(nulls.len(), 32)`) instead of computing against `field_names().len()`.
- **No contract test**: "every name in `field_names()` is accepted by `Builder::set()`" is not asserted, so a typo in `set` would silently no-op.

---

## jit

> The roadmap calls out "JIT crashes on FP math" and "JIT miscompilation on complex control flow". Both are confirmed below.

### (0) Code review
- **Monolithic `x64.rs` — 17 643 lines**. `compile()` is one giant match (`~6985-10800`); split per-opcode-family.
- **Two parallel pipelines** — template-style emitter (`x64.rs`) and Sea-of-Nodes IR (`ir.rs`, `ir_optimize.rs`, `ir_schedule.rs`, `ir_lower.rs`). The IR is shadowed; `ir::ir_compatible` (`ir.rs:1012-1020`) rejects anything with field/invoke/typecheck/new — essentially every realistic method.
- **"No panic in release" discipline is undermined** by `assert!` in `ExecutableBuffer::emit_byte` (`lib.rs:201-211, 230-234`), `expect(...)` in OSR trampoline (`lib.rs:854`), `process::abort()` on `make_executable` failure (`lib.rs:288-303`).
- **`regalloc.rs` silently caps locals at 64** (`:319, 381, 399`); methods with >64 locals (legal up to 65535) get incorrect interference. No assertion or bail.
- `ir.rs:537, 559` — phis hard-typed `IrType::Int` for all locals/stack; would miscompile float/double/reference phis if the IR were widened.
- `ir.rs:950-1006` — `find_branch_targets` ignores `ifnull`/`ifnonnull` (0xc6/0xc7), `goto_w` (0xc8), `tableswitch`/`lookupswitch` — silently mis-detects merges.

### (1) Vulnerabilities (multiple JVM-safety holes)
- **GC root miscoverage (high severity)** — `oop_maps` is documented as "not yet populated" (`lib.rs:438-441, 472, 500`); fallback is conservative stack scan. Combined with `stack_oop_marks` tracked at compile-time but no emitted entries, a moving GC can miss real references or treat ints as oops.
- **Deopt sentinel collides with valid values (high severity)** — `i64::MIN` returned as the deopt marker in RAX (`x64.rs:2951, 6644-6725`). A legitimate `long` method returning `Long.MIN_VALUE`, or a `double` returning `-0.0` (bit pattern `0x8000_0000_0000_0000`), is indistinguishable from "deoptimize". Use an out-of-band channel.
- **W^X races and unchecked patching** — `patch_i32`/`patch_byte` (`lib.rs:254-268`) write to `self.ptr` without checking buffer state. `make_writable`/`make_executable` are non-atomic and not page-aligned-explicit. Apple-Silicon path (`platform.rs:222-240`) uses `mprotect`, but `MAP_JIT` pages require `pthread_jit_write_protect_np` with the `com.apple.security.cs.allow-jit` entitlement — current impl will fail or kill the process.
- **OSR trampoline is non-reentrant** (`lib.rs:740-873`) — bakes `frame_locals.as_ptr()` as absolute `i64` immediate into a fresh executable buffer per invocation, then drops both. One mmap per OSR (DoS-able by hammering a hot loop); unsafe under unwinding.
- **No stack-overflow check in prologue** — zero hits for `stack_check`/`guard_page`. Deep recursion crashes with raw SIGSEGV instead of `StackOverflowError`.
- **Integer overflow in switch decoding** — `(high - low + 1).max(0) as usize` (`x64.rs:8923`, `regalloc.rs:57, 125`) — `max(0)` is applied AFTER `i32` wrap; a crafted classfile makes the JIT allocate huge `Vec`s or skip targets.
- **No exception-table support** — `athrow` bails (`lib.rs:3373-3387`); implicit NPE/AIOOBE from JIT'd code must unwind through interpreter frames, but no unwind info is set up.

### (2) Stubs / Unimplemented
- **`frem`/`drem` (0x72, 0x73)** — scanner-accepted (`x64.rs:1771-1772`) but no codegen arm between `irem` and `ineg` (~8243). Falls to default and crashes/no-ops. **Root cause for "JIT crashes on FP math."**
- `emit_float_binop` (`x64.rs:6747`) always materializes both operands via GPR, contradicting the doc comment promising XMM-resident optimization.
- "26 rounds" optimization claim is not borne out — only `fold_constants + algebraic_simplify + gvn + eliminate_dead_nodes` run in a 8-iteration loop (`ir_optimize.rs:14-26`); other passes are single-pass. Total ~7 distinct passes.
- IR builder bails on any unsupported opcode (`ir.rs:938-939`) — entire `invoke*`, `getfield`/`putfield`, `new`, `anewarray`, `monitor*`, `athrow` universe unsupported.

### (3) Performance
- Template emitter does one stack push/pop per opcode through frame slots; `pop_to_rax`/`push_from_rax` everywhere incurs many memory round-trips a proper regalloc would elide.
- IR pipeline is gated to a tiny subset — wasted effort.
- `canonicalize_stack` (`x64.rs:3315-3333`) flushes everything to memory at every merge; phi emission would be cheaper.
- OSR trampoline allocates a fresh mprotected page per OSR entry; shared dispatching trampoline keyed on (target_addr, locals_ptr) would amortize.
- No code/IR caching: every `compile()` rebuilds the IR and reruns analyses from scratch.

### (4) Tests
- FP unit tests are simple "load arg, op, return" patterns only. **No tests** for: FP loops with phis, FP arrays, `frem`/`drem` (zero hits), NaN propagation through long chains.
- **Only 2 OSR tests**; no OSR-with-FP, no OSR-with-deopt, no OSR-into-inlined-frame.
- 49 `deopt.rs` test markers but they test the *data structures*, not end-to-end JIT → interpreter through a real deopt point.
- **No differential testing** — grep for `differential` returns no matches.
- **No fuzzing** for bytecode scanner or codegen.
- **`regalloc` 64-local truncation is not tested** — no method with >64 locals.

### Top correctness blockers
1. `frem`/`drem` codegen missing (`x64.rs:1771` + missing arm near 8243).
2. IR back-edge merging in `add_merge_predecessor` after `activate_merge` already ran (`ir.rs:485-570`).
3. Branch-target stack canonicalization is silently skipped when depths disagree (`x64.rs:7012-7017`) — root-cause candidate for "JIT miscompilation on complex control flow."
4. `i64::MIN` deopt sentinel collision (`x64.rs:6721-6725`).
5. Silent 64-local truncation in regalloc (`regalloc.rs:319, 381, 399`).
6. Missing oop-map emission (`lib.rs:438-441`) — GC correctness hole.

---

## jit-cuda

### (0) Code review
- Clean module boundary: `lib.rs:11-14` explicitly forbids cudarc/libcuda references and JVM heap access — pure PTX-text emission. Good discipline.
- `emit.rs:571-573` — `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` collapsed into a single arm that always errors downstream; the merging is dead code.
- `emit.rs:710-715` — `fload`/`dload` delegate to `lload`; works because `Reg` carries its own `RegKind`, but the helper-name overlap is fragile.

### (1) Vulnerabilities
- No `unsafe`, no FFI. PTX text uses controlled formatters; user-controlled data flows only through `mangle()` (`lowering.rs:194-210`) with `[A-Za-z0-9_]` allowlist.
- Direct `self.bytes[pc + 1]` indexing in `emit.rs` (e.g. `:391, 404, 548-549`) relies on `instr_size` correctness for memory safety on malformed `.class`. Defense in depth: prefer `.get(...).ok_or(...)`.

### (2) Stubs / Unimplemented (real bug + scope notes)
- **`frem`/`drem` emit `"rem.f32"` / `"rem.f64"` mnemonics (`emit.rs:813-817, 826-830`) — PTX has no such mnemonics; ptxas will reject any kernel that hits this path. Real bug.**
- Analyzer accepts ~210 opcodes; lowering implements ~96 concrete behaviours. Roughly 30+ analyzer-accepted opcodes will still hit `UnsupportedNode` at `emit.rs:660-665` (`*cmp*`, exotic dups, branches outside the canonical loop guard).
- Loop recognizer accepts at most one backward branch (`loop_recog.rs:62-73`) — no nested loops, no multi-loop methods.
- Only static methods, primitive scalars + primitive arrays. No `Object[]`, no objects, no boxed types, no string ops, no fields, no method calls, no allocation, no try/catch, no switch.

### (3) Performance
- Per-thread `st.global.*` to `ret_ptr` for scalar returns (`emit.rs:1221-1235`) — every thread writes the same value. ptxas may CSE but bandwidth-wasteful for large N.
- Bounds-check reloads `pN_len` per array access (`emit.rs:293-298`) instead of hoisting once.
- `RegPool` allocates a register per literal (`emit.rs:300-301, 367-368`); kernel text grows.
- Memory/launch/streams concerns live in `cuda-bridge` and `vm/runtime/gpu_marshal.rs`, not here.

### (4) Tests
- ~25 tests; **ptxas round-trip is `#[ignore]`** (`lowering.rs:401-422`) — default `cargo test` doesn't need CUDA. Good for CI.
- **End-to-end Java → PTX → numerical-result tests missing** — ptxas test verifies syntax only; no kernel execution against a JVM reference.
- No tests for `UnsupportedReturnType`, `JsrRet`, `Monitor`, `HasExceptionHandlers`, `NoCode`, `BadDescriptor`, `UnknownOpcode`.
- No lowering test for `frem`/`drem` — the broken `rem.f32` mnemonic would be caught immediately.
- `straight_line_method_lowers_without_loop` (`lowering.rs:457-469`) violates the "no synthetic bytecode" rule it claims to follow.

---

## cuda-bridge

### (0) Code review
- `backend_cuda.rs:103-152` — argument binding splits args into 5 type-specific `Vec`s with 5 cursor indices; could be one walk over a single `Vec<KernelArg>`.
- `lib.rs:174-205` — `DeviceBuffer` exposes `len()`/`is_empty()` but no `Drop` impl or comment confirming `CudaSlice` cleans up.
- `lib.rs:107-115` — README example (`README.md:33`) calls `module.launch(...)` which does not exist. README is out of sync.
- `backend_cuda.rs:31-37` — `probe()` hard-codes `ordinal: 0`. Misleading "all devices" name.
- `DeviceCaps` (`lib.rs:39-45`) is **not** `#[non_exhaustive]` — adding fields breaks pattern matches.

### (1) Vulnerabilities
- **`backend_cuda.rs:122-124, 147-150`** — `KernelArg::DevicePtr(u64)` passed via `builder.arg(&ptr)` as plain `u64`, bypassing cudarc's `CUdeviceptr` type. May misalign on cudarc 0.13's `DeviceRepr` for `u64`.
- **`backend_cuda.rs:212-216`** — `device_ptr()` discards the `_record` returned by `CudaSlice::device_ptr`. In cudarc 0.13 this record is a sync handle ensuring the pointer remains valid relative to stream ordering. Dropping it may cause use-after-free / stream race if the buffer is freed while a kernel using the bare `u64` is still queued.
- **`backend_cuda.rs:153-157`** — `unsafe { builder.launch(...) }` with no safety comment; arg/PTX mismatch is UB.
- No mapping from `CUresult` codes to typed `DeviceError` variants — every cudarc error stringifies into `DeviceError::Driver(String)`. `Memcpy`/`Launch` variants exist but are never produced.

### (2) Stubs / Unimplemented
- No `todo!()`/`unimplemented!()`/`FIXME` in non-test code. `backend_stub.rs` is a deliberate `NoDriver` stub.

### (3) Performance
- `HashMap<String, CudaFunction>` keyed by `String` (`backend_cuda.rs:60`); every `launch_raw` does a string-hash lookup. Return a typed handle from `from_ptx` so callers skip the lookup.
- No PTX module caching across calls — recompile/link per `from_ptx`.
- Only `ctx.default_stream()` used — no async overlap of memcpy + compute.
- No host-pinned memory API — pageable host memory roughly halves PCIe bandwidth versus pinned buffers.
- Per-launch allocations of 5 `Vec`s (`backend_cuda.rs:103-152`); use `SmallVec`/`ArrayVec`.

### (4) Tests
- No `tests/` directory; only inline tests gated `#[cfg(all(test, not(feature = "cuda")))]`.
- Stub-mode coverage solid, but **`DeviceModule::from_ptx`, `DeviceBuffer::{uninit,zeros,from_host,to_host}`** are not asserted to return `NoDriver` in stub mode.
- **`gpu-it` feature is declared but no `#[cfg(feature = "gpu-it")]` tests exist** — feature is dead.
- No `Send`/`Sync` static-assert tests for `DeviceContext`/`DeviceBuffer`.

---

## classloading

### (0) Code review
- **`define_class_with_options` skips verification entirely** (`class_manager.rs:1309-1944`). It records `skip_verification` in a side set (`:1892`) but **never invokes `verifier::verify_class` or `bytecode_verifier::verify_bytecode`**. Any caller of `define_class` (`Unsafe.defineClass`, `MethodHandles.Lookup.defineClass`) gets a structurally valid but unverified class.
- `bootstrap → extension → application` delegation is **hard-coded** (`class_manager.rs:1281-1302`); `ClassFinder` trait has no `parent` link, no `loadClass` analogue.
- 48 `unwrap()` calls in `class_manager.rs` (some test, several production).
- Error propagation in `class_path.rs` uniformly swallowed into `tracing::debug!` (`:378, 384, 395, 466-468, 606-614`). A bad JAR via `-cp` produces zero diagnostics on stderr.

### (1) Vulnerabilities
- **Zip-bomb vector**: every read path preallocates `Vec::with_capacity(entry.size() as usize)` (`class_path.rs:477, 551, 562, 594, 990, 1635, 1718`). `size` is attacker-controlled — a JAR declaring `size = u64::MAX` triggers a multi-gigabyte allocation **before** any read. **No** `max_uncompressed_size` cap, no streaming limit, no ratio check.
- **Zip-slip mitigation incomplete** — when *extracting fat-JAR entries* (`extract_fat_jar_entries`, `:541-574`), entry names go into `entries_cache` verbatim. No validation against `..`, absolute paths, NUL bytes. Same for `load_jmod` (`:1715-1724`) and `extract_jar_signer_blocks` (`:973-997`). In-memory only, so no on-disk write, but resource-namespace poisoning is possible.
- **Multi-release shadow attack** (`class_path.rs:343-355`) — walks `../../../apps/META-INF/versions/{N}/...` from 25 down to 9 **regardless of the running JVM's release** and **regardless of whether the manifest declares `Multi-Release: true` for the specific class**. A JAR's `versions/25/java/lang/String.class` shadows the base entry.
- **No JAR signature verification** — `extract_jar_signer_blocks` reads `.RSA`/`.DSA`/`.EC` blocks as opaque bytes and attaches them to `CodeSource`. No manifest-digest verification, no `.SF` parsing, no PKCS#7, no cert-chain validation. Signed JARs are trusted on filename alone.
- **Symlink-traversal check is fail-open** — on `canonicalize` failure the check is silently skipped and the read proceeds (`class_path.rs:725-738`).
- NUL-byte / backslash check inconsistent across `find_class_source_path` / `find_class_code_source_info` (`:834, 912`) vs the resource paths.

### (2) Stubs / Unimplemented
- No `TODO`/`FIXME`/`unimplemented!()` markers.
- **Classloader hierarchy is the roadmap blocker.** `ClassFinder` has no parent link, no Java-side `ClassLoader` reference, no `findLoadedClass`/`defineClass` round-trip. Delegation is hard-coded in 3 lines. Custom Java-side loaders cannot participate.
- **Module-system enforcement** is name-keying only — `scan_module_infos` collects every `module-info.class`, but `loadClass` does not consult module-readability or package-visibility.
- PKCS#7 / cert-chain decoding explicitly punted (`class_path.rs:898-904`).

### (3) Performance
- Manifest re-read per JAR after the fat-JAR probe (`class_path.rs:435` + `448-450`).
- `probe_fat_jar_structure` iterates only `0..archive.len().min(100)` (`:489-499`) — false-negatives on JARs where interesting entries are at index > 100.
- `find_in_archive` takes the archive `Mutex` per class load (`:1629-1643`) — no parallel class loading from a single JAR.
- `find_in_multi_release_archive` does up to **17 `by_name` probes per class lookup** (`:347-352`); cache versioned-name set or test `../../../apps/META-INF/versions/` existence first.
- `find_class_code_source_info` re-canonicalizes the JAR path and re-scans signature blocks on every call (`:936-941, :973-997`).
- `cds_class_cache` (`class_manager.rs:502`) is a byte cache, not a parsed-class cache; parsed `Class` is the expensive object.

### (4) Tests
- 5 test files / 59 `#[test]`s — all annotation, define/redefine, nest-host paths. **None test JAR/JMOD/jimage loading.**
- **No tests** for: multi-release JAR resolution, malformed JARs (truncated central directory, zip-slip filenames, zip-bomb sizes, encrypted entries, ZIP64), conflicting class definitions across two JARs, user-defined classloader behaviour, signed JAR verification, JMOD with corrupted ZIP, jimage corruption.
- Inline tests in `class_path.rs` cover Spring Boot fat-JAR happy path and 4 path-traversal cases. The crate's primary attack surface has no security-boundary coverage.

---

## gc

> Roadmap notes "GC 23x slower on allocation-heavy workloads (Binary Trees, real-world object creation)". The root cause is identified below.

### (0) Code review
- **Cheney scan duplicated 6+ times**: `gen_heap.rs:1071-1390` repeats the same ref-array / Value-slot walk **four times** (Phase 2, Phase 2b, Phase 2.5b promoted scan, `collect_with_finalizers`). Near-identical copies in `gc.rs:137-204`, `gc.rs:328-438`, `old_gen.rs:369-415`, `concurrent_mark.rs:347-385`. Bug fixes must land in 6+ places.
- `heap.rs:780-792` and `gen_heap.rs:1800-1811` use `process::abort()` for OOM — kills the entire VM with no Java-side `OutOfMemoryError`. `try_alloc_*` plumbing exists but the panicking wrappers are still on hot paths.
- `gen_heap.rs:520-602` — `get_field` silently returns `Value::Object(None)` on `index >= num_slots`; `set_field` silently drops. Masks layout-mismatch bugs.
- `gen_heap.rs:608-626` — `volatile_lock: Mutex<()>` serializes **every volatile read/write on the entire heap**. Replace with striped lock keyed on `obj_ref >> 3 & N-1`.
- `reference.rs:125-145` `remove_timeout` and `heap.rs:982-996` `wait_for_gpu_critical` use `yield_now()` spin instead of `Condvar`.
- `numa.rs`, `zgc.rs`, `metaspace.rs`, `class_unloading.rs` — large modules with no inbound call sites from the active heap path; appear to be dead.

### (1) Vulnerabilities
- Cheney scan reads `Value` via `std::ptr::read` (16 bytes) without checking the tag (`gc.rs:182`, `gen_heap.rs:1137`); a torn `Value::Object` during volatile access on another code path makes the GC follow a garbage pointer.
- Write barrier (`gen_heap.rs:836-858`) reads target object header without holding any lock; safe only during STW. On a real concurrent GC this is a data race.
- **Integer overflow in heap accounting** — `HEADER_SIZE + num_fields * SLOT_SIZE` uses unchecked `*` (`gen_heap.rs:243-244, 269, 334`). The `try_alloc_*` variants exist but the panicking versions allow attacker-controlled `num_fields` from a malformed class file to cause UB.
- Forwarding pointer race (`gen_heap.rs:1969-1984`) — installs `forwarding_ptr` in OLD header **after** the new copy is made; a parallel marker (concurrent GC) reading in between sees a non-forwarded object and scans stale fields.

### (2) Stubs / Unimplemented
- **ZGC (`zgc.rs`)** — colored pointers + barriers implemented but `lib.rs:38` does not re-export anything; not wired into `VmHeap`. Effectively dead.
- **NUMA (`numa.rs`)** — not exported. Dead.
- **`class_unloading.rs`** — not exported. Dead.
- **Concurrent marking** wired into `GenerationalHeap` (via `enable_concurrent_gc`) but `collect_garbage_inner` only runs the STW Cheney path. The "concurrent" machinery is built but not driven.
- **Reference processing**: Soft/Weak/Phantom/Finalizer implemented in `reference.rs` but **Soft and Weak processing is not invoked from the minor-GC path**.
- **Finalizer queue for G1** (`vm_heap.rs:440-446`) returns empty `Vec::new()` for dead finalizers — silent finalizer skip.

### (3) Performance — root cause of the 23x regression
1. **`gen_heap::alloc_object` / `alloc_array` never consults a TLAB** (`gen_heap.rs:1750-1764`). Every single object hits the global `young_from` mutex. TLABs exist (`tlab.rs`) and the refill API is wired (`gen_heap.rs:1780-1792`) but the allocation API bypasses them entirely. **This alone explains a >10x slowdown** vs HotSpot's per-thread bump pointer.
2. Per-object zero-fill (`gen_heap.rs:1759`) — would be redundant if TLAB pre-zeroed the slab.
3. Write barrier (`gen_heap.rs:836-858`) locks the **global** `card_table.lock()` on every reference store. The thread-local card buffer (`card_table.rs:175-196`) exists but is unused.
4. **`pointer_map: HashMap<usize, usize>`** (`gen_heap.rs:983`, `gc.rs:117`) uses default SipHasher and grows from empty each GC. For Binary Trees (~100k live objects per GC) this is ~3 ms wasted per cycle. `rustc-hash` is already a dependency — use `FxHashMap`.
5. **O(N^2) promoted scan**: `gen_heap.rs:1164-1170, 1327-1332` allocates a fresh `Vec` and iterates `pointer_map.values()` doing `old_gen.contains` per entry, per loop iteration.
6. **`HashSet<usize>` for `scanned_promoted`** (`:1082`) — use the `GC_FLAG_MARKED` header bit instead of a side set.
7. STW duration: `scan_dirty_cards` (`:2002-2057`) walks the **entire** old gen for every dirty card; worst-case ~10 ms for a 128 MB old gen with a single dirty card.

### (4) Tests
- **No allocation-storm benchmarks.** Zero `criterion` config; no `[[bench]]` in `Cargo.toml`. The 23x regression is invisible to the test suite.
- **No multi-threaded GC tests.** All `collect_garbage` tests single-threaded; no concurrent alloc + GC.
- **No long-running soak tests.** Max ~5 GC cycles in any test.
- **No fragmentation regression test for `OldGen`.**
- **No write-barrier completeness test** — only one card-dirty assertion exists.
- TLAB tests are extensive at the unit level but **never tested against a real `GenerationalHeap` allocation flow** — consistent with the integration gap (TLABs aren't wired in).
- No `proptest`/`loom` race detection.

### Highest-impact fixes for the 23x regression
1. Wire TLABs into `GenerationalHeap::alloc_object`/`alloc_array` fast path.
2. Switch `pointer_map`/`scanned_promoted` to `FxHashMap`/`FxHashSet`.
3. Use a marker bit instead of `HashSet<usize>` for promoted tracking.
4. Activate the thread-local card buffer; drain at safepoint.
5. Add a `criterion` benchmark suite and gate PRs on it.

---

## vm

### (0) Code review
- **`vm/src/vm.rs` is 62 969 lines with 1 566 functions** in one file. `runtime/interpreter.rs` is 14 300; `vm/vm_exec.rs` 8 051; `vm/vm_init.rs` 7 603. Split is required.
- **Two-path interpreter dispatch** (`runtime/interpreter.rs:2148`) — fast `match` vs slow `Instruction::decode`, gated by `is_jdk_class` (`frame.rs:256-262`). The substring-match against `"springframework"` at `frame.rs:261` to disable the fast path for Spring bytecode is a striking smell.
- **`interpreter::execute`** (`interpreter.rs:1040`) is ~1 100 lines that reads `CRATONVM_BD_DEBUG`/`CRATONVM_IAE_TRACE` env vars on every call.
- **Bring-up scaffolding never deleted** — `eprintln!` for "S111r*", "C29 trace", "WF-NPE-TRACE", "SUREFIRE-NPE-TRACE", "Round 34" across `exceptions.rs:194-275`, `vm.rs` (x61), `vm_init.rs` (x68). Production hot paths walk the full frame chain on every NPE/IAE.
- **Wrong error variants**: stack underflow / overflow in `value_stack.rs:240-308` use `RuntimeError::NotImplemented { feature: "operand stack overflow" }`, which maps to `MethodCallFailed::InternalError` and bypasses Java `catch` handlers.

### (1) Vulnerabilities
- **`as i32` truncation in AIOOBE** (`interpreter.rs:4750-4753`) — cast through `usize` and back wraps for arrays of length ≥ 2^31.
- **Monitor diagnostic strings allocated unconditionally on hot path** (`interpreter.rs:6388-6420`) — `class_name().to_string()` + `method_name().to_string()` + `format!(...)` before every monitor acquire, plus `Instant::now()` and a `flight_recorder.lock()` per contended enter. DoS-amplifier under heavy `synchronized` traffic.
- **GC barrier has no timeout** (`gc_barrier.rs:82-87`) — `wait_for_all` blocks on `Condvar::wait` with no deadline. Any peer thread failing to reach safepoint deadlocks the initiator.
- **`MonitorTable` keys monitors by raw `obj_ref.as_ptr() as usize`** (`monitor.rs:354`); relies on `remap_after_gc` to re-key (`:516`). The rekey holds `monitors.lock()` across the entire drain, blocking **every** monitor op for the duration of GC.
- **Production `.unwrap()`** in `coerce_arg`/`coerce_return` (`interpreter.rs:8359, 8364, 8426, 8432`) — `is_primitive_desc("")` is `false` so they're technically sound, but the CI gate (`interpreter.rs:13363`) is supposed to forbid `.unwrap()` and these survived.
- `pop_unchecked`/`push_unchecked` panic on miscount (`value_stack.rs:217, 319`) — any future fast-path extension hitting real JDK code panics the VM thread.

### (2) Stubs / Unimplemented
- The hot files (`interpreter.rs`/`vm_exec.rs`) are CI-gated against `unimplemented!`/`todo!`/`panic!`. True stubs are in: `runtime/offload.rs` (7), `runtime/gpu_marshal.rs` (8), `runtime/lockfree_resolve.rs` (3), `threading/varhandle.rs`, `threading/event_loop.rs`.
- `RuntimeError::NotImplemented` overloaded as fallback exception class — used for missing classes *and* stack underflow/overflow.
- `Jsr`/`Ret` handle only the one `ReturnAddress(u32)` tag; verifier mismatch throws opaque `InternalError`.

### (3) Performance
- **Hot-path env-var lookups** — 32 in interpreter alone (`interpreter.rs:1048, 1053, 1345, ...`). Every method entry pays for `getenv` syscalls. Cache at VM init into `Once<bool>` or feature-gate.
- **`eprintln!` and full frame walks in `throw_runtime_error`** (`exceptions.rs:194-275`) — every NPE construction walks 15-40 stack frames + `class_manager.read()` per frame, even when no env var is set (empty `for` loop still pays iteration cost).
- **Monitor enter records JFR + `Instant::now()` every time** (`interpreter.rs:6394-6411`) — move the >1 ms threshold check before the system-time fetch.
- **Recursive interpreter** — every `invoke*` opcode recurses through `invoke_on_class_shared → execute → execute_frame` on the host stack. The `max_stack_depth=1024` cap exists to match Rust's stack budget; **this is the root cause of "java.util.stream stack-overflow"** in the roadmap.
- **Two encode/decode round-trips per `Aload`** (`interpreter.rs:4721-4732`) — `get_local → Value → coerce_value_for_return → push(Value)`.
- `MonitorTable::get_or_create` (`monitor.rs:353`) takes a global `Mutex` per `monitorenter` — sharded map would reduce contention.

### (4) Tests
- **`tests/interpreter_tests.rs`** has 362 `#[test]`s; **all conditional on `class_files_available()`** (`:33-41`) and silently `return` if `javac` is missing. CI must guarantee `javac` or coverage drops to zero — **silent skip is invisible in CI today**.
- Exception-table handler tests (nested try/catch, multi-catch, finally-with-return) exist but skip with same `javac` gate.
- 23 unit tests in `threading/monitor.rs`; **Java-level `synchronized`/`wait`/`notify` tests** are absent from the dedicated tests directory.
- **No threading correctness stress test** — no concurrent `monitorenter` from many OS threads, no GC-during-monitor-rekey, no `GcBarrier::wait_for_all` timeout test.
- `tests/differential.rs` exists but no cross-product over the opcode space.

### Top priorities
1. Split `vm/src/vm.rs` — actively impedes any work in the crate.
2. Purge `eprintln!`, `CRATONVM_*_TRACE`, and dead-trace frame walks from the NPE/IAE construction path.
3. Fix `RuntimeError::NotImplemented` misuse for stack under/overflow.
4. Add a timeout to `GcBarrier::wait_for_all`.
5. Complete the iterative-interpreter refactor so streams stop blowing the Rust stack.
6. Make `javac` missing a CI failure, not a silent skip.

---

## vm-cli

### (0) Code review
- **2 071-line single `main.rs`** — arg struct, three pre-clap rewriters, watchdog, exception renderer, panic hook all in one file. `run()` alone is ~1 100 lines.
- **clap usage anti-pattern**: half the JVM CLI surface (`-D`, `-XX:+`, `-XX:-`, `-XX:Foo=`, `-agentlib:`, `-javaagent:`, `-classpath`, `-cp`, `-jar`, `-mp`) is stripped or rewritten before clap parses. `--help` is misleading.
- **Mojibake corruption throughout doc comments** at `main.rs:11, 164, 268, 463, 811, 822, 870, 876` — em-dashes round-tripped through Windows-1251. Line 11 is the `#[command(about)]` source, so `--help` output ships the corruption.
- Magic numbers without constants: default watchdog `120` (`:910`), grace `3` (`:941`), `64 * 1024 * 1024` stack size (`:1733`), cause-chain depth `8` (`:1332`).

### (1) Vulnerabilities
- **No path validation on user-supplied file paths** — `--XX:SharedArchiveFile`, `--XX:AOTCache`, `--XX:AOTCacheOutput`, `--dump-missing-natives*`, `--Xbootclasspath`, `--java-home`, `--jar`, `-XX:HeapDumpPath=` are read/written verbatim with no canonicalization or working-directory confinement.
- **`CLASSPATH` env var unconditionally honored** (`:653`) with no log line.
- **`CRATONVM_DEFAULT_WATCHDOG_SEC` / `CRATONVM_DISABLE_DEFAULT_WATCHDOG`** read with no validation — malicious env can disable the only hang protection.
- **JAR manifest `Class-Path` entries appended without validation** (`:614`) — malicious JAR can pull arbitrary relative paths into the classpath. (Matches `java`; document as trust boundary.)
- System property keys/values have no length limit (`:436-447`).

### (2) Stubs / Missing
- No `TODO`/`FIXME`/`unimplemented!()` in this crate.
- **Missing `java` flags vs HotSpot**: `-Xms`, `-Xss`, `-Xrs`, `-Xint`, `-Xcomp`, `-Xbatch`, `-Xfuture`, `-server`/`-client`, bare `-version`, `--enable-preview`, `--enable-native-access`, `--source`, `-ea`/`-da`/`-esa`/`-dsa`, `-XX:+UseG1GC` and GC selection, `-XX:MaxMetaspaceSize`, `-XX:MaxDirectMemorySize`, `-XX:ReservedCodeCacheSize`, `-verbose:jni`, `-?` short, `@argfile` (JEP 343).

### (3) Performance
- **No `--version` fast path.** `main.rs:1600` unconditionally installs panic hook, spawns 64 MB stack thread, initializes `tracing_subscriber`, preparses argv three times before clap can detect `--version`. Peek argv[1] first.
- **Default 64 MB stack** allocated for every invocation including `--help` (`:1733`).
- **Three sequential argv copies** in preparse (`:354, 412, 475`).
- Watchdog thread spawned unconditionally for `--help`/`--version`/HelloWorld (`:915-1043`).

### (4) Tests
- Inline `#[cfg(test)]` covers `parse_size`, `validate_class_name`, `extract_system_properties`, `normalize_java_launcher_argv`, `extract_hotspot_flags`, `expand_aggregate_jars`.
- **No subprocess / `assert_cmd` integration tests.** The dual-binary aliasing (`cratonvm` + `java`) is untested.
- **No tests for the `run()` orchestration** — exception renderer (300 LOC), watchdog (200 LOC), `-jar` Quarkus classpath synthesis, JDWP wiring, JPMS flag forwarding, HotSpot agent option splitting, `--` separator handling, AOT/CDS mode mapping, init-level transitions — **all untested**.
- No fuzz on the three preparse rewriters.

---

## jfr

### (0) Code review
- **`parking_lot` is in `Cargo.toml:13` but never used.** Dead dependency.
- **No concurrency at all.** `FlightRecorder::record_event` (`recording.rs:196`) takes `&mut self`. The dual `record_event` / `record_event_arc` API + `Arc::try_unwrap` dance at `recording.rs:106` is performance theater — `try_unwrap` always succeeds because there is no cross-thread sharing.
- Unused `HashMap`/`HashSet` imports at `recording.rs:1`, `repository.rs:1`, `stream.rs:11` (FxHash variants are used in non-test code).
- **Ring-buffer `type_index` correctness fragility** in `repository.rs:42` — depends on `swap_remove` ordering; not exercised by the test pushing 4 same-type events into capacity 2.
- **Massive duplication in `builtin.rs`** — 35+ near-identical `emit_*_event` helpers (`:810-2200`). A macro would reduce ~1 400 lines to ~200.

### (1) Vulnerabilities
- **Unbounded growth on string fields** — `EventValue::String(Arc<str>)` with no length cap. Long `monitor_class`, `path`, `commandLine`, `message` from `emit_java_exception_throw_event` (`builtin.rs:1630`) sit in the ring until eviction.
- **`max_size` / `max_age` settings ignored** — stored on `RecordingSettings` but enforced nowhere. Only limit is hard-coded `100_000` in `EventRepository::default()`.
- **Log-injection via event names** — event-type names written verbatim into JFR metadata (`dump.rs:233-235`); `EventTypeRegistry::register` is `pub`, so external callers can inject any string. No control-char/length validation.
- **`u64 as i64` truncation** at `dump.rs:174-176` — `start_time`, `duration`, `thread_id` silently wrap negative for values ≥ 2^63.

### (2) Stubs / Unimplemented
- No `TODO`/`unimplemented!()` markers, but **JFR format conformance is stubbed**:
  - `build_metadata_section` (`dump.rs:209`) explicitly states *"Real JFR metadata uses a complex XML-like structure stored in binary. We use a simplified but compatible format"* — the metadata is **not loadable by `jfr print` / JMC / `jdk.jfr.consumer.RecordingFile`**. Calling this "JFR v2.0 binary format" in the file header is misleading.
  - `build_checkpoint_section` (`dump.rs:281`) writes a checkpoint with **zero constant pools**. Real JFR uses constant pools heavily for strings, thread names, stack traces, class info. Every string is inlined per-event.
  - **No stack trace support** anywhere: `EventInstance` has no `stack_trace_id` field. `has_stacktrace: true` event types (`builtin.rs:206, 261, 290, ...`) cannot actually carry a stack trace.
- **48 builtin event types registered**, only ~35 have emit helpers; **13 registered but never emitted** (`jdk.ObjectAllocationSample`, `jdk.NativeMethodSample`, `jdk.ExceptionStatistics`, `jdk.ModuleRequire`, `jdk.ModuleExport`, `jdk.PhysicalMemory`, `jdk.ContainerCPUUsage`, `jdk.ContainerMemoryUsage`, `jdk.SystemProcess`, `jdk.InitialEnvironmentVariable`, `jdk.GCReferenceStatistics`).
- **Major JFR event types missing**: `jdk.NativeLibrary`, `jdk.JVMInformation`, `jdk.OSInformation`, `jdk.CPUInformation`, `jdk.G1HeapSummary`, `jdk.ZAllocationStall`, `jdk.SecurityProperty`, `jdk.TLSHandshake`. No `jdk.ActiveSetting` per-type settings.

### (3) Performance
- **Per-event allocation explosion in builtins** — every emit builds a `Vec<EventValue>` (~80–200 bytes), `Arc::from(&str)` for each string field, then clones into the repository. At 100k events/sec this is a serious allocator hot path.
- **No string-pool interner** — `monitorClass`, `objectClass`, `method`, `path` repeated across millions of events but re-allocated.
- **`find_by_name` on every emit** — String hash lookup per call site. Cache `EventTypeId`.
- **`events_by_type` allocates a `Vec` per call** (`repository.rs:97`). Return an iterator.
- **`stream::next_event`** uses `repo.iter().nth(rel)` in a `while` loop — O(n^2) (`stream.rs:214`).

### (4) Tests
- **No emit→parse round-trip for the dumped file** for all event types. `dump.rs` tests verify header bytes only.
- **No JDK conformance tests** — nothing verifies the file is loadable by `jfr print` or `RecordingFile`. Given the metadata is stubbed, the answer is "it isn't", but no test asserts either way.
- **No tests for `max_size` / `max_age` enforcement** — because the features don't exist.
- **No tests for concurrent emission** — appropriate since the crate isn't thread-safe, but the constraint isn't enforced via `static_assert_not_impl_any!(FlightRecorder: Sync)`.
- **No fuzz / malformed-input tests** for `read_events` or `read_jfr_header`.
- ~150 tests but skewed toward trivial getter/setter/Debug-derive checks.

---

# Cross-Cutting Themes

The 16 reviews surface recurring patterns. Treat these as workstreams that cut across crates:

### Theme C-1 — "Silently degrade" instead of "throw"
Multiple crates default to returning `Option::None`/empty/`Ok` where Java semantics require an exception:
- `native-collections` `native_al_get`/`set`/`add_at` return null on OOB instead of `IndexOutOfBoundsException`.
- `gen_heap` `get_field`/`set_field` silently drop on layout mismatch.
- `native-api` `NativeContext` defaults fail-open on security-relevant predicates.
- `reader` `decode_value` silently degrades unaligned `VTAG_OBJECT` to `Object(None)`.
- `jfr` `Builder::set` silently `eprintln!`s on typos.
- **Action**: introduce a `policy: strict | lenient` mode and default to `strict` in tests.

### Theme C-2 — Unbounded allocations on attacker-controlled length
Found in `reader` (`tableswitch`/`lookupswitch`, unknown attributes), `classloading` (zip-bomb across 7 sites), `native-io` (7 sites), `jfr` (event strings), and `gc` (`HEADER_SIZE + num_fields * SLOT_SIZE` overflow).
- **Action**: workspace-wide `safe_alloc::checked_with_capacity(len, max)` helper; replace every `Vec::with_capacity(attacker_value)` with a clamped or rejecting variant.

### Theme C-3 — Hot-path lock traffic
- `gc`: global `card_table` mutex per reference store; global `young_from` mutex per allocation; global volatile mutex per volatile op.
- `vm`: global `MonitorTable::get_or_create` mutex per `monitorenter`.
- `native-api`: process-global `parking_lot::Mutex` ring on every native call.
- `native-builtins`: arena `HashMap<i64, Arena>` under `RwLock`; every `Unsafe.get_byte` takes a read lock.
- `native-io`: process-global RAF mutex; outer RwLock + per-entry Mutex on FD table.
- `native-collections`: process-global LHM overlay mutex.
- **Action**: sharded maps, per-thread buffers, atomic rings.

### Theme C-4 — Hot-path environment-variable reads & `eprintln!` debug
- `vm/runtime/interpreter.rs`: 32 env-var reads in the main loop.
- `vm/runtime/exceptions.rs:194-275`: every NPE construction walks 15-40 frames + `class_manager.read()` per frame, even when no env var is set.
- `native-collections`: `[HM-EQ]` `std::env::var` on every HashMap key comparison.
- `vm/src/vm.rs`, `vm/vm_init.rs`: 60+ each of `CRATONVM_*_TRACE` / `eprintln!` blocks.
- **Action**: one-shot init reading all env vars into a global config; feature-flag debug scaffolding; remove dead trace blocks.

### Theme C-5 — Hot-path `String` allocations & string-keyed lookups
- `jit-api`: stringly-typed builder.
- `jit-cuda`: `HashMap<String, CudaFunction>` for kernel lookup.
- `reader`: 30-arm `match` on `&str` for attribute names; many `.to_string()` of CP entries already held as `Arc<str>`.
- `native-builtins`: `StringBuilder.indexOf` allocates twice the string size per call.
- `vm`: monitor diagnostic strings allocated unconditionally per `monitorenter`.
- **Action**: pre-interned IDs at registration / attribute parse; `Arc<str>` everywhere CP-derived; `Cow<'static, str>` in error payloads.

### Theme C-6 — Tests prove registration but not behaviour
- `native-collections`: 60 tests, almost all `r.find(c, "put", ...).is_some()`.
- `native-awt`: 1 test for `natives.rs`, asserts count ≥ 50.
- `jit-api`: tests assert raw magic-number counts of fields rather than computing from `field_names().len()`.
- **Action**: enforce "registration ⇒ behavioural test" via a per-crate coverage gate.

### Theme C-7 — JVM safety holes in the JIT/GC interface
- `jit`: `oop_maps` not emitted; conservative stack scan is the only fallback.
- `jit`: `i64::MIN` deopt sentinel collides with valid values.
- `gc`: forwarding-pointer race installs forwarding after the copy, exposing a window where a concurrent marker scans stale fields.
- `vm`: monitor table rekeyed under a global lock that blocks every monitor op during GC.
- **Action**: a Phase dedicated to GC-JIT safety (oop maps, deopt sentinel out-of-band, atomic forwarding install).

### Theme C-8 — Verification is recordkeeping, not gating
- `classloading::define_class_with_options` records `skip_verification` in a side set but **never calls the verifier**. Any caller of `Unsafe.defineClass` / `MethodHandles.Lookup.defineClass` gets unverified bytecode.
- `native-builtins` Unsafe paths explicitly `skip_verification: true`.
- `native-io` path validation is globally toggleable from any code in the process.
- **Action**: gating must happen at the chokepoint, not by convention.

### Theme C-9 — Recursive interpreter blocks stream-heavy workloads
- `vm`: roadmap calls out `java.util.stream` stack-overflow; interpreter still recurses through Rust frames per `invoke*` despite the `Iterative Interpreter Refactor` test (`vm_init.rs:7908-7935`).
- **Action**: finish Phase 16.1 (Iterative Interpreter) — already on the roadmap above.

### Theme C-10 — Single-file giants block refactoring
- `vm/src/vm.rs` (62 969 lines), `runtime/interpreter.rs` (14 300), `jit/src/x64.rs` (17 643), `native-collections/src/lib.rs` (19 576), `native-builtins/src/lib.rs` (34 736), `native-io/src/lib.rs` (12 960), `classloading/src/class_path.rs` (3 152), `vm-cli/src/main.rs` (2 071).
- **Action**: split before any meaningful work proceeds; these files dwarf the diff radius and produce merge-conflict storms.

---

# Roll-up: Top 15 Workstream-Sized Items

Ordered by user-visible impact:

1. **TLAB wiring in `gc::GenerationalHeap::alloc_object`/`alloc_array`** + thread-local card buffer. Addresses 23x allocation regression.
2. **Iterative interpreter (Phase 16.1)** — unblocks `java.util.stream` workloads.
3. **JIT `frem`/`drem` codegen + IR back-edge merge fix + branch-target stack canonicalization** — Phase 16.2/16.3 root-causes.
4. **JIT oop-map emission + deopt sentinel out-of-band** — JVM-safety holes.
5. **Classloader hierarchy (`ClassFinder` parent link, `loadClass` protocol)** — unblocks WAR/multi-JAR apps.
6. **Verifier invocation in `define_class_with_options`** — close the unverified-class hole.
7. **Workspace `safe_alloc` helper** + wire to `reader::tableswitch`/`lookupswitch`, `classloading` zip paths (zip-bomb cap), `native-io` length-prefixed reads, `jfr` event strings.
8. **`gc` write-barrier path** — replace global `card_table` mutex with thread-local card buffer drained at safepoint.
9. **`native-builtins` arena `read::<N>` offset bug fix + `arraycopy` bulk intrinsic + ArrayStoreException semantics**.
10. **Remove `native-collections` (`synthetic-jdk` feature)** — already on Definition-of-Done.
11. **Split `vm/src/vm.rs` (62 969 lines)** and `jit/src/x64.rs` (17 643 lines) into submodules.
12. **Purge `CRATONVM_*_TRACE`/`eprintln!` from `vm/runtime/exceptions.rs` + `vm.rs` + `vm_init.rs`** — hot-path cleanup.
13. **JFR JDK-tool conformance pass** — current dialect is unreadable by `jfr print`/JMC; fix `build_metadata_section` and add constant pools.
14. **`CompactValue` 47-bit pointer cfg-gate** — silent corruption on non-x86_64 / 5-level paging.
15. **`GcBarrier::wait_for_all` deadline + `MonitorTable` shardable rekey** — current paths deadlock the VM under safepoint failure or GC.

---

# Audit metadata

- **Run date**: 2026-05-16
- **Method**: 16 parallel agents (one per crate); 5 on Opus (reader, types, native-api, jit-api, cuda-bridge), 11 on Sonnet (re-launched after first batch hit the Opus daily quota mid-run). Findings cross-checked against file/line references; quotes preserved verbatim.
- **Scope**: source in `<repo>/<crate>/{src,tests}` and `Cargo.toml` only. Did not attempt builds, runs, or external benchmarks during the audit.
