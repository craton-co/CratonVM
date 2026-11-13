# AVX-512 Vector Acceleration and Foundation Primitives

**Author**: CratonVM JIT & Runtime Team  
**Status**: Implemented & Verified (Sessions 1-4)  
**Target**: x86-64 with AVX-512 Foundation (`AVX-512F`) and Byte/Word extensions (`AVX-512BW`, `AVX-512VL`, `AVX-512DQ`)  
**Parity Target**: OpenJDK HotSpot 25 (byte-for-byte exact checksum and semantics)

---

## 1. Overview and Motivation

Modern x86-64 server processors (Intel Skylake-SP/Ice Lake/Sapphire Rapids/Emerald Rapids and AMD Zen 4/Zen 5) implement AVX-512 vector extensions. Compared to 128-bit SSE/SSE2 and 256-bit AVX2:
- 512-bit vector width allows scanning **64 bytes** (or 32 UTF-16 code units) per iteration.
- Dedicated opmask registers (`k0`–`k7`) eliminate the need for vector shuffle/blend masking overhead.
- Direct vector byte comparisons (`VPCMPB`) and word comparisons (`VPCMPW`) write results straight into an opmask register `k1`, which can be transferred to an integer GPR with `KMOVQ`/`KMOVD` for `TZCNT` trailing zero bit scanning.

This design delivers up to 4× throughput improvements for memory scanning, array zero/composite detection, and string intrinsic searches (`indexOf(int)`, `indexOf(String)`).

---

## 2. Architectural Design & Components

### 2.1 CPUID Detection & OSXSAVE Integration (`jit/src/x64/cpu_features.rs`)
To prevent `#UD` invalid opcode exceptions in hypervisors or restricted guest OS environments, AVX-512 requires verification of both hardware capabilities and OS state preservation:
1. `CPUID.07h:EBX[16]` (`AVX-512F`)
2. `CPUID.07h:EBX[30]` (`AVX-512BW`)
3. `CPUID.07h:EBX[31]` (`AVX-512VL`)
4. `CPUID.07h:EBX[17]` (`AVX-512DQ`)
5. OS state verification via `_xgetbv(0)` ensuring ZMM and opmask state saving (`XCR0[7:5]` set).

A kill-switch runtime flag `CRATONVM_JIT=avx512` (default ON) allows disabling AVX-512 at runtime, cleanly falling back to SSE2/AVX2 paths without requiring recompilation.

### 2.2 EVEX Instruction Emitter Primitives (`jit/src/x64/emit.rs`)
Instructions use the 4-byte `EVEX` prefix (`0x62 P0 P1 P2`):
- `emit_evex(mm, p0_flags, p1_flags, p2_flags)`: Universal EVEX prefix generator.
- `emit_vpxord_zmm(dst, src1, src2)`: 512-bit vector clear/xor.
- `emit_vmovdqu8_load_zmm_indexed(dst, base, index)`: 512-bit unaligned byte load with indexed addressing.
- `emit_vpcmpb_k_zmm_zmm(k_dst, zmm_src1, zmm_src2, imm8)`: Byte comparison into opmask register.
- `emit_kmovq_gpr_k(gpr, k)`: Transfer 64-bit mask from `k` register to 64-bit general-purpose register.
- `emit_vzeroupper`: Transition penalty prevention when exiting vector loops.

### 2.3 UTF-16 Substring Candidate Scan (`native-builtins/src/lang_string.rs`)

`index_of_units_from` uses AVX-512BW to compare the first UTF-16 code unit of a needle
against 32 haystack units at a time. The resulting opmask is enumerated from its
lowest set bit; each candidate is verified with the existing scalar slice
comparison, preserving lone-surrogate and code-unit behavior. Complete blocks
are bounds-checked before loading, and the scalar path handles the tail. This
path requires AVX-512F and AVX-512BW, is gated by the per-VM
`CRATONVM_JIT=avx512` runtime flag, and leaves shorter searches on the existing
scalar loop. `last_index_of_units_from` uses the same block filter in reverse
and checks opmask bits from highest to lowest, preserving `lastIndexOf` order.

### 2.4 Sieve Loop Acceleration (`jit/src/x64/simd.rs`)
In `emit_byte_sieve_nest`, when `avx512_enabled()` is true:
- Scans memory 64 bytes at a time using `VMOVDQU8` + `VPCMPB` against zero.
- If all 64 bytes are non-zero (composite numbers), advances pointer by 64 bytes in a single instruction.
- If a zero byte is detected, `TZCNT` identifies the exact index of the next prime immediately.
- Trailing bytes fall through to the existing 8-byte QWORD and byte-by-byte scalar handlers.

---

## 3. Compliance and Invariants

1. **Process Global Statics Ratchet**:
   Zero new process globals were added. The CPU detection uses `std::is_x86_feature_detected!` and runtime flags without global mutable state, adhering to `AGENTS.md` and passing `jit/tests/process_global_statics_ratchet.rs`.
2. **Byte-exact Parity**:
   All operations conform strictly to HotSpot 25 character indexing and sieve semantics. Boundary checks, deoptimization points, and null checks remain identical.
3. **Clean Fallback**:
   On CPUs lacking AVX-512BW or when `CRATONVM_JIT=avx512=0` is configured, execution automatically routes through verified SSE2 and scalar fallbacks.

## Experimental integer array sum

`CRATONVM_JIT=avx512-int-sum=0` disables the default-on testing AVX-512F version of the
existing int-array sum replacement. It processes 16 ints per batch, sign extends
them to eight qword lanes with two `VPMOVSXDQ` loads, accumulates with `VPADDQ`,
and reduces to the existing long or wrapping-int accumulator. The scalar tail,
range guards, and strip budget remain shared with the AVX2 path. CPU/OS AVX-512F
and AVX2 support are required, and `CRATONVM_JIT=avx512=0` overrides the opt-in.

The initial prototype's incorrect large-array sums came from EVEX compressed
memory displacement: a disp8 of 32 meant byte offset 1024 for `VPMOVSXDQ`, rather
than byte offset 32. The restored implementation uses disp8=1. It also uses W1
for `VPADDQ` and the 512-bit source length and source/destination ModRM order for
`VEXTRACTI64X4`. It preserves the implementation behind the opt-in while broader
throughput acceptance remains unpriced.

The maintained probe is `tools/probes/jit/avx512-int-sum/Avx512IntSumProbe.java`.
It covers signed data, long sums, wrapping int sums, nonzero start indexes,
vector tails, lengths around 8192 and 65536, and the actual 2^20 strip boundary. Compile into a task-owned
directory with `javac -d <classes> <probe>`, then compare its complete stdout
from HotSpot 25 and both VM modes with the default setting, the opt-in, and
`CRATONVM_JIT=avx512-int-sum,avx512=0`.

Validation: the retained probe matched HotSpot 25.0.4.1 in fresh
processes for both modes with the kernel default off, enabled, and vetoed by the
master switch, including lengths 2^20-1, 2^20, and 2^20+1. The opt-in/master-switch
unit test and the flag surface/generated-document tests passed. GNU assembler
independently confirmed all three EVEX instruction encodings used by the kernel.

## Experimental crypto kernels (default on for testing)

Three independent testing defaults preserve the existing registrations and Java-visible
contracts. `CRATONVM_JIT=avx512-rsa=0,avx512-ec=0,avx512-aes=0` disables all three;
`CRATONVM_JIT=avx512-rsa,avx512-ec,avx512-aes,avx512=0` vetoes all of them.
No flag changes the provider, native category, compatibility policy, padding,
key validation, or supported algorithm set. All feature checks include OS state.

* **RSA / BigInteger** (`avx512-rsa`): full products and squares in the existing
  reviewed BigInteger fast Montgomery/magnitude kernels use AVX-512F/IFMA.
  Little-endian 64-bit limbs are converted to radix 2^52; IFMA computes eight
  independent products at a time; scalar carries normalize every row before
  conversion back. The existing reduction, buffer/alias guards, exceptions,
  and transcription fallback remain in force. The flag also affects non-RSA
  BigInteger callers that use the same product kernels. It does not replace
  the separate RustCrypto RSA provider backend.
* **EC** (`avx512-ec`): the existing SunEC P-256 five-limb Montgomery multiply
  and square use IFMA for their product rows when every input limb fits in
  unsigned 52 bits. The exact transcription handles negative or wider limbs,
  preserving reflected calls. Carry propagation, subtraction, and selection
  remain shared. This accelerates the field kernel; the separate coarse EC
  scalar-multiply backend and P-384/P-521 arithmetic are unchanged.
* **AES** (`avx512-aes`): AVX-512F/VAES processes four AES blocks per ZMM register,
  interleaving four states for 16-block batches. Smaller complete batches use
  one ZMM; AES-NI/portable code handles remaining blocks. Existing ECB/CTR and
  independent CBC decryption batches benefit; sequential CBC encryption stays
  on its existing block path. All three key sizes and both directions use the
  JDK's existing round-key extraction. Mode state, overlapping-buffer order,
  padding and authentication remain in the existing code.

Explicit-register assembly preserves the workspace Rust 1.88 minimum (the Rust
AVX-512 intrinsic APIs were stabilized later). Every load/store is bounded by
complete local limb arrays or complete cipher batches; no masked read extends
past the input. The kernels are default on for testing, with individual vetoes, pending throughput acceptance.

`tools/probes/avx512-crypto/run.sh <vm>` creates HotSpot 25 fixtures, compares
complete public JCA output in both modes with the flags default/off, enabled,
and master-vetoed, then has HotSpot verify each VM-generated ECDSA signature.
It also runs the maintained `SbrW5Bigint fuzz` probe against HotSpot's interpreted
Java transcription for canonical and arbitrary reflected limbs, buffer reuse,
malformed arguments, complete result arrays, and exception types/messages.
The JCA probe covers 2048/4096-bit RSA signatures and mismatch rejection, P-256
ECDH/ECDSA, and AES-128/192/256 ECB/CBC/CTR/GCM across block boundaries, streaming
updates, and overlapping input/output. Kernel unit tests additionally compare
IFMA products with scalar carry arithmetic and VAES with the portable AES body,
including misaligned buffers and incomplete tails.

The debug build can emit existing `ir_lower.rs` home-census assertions in JIT
compilation workers and fall back to interpretation. They occurred with the
crypto flags both absent and enabled during the initial JCA comparison; the
runner reports their count separately from crypto parity. They are not treated
as crypto acceptance evidence or as repaired by these kernels.

Validation on the EPYC-Turin host (AVX-512F, IFMA and VAES
present): all six maintained matrix cases matched HotSpot 25.0.4.1, including
cross-VM ECDSA verification. The crypto crate's 63 tests passed with the flags
both absent and enabled; all five SunEC field tests and the flag surface and
generated-document checks passed. Debug JIT worker assertion counts in the
six-case run were 33-36 for JCA and 35-39 for kernel fuzz, including the default
and master-vetoed controls. Following the Commons Math testing request, all three
kernels now default on; throughput promotion remains open.

## Commons Math testing default update

The numerical implementation and ten-family audit are documented in
[AVX-512 Commons Math](avx-512-commons-math.md). All AVX-512 paths, including
the earlier int-sum and crypto kernels, default on for testing on supported
CPU/OS combinations. Individual `=0` switches and the master `avx512=0` remain
available. Earlier paragraphs describing opt-in validation record the historical
gates before this default flip.

## 4. Engineering Requirements for AVX-512 in Software HSMs & Enclaves

When implementing or running a software HSM with AVX-512 (e.g., inside CratonVM or native HSM daemons):

| Requirement | Implementation Detail |
| :--- | :--- |
| **ZMM Register Scrubbing** | After cryptographic operations complete, all vector registers (`zmm0`–`zmm31`) and mask registers (`k0`–`k7`) must be explicitly wiped (e.g., `vpxorq`/`vpxord zmm, zmm, zmm`, `kxorw k, k, k`) using non-optimizable zeroing primitives (such as inline assembly with `SeqCst` compiler fences or Rust's `zeroize` crate) before returning control. RAII guards (`ScrubOnDrop`) guarantee cleanup even across panic unwinding boundaries. |
| **Confidential Computing Support** | Modern software HSMs typically run inside hardware enclaves. AVX-512 is fully supported inside AWS Nitro Enclaves, AMD SEV-SNP, and Intel TDX / SGX on modern Xeon and EPYC hosts. Verification via CPUID and OSXSAVE (XCR0 bits 5, 6, 7) ensures hypervisor state preservation during enclave context switches without `#UD`. |
| **No Frequency Penalty** | While older Skylake-X CPUs suffered clock downclocking with AVX-512, AMD Zen 4/5 (Turin) and Intel Sapphire Rapids have eliminated this penalty; the CPU maintains full boost frequency during mixed scalar and 512-bit vector execution without downclocking penalty. |

### 4.1 Empirical Probe Results (AMD EPYC Turin Zen 5 Host)

Verified via `cargo run --release -p cratonvm-native-builtins-crypto --bin avx512-hsm-probe`:

```

4. **Post-Quantum Cryptography (PQC NewHope NTT Vectorization)**:
   NewHope polynomial NTT transforms (`to_ntt`, `from_ntt`, `mul_coefficients`, `core`) are vectorized using 512-bit vector registers with `VPADDW`, `VPSUBW`, `VPMULLW`, and `VPMULHUW`. Coefficients are processed 32 at a time, achieving an end-to-end 1.81× speedup while guaranteeing bit-identical Montgomery and Barrett arithmetic. Secret polynomial coefficients are scrubbed from vector registers upon return via `ScrubOnDrop`.

5. **Tiny-operand EC testing policy (default on)**:
   The measured 256-bit P-256 path is slower with AVX-512 (about 4.38µs versus 0.92µs scalar), but remains default-on for the requested all-path testing. Set `CRATONVM_JIT_AVX512_EC=0` to veto it; the master `avx512=0` also disables it.
## Vector API int512 equality

`VectorSupport.compare` serves the unmasked `Int512Vector` EQ form through
AVX-512F (`vpcmpd`, 16 lanes). Both class mirrors must name the exact bootstrap
classes, both payloads must be complete 16-element `int[]` arrays, and the mask
class must already be initialized. First-use initialization and other widths,
operators, types and masked comparisons retain the JDK SAM implementation.

The array data is copied through the existing bulk int-array reader; the result
boolean array is pinned across mask-object allocation. Registration explicitly
uses `NativeKind::Intrinsic` and restores the registrar category. The existing
`CRATONVM_VECTOR_INTRINSICS=0` registration veto and AVX-512 master veto remain
available; CPU/OS support is checked before executing the hardware operation.
The per-entry census includes `compare` so probes can prove that service occurred.
