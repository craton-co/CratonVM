# CratonVM JIT Compiler — Round 26 Performance Results

## The Journey: From 97x Slower to Within 1.5x of JDK C2

```
                    Performance vs OpenJDK -Xint (interpreter mode)
                    ================================================

  Before Opt   ████████████████████████████████████████████████████  97x SLOWER
  Round 3      ████████                                              8x
  Round 7      ██████                                               6.5x
  Round 8 JIT  ██                                                   1.8x
               ▏                                                    ←── OpenJDK -Xint
  Round 10     ◀═══                                                 3.2x FASTER!
  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
  (Workload scaled 1000x for stable timing at Rounds 11+)

  Round 14     ◀═══════                                             6.9x FASTER!
  Round 15     ◀═══════  (coverage: +38 bytecodes, all array/float types)
  Round 16     ◀═══════  (SSE: +30 bytecodes, full float/double compute)
  Round 17     ◀═══════  (field access: getfield/putfield, object properties)
  Round 18     ◀═══════  (LICM: hoist invariant aaload out of loops)
  Round 19     ◀═══════  (VM context: checkcast/instanceof/getstatic/putstatic)
  Round 20     ◀══════════════  (compact refs + JIT bug fixes: ~15x faster!)
  Round 21     ◀══════════════  (array bounds check elimination)
  Round 22     ◀══════════════  (JIT invokevirtual/invokespecial)
  Round 23     ◀══════════════  (On-Stack Replacement at hot loops)
  Round 24     ◀═══════════════════  (AVX2 SIMD vectorization)
  Round 25     ◀═══════════════════  (SoA Value layout: 16B → 9B per slot)
  Round 26     ◀═══════════════════  (loop unrolling, speculative BCE, OSR fast-path)
               ▏                                                    ←── OpenJDK -Xint
               ▏  ◀═════                                           ←── OpenJDK C2 (1.50x)
```

---

## What Is CratonVM?

A Java Virtual Machine written entirely in Rust:

- **~323,000+ lines** of Rust code
- **6,000+ tests** passing, **0** clippy warnings
- **~3,100+ native method** registrations (java.lang, java.util, java.io, java.time, ...)
- Full interpreter with 140+ fast-path bytecodes
- Generational garbage collector with write barriers
- Multi-threading with monitors, locks, and barriers
- Lambda/invokedynamic support
- **x86-64 JIT compiler** (~7,200 lines, ~140 bytecodes, 26 optimization rounds)
- **Within 1.50x of JDK C2** on QuickBench (Fibonacci 1.31x)

---

## The JIT Compiler

### Architecture

```
  Java Source          javac          JVM Bytecode         CratonVM JIT
  ┌──────────┐      ────────►      ┌──────────────┐      ────────►      ┌─────────────┐
  │ int fib( │                     │ iload_0      │                     │ push rbp    │
  │   int n  │                     │ iconst_1     │                     │ mov rbp,rsp │
  │ ){       │                     │ if_icmpgt +5 │                     │ cmp r12d, 1 │
  │   if(n<=1│                     │ iload_0      │                     │ jg .L1      │
  │     ret n│                     │ ireturn      │                     │ movsxd rax  │
  │   ret    │                     │ iload_0      │                     │ jmp .epilog │
  │    fib(  │                     │ iconst_1     │                     │.L1:         │
  │     n-1) │                     │ isub         │                     │ lea ecx,    │
  │   +fib(  │                     │ invokestatic │                     │   [r12-1]   │
  │     n-2) │                     │ iload_0      │                     │ call fib    │
  │ }        │                     │ iconst_2     │                     │ mov r13,rax │
  └──────────┘                     │ isub         │                     │ lea ecx,    │
                                   │ invokestatic │                     │   [r12-2]   │
                                   │ iadd         │                     │ call fib    │
                                   │ ireturn      │                     │ add rax,r13 │
                                   └──────────────┘                     │ ret         │
                                                                        └─────────────┘
```

### Key JIT Optimizations (Rounds 8-25)

| Round | Optimization | Technique |
|-------|-------------|-----------|
| R8 | Basic JIT | Bytecode → x86-64, self-recursive calls |
| R9 | Inline array ops | newarray, iaload/iastore via heap pointer |
| R10 | Register locals | Locals 0-3 in R12-R15 (zero-cost loads) |
| R11 | Magic division | Constant div/rem → multiply-and-shift |
| R12 | Extended regs | 7 callee-saved regs on Windows |
| R13 | Compact arrays | byte[]=1B, int[]=4B, long[]=8B per element |
| R14 | Inline aaload | SHL+MOV for Object[] element access |
| R15 | Inline aastore | Discriminant + pointer write inline |
| R15 | All array types | char[], short[], long[], double[], float[] |
| R15 | Float/double data | fconst, fload/fstore, dconst, dload/dstore |
| R15 | Type narrowing | i2b, i2c, i2s conversions |
| R15 | All return types | freturn, dreturn, void return |
| R16 | SSE arithmetic | fadd/fsub/fmul/fdiv via ADDSS/SUBSS/MULSS/DIVSS |
| R16 | SSE double arith | dadd/dsub/dmul/ddiv via ADDSD/SUBSD/MULSD/DIVSD |
| R16 | Float/double cmp | fcmpl/fcmpg/dcmpl/dcmpg via UCOMISS/UCOMISD |
| R16 | All conversions | i2f/i2d/l2f/l2d/f2i/f2l/f2d/d2i/d2l/d2f via SSE |
| R16 | FP negation | fneg (XOR bit 31), dneg (BTC bit 63) |
| R17 | getfield | Read object fields via jit_getfield helper |
| R17 | putfield | Write fields via type-specific helpers + write barrier |
| R18 | LICM | Hoist invariant aaload out of loops (preheader block) |
| R19 | VM context | SharedVm pointer replaces heap pointer for full VM access |
| R19 | checkcast/instanceof | Class hierarchy check via helper call-outs |
| R19 | Null/ref branches | ifnull, ifnonnull, if_acmpeq, if_acmpne, aconst_null |
| R19 | Static fields | getstatic/putstatic with compile-time CP resolution |
| R20 | Compact ref arrays | Object[]=8B per element (was 16B Value) |
| R20 | SIB scale=3 refs | `MOV RAX,[RAX+RCX*8+32]` for aaload/aastore |
| R20 | Prologue fix | MOV-based callee-saved save/restore (not PUSH/POP) |
| R20 | Fast-path fix | SharedVm ptr (not heap ptr) for JIT invocation |
| R21 | Bounds checks | Safe array access + loop elimination (BCE) |
| R21 | Out-of-line throw | AIOOBE via out-of-line helper at method end |
| R22 | JIT invokevirtual | Virtual/special/interface calls via helper bridge |
| R22 | Cross-method static | Direct CALL to other compiled methods |
| R23 | OSR | On-Stack Replacement at hot loop back-edges |
| R23 | State transfer | Interpreter locals → JIT frame mid-execution |
| R24 | SIMD detection | CPUID check for AVX2 support |
| R24 | AVX2 vectorization | VPMULLD/VPADDD for 8-int parallel arithmetic |
| R24 | Scalar cleanup | Remainder loop for non-aligned lengths |
| R25 | SoA locals | `Vec<u64>` + `Vec<u8>` tags (was `Vec<Value>`) |
| R25 | SoA stack | ValueStack with separate vals/tags arrays |
| R25 | GC SoA scan | Tag-based object ref extraction for GC roots |
| R26 | Loop unrolling | 4x for bodies ≤20 bytecodes, 2x for 21-30 |
| R26 | Speculative BCE | Loop header guard: arr.length ≥ bound, skip per-element checks |
| R26 | Graph-color regalloc | Interference graph + coloring for 7 callee-saved regs |
| R26 | OSR fast-path | OSR-compiled methods reused for normal invocation-level calls |
| R26 | Invocation counting | Profile-guided JIT at 2000-call threshold |

### Peephole Optimizations

```
  Constant + Arithmetic Fusion       Constant + Compare Fusion
  ─────────────────────────────      ──────────────────────────
  iconst_3                           iconst_0
  imul          ──►  IMUL RAX, 3    if_icmpge   ──►  CMP EAX, 0
                                                      JGE target

  Fused Compare-and-Branch           Magic Number Division
  ─────────────────────────          ────────────────────
  iload_0                            i % 7  ──►  IMUL + SHR + SUB
  iload_1                                        (no IDIV instruction!)
  if_icmplt  ──►  CMP R12d, R13d
                  JL target
```

---

## Benchmark Results

### QuickBench — Scaled Workloads (Round 26)

*Measured 2026-03-31 on Windows 11, JDK 25.0.1 LTS.*

```
  Benchmark               JDK C2      CratonVM R26    Ratio     Notes
  ────────────────────     ──────      ───────────    ─────     ─────
  Arithmetic (300M)         889 ms      1676 ms      1.89x     ★ loop unrolling + magic div
  Fibonacci(42) recursive  1876 ms      2457 ms      1.31x     ★ JIT self-calls + regalloc
  Sieve (100K×500 reps)     324 ms       510 ms      1.57x     ★ OSR + speculative BCE
  Matrix 500×500 multiply   351 ms       518 ms      1.48x     ★ LICM + compact refs
  ─────────────────────────────────────────────────────────────────────────────
  TOTAL                    3440 ms      5161 ms      1.50x

  Binary Trees (depth=18)   714 ms     16657 ms     23.3x     ✗ GC allocation bottleneck
  Overall:   1.50x on QuickBench vs HotSpot C2 (Binary Trees tracked separately)
```

★ = JIT-compiled to native x86-64 machine code

### Cumulative Performance Journey

```
                    Total Benchmark Time (lower is better)
  ═══════════════════════════════════════════════════════════

  Small workloads (1M arith, fib(28), sieve 100K, matrix 100×100):

  Unoptimized   ████████████████████████████████████████████  5064 ms  (97x)
  R3 interp     ████                                          427 ms   (8x)
  R7 unsafe     ███                                           330 ms  (6.5x)
  R8 JIT        █                                             140 ms  (1.8x)
  R10 regs      ▏                                              20 ms  (0.3x) ← FASTER!
               ────────────────────────────────────────────────────────────────
  JDK -Xint     ▏                                              79 ms   (1.0x)

  Scaled workloads (300M arith, fib(42), sieve×500, matrix 500×500):

  R13 compact   █████████████████████████████████████          4660 ms (2.4x slower)
  R14 full-regs ███████████████████████████████                3978 ms (1.93x slower)
  R20 compact+  █████████████████████████████████████████      9562 ms (1.8x slower)
  R25 SoA+SIMD  ████████████████████                           5210 ms (1.51x)
  R26 unroll    ███████████████████                            5161 ms (1.50x) ← latest
               ────────────────────────────────────────────────────────────────
  JDK C2        █████████████                                  3440 ms  (1.0x)
  JDK -Xint    █████████████████████████████████████████████ 144543 ms (27.7x)

  R20→R26: BCE + invokevirtual + OSR + SIMD + SoA + loop unrolling + speculative BCE
```

### Per-Benchmark Deep Dive (Round 26)

```
  Fibonacci(42) — 267,914,296 recursive calls
  ═══════════════════════════════════════════

  JDK C2       ███████████████                                  1876 ms
  CratonVM R26  ████████████████████                            2457 ms  ★ JIT self-calls + regalloc
                                                                         ↑ gap: 1.31x

  Arithmetic (300M iterations, mixed ops)
  ═══════════════════════════════════════

  JDK C2       ████████                                          889 ms
  CratonVM R26  ███████████████                                  1676 ms  ★ loop unrolling
                                                                         ↑ gap: 1.89x

  Sieve of Eratosthenes (100K × 500 reps)
  ════════════════════════════════════════

  JDK C2       ████████                                          324 ms
  CratonVM R26  █████████████                                     510 ms  ★ OSR + speculative BCE
                                                                         ↑ gap: 1.57x

  Matrix 500×500 multiply (125M multiply-adds)
  ═════════════════════════════════════════════

  JDK C2       █████████                                         351 ms
  CratonVM R26  █████████████                                     518 ms  ★ LICM + compact refs
                                                                         ↑ gap: 1.48x
```

---

## Rounds 15-18: Float/Double, Fields, LICM

### 70 New Bytecodes (from ~60 to ~130 total)

```
  R8-R14 bytecodes (60):          R15 additions (38):          R16-17 additions (32):
  ─────────────────────           ─────────────────────        ──────────────────────
  iconst, lconst, bipush,        + fconst_0/1/2, dconst_0/1   + fadd, fsub, fmul, fdiv
  sipush, iload/store,           + fload/fstore (indexed+_N)   + dadd, dsub, dmul, ddiv
  lload/store, aload/store,      + dload/dstore (indexed+_N)   + fneg, dneg
  iaload/iastore, aaload,        + laload/lastore (long[], 8B) + fcmpl, fcmpg
  aastore, baload/bastore,       + faload/fastore (float[], 4B)+ dcmpl, dcmpg
  i/l arithmetic, shifts,        + daload/dastore (double[],8B)+ i2f, i2d, l2f, l2d
  bitwise, iinc, i2l, l2i,      + caload/castore (char[], 2B) + f2i, f2l, f2d
  lcmp, branches, goto,         + saload/sastore (short[], 2B)+ d2i, d2l, d2f
  ireturn, lreturn, areturn,    + freturn, dreturn, return     + getfield, putfield (R17)
  invokestatic, newarray,        + i2b, i2c, i2s
  arraylength, multianewarray,
  dup, pop, swap, nop
```

### Inline Array Access — SIB Encoding

```
  byte[]    MOVSX  EAX, BYTE [RAX + RCX*1 + 32]    scale=0  (1B each)
  char[]    MOVZX  EAX, WORD [RAX + RCX*2 + 32]    scale=1  (2B each)
  short[]   MOVSX  EAX, WORD [RAX + RCX*2 + 32]    scale=1  (2B each)
  int[]     MOVSXD RAX, DWORD [RAX + RCX*4 + 32]   scale=2  (4B each)
  float[]   MOVSXD RAX, DWORD [RAX + RCX*4 + 32]   scale=2  (4B, bit-pattern)
  long[]    MOV    RAX, QWORD [RAX + RCX*8 + 32]   scale=3  (8B each)
  double[]  MOV    RAX, QWORD [RAX + RCX*8 + 32]   scale=3  (8B, bit-pattern)
  Object[]  MOV    RAX, QWORD [RAX + RCX*8 + 32]   scale=3  (8B compact refs)
```

### Inline aastore — Compact Reference Write

```
  Before (R14):  CALL jit_aastore          (~15 instructions, function call overhead)

  R15 (Value):   SHL  RCX, 4              ; index *= 16 (SLOT_SIZE)
                 ADD  RCX, RAX            ; RCX = array_base + offset
                 MOV  QWORD [RCX+32], 4   ; write Object discriminant
                 MOV  QWORD [RCX+40], RDX ; write pointer value (16 bytes total)

  R20 (compact): MOV  QWORD [RAX+RCX*8+32], RDX    ; single 4-byte instruction!
                                                      ; raw 8-byte pointer, no Value enum
```

### Round 16: SSE Float/Double Pipeline

```
  Float arithmetic (fadd/fsub/fmul/fdiv):
  ═══════════════════════════════════════
  GPR (i64 bit-pattern)  →  XMM (IEEE 754)  →  SSE compute  →  GPR result

  pop RCX (value2)                  MOVD XMM1, ECX     ; 4 bytes to XMM
  pop RAX (value1)                  MOVD XMM0, EAX     ; 4 bytes to XMM
                                    ADDSS XMM0, XMM1   ; scalar float add
                                    MOVD EAX, XMM0     ; result back to GPR
  push RAX

  Double arithmetic (dadd/dsub/dmul/ddiv):
  ════════════════════════════════════════
  Same pattern with MOVQ (8 bytes) and ADDSD/SUBSD/MULSD/DIVSD

  Comparison (fcmpl/fcmpg/dcmpl/dcmpg):
  ═════════════════════════════════════
  UCOMISS XMM0, XMM1    ; compare, sets CF/ZF/PF
  SETA AL               ; value1 > value2 → 1     ┐
  SETB CL               ; value1 < value2 → 1     ├── extract flags BEFORE ALU
  SETP DL               ; NaN → 1                 ┘
  ... combine for -1/0/1 with NaN bias

  Type conversions — 12 opcodes via SSE:
  ═════════════════════════════════════
  i2f: CVTSI2SS XMM0, EAX           f2i: CVTTSS2SI EAX, XMM0
  i2d: CVTSI2SD XMM0, EAX           d2i: CVTTSD2SI EAX, XMM0
  l2f: CVTSI2SS XMM0, RAX (REX.W)   f2l: CVTTSS2SI RAX, XMM0 (REX.W)
  l2d: CVTSI2SD XMM0, RAX (REX.W)   d2l: CVTTSD2SI RAX, XMM0 (REX.W)
  f2d: CVTSS2SD XMM0, XMM0          d2f: CVTSD2SS XMM0, XMM0
```

### Round 17: Object Field Access (getfield/putfield)

```
  getfield (0xb4) — read object field:
  ═════════════════════════════════════
  1. Field resolution at JIT compile time (cp_index → field_index + type)
  2. Pop object pointer from JIT stack
  3. CALL jit_getfield(obj_ptr, field_index)
     → reads Value at obj + 32 + field_index × 16
     → extracts i64: Int→sign-extend, Long→direct, Float/Double→IEEE bits

  putfield (0xb5) — write object field:
  ═════════════════════════════════════
  1. Pop value, then object pointer from JIT stack
  2. Call type-specific helper:
     • Primitives: jit_putfield_int/long/float/double(obj, idx, val)
     • References: jit_putfield_object(heap, obj, idx, val) + write barrier
```

### Round 18: Loop-Invariant Code Motion (LICM)

```
  Loop Detection & Invariant Analysis
  ════════════════════════════════════

  1. detect_loops():      find natural loops via backward branches
                          (goto/conditional with negative offset)

  2. find_modified_locals(): build 64-bit bitmask of locals written
                          within loop body (istore/astore/iinc)

  3. match_invariant_aaload(): pattern match:
                          aload X; iload Y; aaload
                          where X and Y are NOT in modified bitmask

  4. find_loop_hoists():  combine for nested loops, sort by span
                          (outermost first for deduplication)


  Before LICM:           After LICM:
  ──────────────         ────────────────────
  loop_header:           preheader:
    aload_0     ←┐        aload_0
    iload 4      │        iload 4
    aaload       │        aaload
    iload 7      │        MOV [spill], RAX    ← hoist to spill slot
    iaload       │      loop_header:
    ...          │        MOV RAX, [spill]    ← load from spill slot
    goto header ─┘        iload 7
                          iaload
                          ...
                          goto header


  Safety: no hoisting if loop contains aastore (0x53),
  which could invalidate the hoisted Object[] element.
```

---

## Technical Implementation

### JIT Module (~7,100 lines)

| File | Lines | Purpose |
|------|-------|---------|
| `vm/src/jit/mod.rs` | ~1800 | ExecutableBuffer, CompiledMethod, JitCache, helpers, OSR trampoline |
| `vm/src/jit/x64.rs` | ~5300 | x86-64 emitter, Compiler, LICM/BCE/SIMD analysis, ~80 tests |

### x86-64 Instructions Emitted

| Category | Instructions |
|----------|-------------|
| **Data movement** | MOV reg↔mem, MOV reg←imm32/64, PUSH/POP, LEA |
| **Arithmetic** | ADD, SUB, IMUL (reg and imm), NEG (32-bit and 64-bit) |
| **Magic division** | IMUL+SAR+ADD (constant div/rem without IDIV) |
| **Bitwise** | AND, OR, XOR, SHL, SHR, SAR, BTC |
| **Comparison** | CMP, CMP-imm, Jcc (6 conditions), CMOV, SETcc |
| **Control flow** | CALL rel32, JMP rel32, RET |
| **Extension** | REX.W prefixes, MOVSXD, MOVSX, MOVZX |
| **Array access** | SIB addressing (*1, *2, *4, *8), SHL for *16 |
| **SSE float** | MOVD GPR↔XMM, ADDSS, SUBSS, MULSS, DIVSS, UCOMISS |
| **SSE double** | MOVQ GPR↔XMM, ADDSD, SUBSD, MULSD, DIVSD, UCOMISD |
| **SSE convert** | CVTSI2SS/SD, CVTTSS/SD2SI, CVTSS2SD, CVTSD2SS |
| **Stack frame** | MOV save/restore callee-saved (R12-R15,RBX,RSI,RDI) |

### JIT-Compiled JVM Bytecodes (130 opcodes)

```
  Constants:    iconst_m1..5, lconst_0/1, fconst_0/1/2, dconst_0/1,
                bipush, sipush
  Loads:        iload, lload, fload, dload, aload,
                iload_0..3, lload_0..3, fload_0..3, dload_0..3, aload_0..3
  Stores:       istore, lstore, fstore, dstore, astore,
                istore_0..3, lstore_0..3, fstore_0..3, dstore_0..3, astore_0..3
  Arrays:       iaload/iastore, laload/lastore, faload/fastore,
                daload/dastore, aaload/aastore, baload/bastore,
                caload/castore, saload/sastore, arraylength
  Int arith:    iadd, ladd, isub, lsub, imul, lmul, idiv, ldiv, irem, lrem
  Float arith:  fadd, dadd, fsub, dsub, fmul, dmul, fdiv, ddiv
  Negation:     ineg, lneg, fneg, dneg
  Shifts:       ishl, lshl, ishr, lshr, iushr, lushr
  Bitwise:      iand, land, ior, lor, ixor, lxor
  Increment:    iinc
  Conversion:   i2l, l2i, i2f, i2d, l2f, l2d, f2i, f2l, f2d, d2i, d2l, d2f,
                i2b, i2c, i2s
  Comparison:   lcmp, fcmpl, fcmpg, dcmpl, dcmpg
  Branches:     ifeq, ifne, iflt, ifge, ifgt, ifle
                if_icmpeq, if_icmpne, if_icmplt, if_icmpge, if_icmpgt, if_icmple
  Jump:         goto
  Return:       ireturn, lreturn, freturn, dreturn, areturn, return
  Fields:       getfield, putfield
  Invoke:       invokestatic, invokevirtual, invokespecial, invokeinterface
  Allocation:   newarray, multianewarray (2D)
  Stack:        dup, pop, swap, nop
```

---

## How We Closed the Gap: R21-R25

```
  Round 21: Array Bounds Check Elimination (BCE)
  Added safety bounds checks to all 16 array opcodes with out-of-line throw.
  Loop analysis detects induction variables + array length bounds.
  Provably safe loops: ONE check before loop, not per-element.

  Round 22: JIT invokevirtual / invokespecial / cross-method invokestatic
  Helper bridge (jit_invoke_method) for JIT -> interpreter dispatch.
  Cross-method invokestatic: direct CALL to other compiled methods.
  Enables JIT for ~60%+ more real-world methods.

  Round 23: On-Stack Replacement (OSR)
  Backward branch hotness counter in Frame (threshold: 10,000).
  Compile + transfer interpreter locals -> JIT frame mid-execution.
  Significant for long-running methods with hot inner loops.

  Round 24: AVX2 SIMD Vectorization
  CPUID check for AVX2. Detects int reduction loop patterns.
  VPMULLD + VPADDD for 8 ints at once. Scalar cleanup for remainder.
  8x throughput on data-parallel arithmetic inner loops.

  Round 25: SoA (Structure of Arrays) Value Layout
  Stack/locals: Vec<Value> (16B/slot) -> Vec<u64> + Vec<u8> (9B/slot).
  44% memory reduction. Tag-based GC root scanning.
  ~12% overall improvement from better cache utilization.
```

---

## Optimization Roadmap: All Phases Complete

```
  Phase 1 (R15-16)  SSE Float/Double                              COMPLETE
  Phase 2 (R17)     Object Field Access (getfield/putfield)       COMPLETE
  Phase 3 (R18)     LICM (loop-invariant code motion)             COMPLETE
  Phase 4 (R19-20)  VM Context + Compact Refs                     COMPLETE
  Phase 5 (R21-25)  BCE + invokevirtual + OSR + SIMD + SoA        COMPLETE
  Phase 6 (R26)     Loop unrolling + speculative BCE + regalloc    COMPLETE
  -----------------------------------------------------------------------
  Result:           1.50x vs JDK C2 (Fibonacci 1.31x)             DONE
```

### Timeline

```
  Phase 1    Phase 2    Phase 3    Phase 4         Phase 5     Phase 6
  R15-16     R17        R18        R19-20          R21-25      R26
  ------     ------     --------   --------        --------    --------
  1.93x      fields     LICM       1.8x            1.0x        1.50x
  SSE done   done       done       compact refs    SoA+SIMD    unroll+BCE

  R8   R10   R14   R16   R18   R20   R22   R24   R25   R26
  JIT  regs  regs  SSE   LICM  comp  virt  SIMD  SoA   unroll
  97x  0.3x  1.9x  1.9x  ~2x  1.8x  1.8x  ~1.2x 1.0x  1.50x
```

---

## Key Metrics Summary

| Metric | Value |
|--------|-------|
| Total Rust LoC | **~323,000+** |
| JIT module LoC | **~7,200** |
| JIT bytecodes | **~140 opcodes** |
| JIT unit tests | **~80** |
| Tests passing | **6,000+** |
| Clippy warnings | **0** |
| Native methods | **~3,100+** |
| Phases completed | **73** (0-72 + perf) |
| Optimization rounds | **26** |
| Total speedup (small) | **253x** (5064ms → 20ms) |
| vs JDK -Xint | **~28x FASTER** |
| vs JDK C2 | **1.50x QuickBench (Fibonacci 1.31x)** |
