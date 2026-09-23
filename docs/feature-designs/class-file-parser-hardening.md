# Class-file parser hardening (`reader/`)

**Status:** Shipped (default on). Parsing is fully fallible and every
attacker-controlled length is bounded.

## The threat model

An attacker supplies the `.class` bytes. Every length, count, index and offset
in the file is attacker-controlled. The parser must reject non-conforming input
with a precise error, without panicking, without allocating attacker-chosen
amounts of memory, and without reading out of bounds.

## What it does today

- **Every entry point returns `Result`.** `read_class`, `read_class_arc` and
  `read_class_shared` in `reader/src/class_reader.rs` yield
  `Result<ClassFile, ClassReaderError>`; errors are typed in
  `reader/src/class_reader_error.rs`.
- **Every primitive read is bounds-checked and fallible.**
  `reader/src/buffer.rs` — `read_u8`/`u16`/`u32`/`i32`/`i64`/`f32`/`f64`,
  `read_bytes` and `skip` all return `Result`, with
  `ClassReaderError::UnexpectedEndOfData`.
- **Length-driven allocation is closed.** `reader/src/limits.rs` is a dedicated
  module whose `ensure_count_fits` / `bounded_capacity` bound every reservation
  by *bytes actually remaining*, not by a magic constant, so "65,535 constant
  pool entries in a 40-byte file" is rejected before the allocator is touched.
  Explicit caps: `MAX_CODE_LENGTH`, `MAX_ATTRIBUTE_DEPTH` (16),
  `MAX_ANNOTATION_DEPTH` (256), `MAX_SIGNATURE_DEPTH` (256),
  `MAX_SWITCH_ENTRIES`, `MAX_STACK_MAP_ENTRIES`, `PREALLOC_CAP` (1024),
  `MIN_CONSTANT_POOL_ENTRY_BYTES`.
- **No production panics.** The `unwrap()` / `panic!` occurrences in
  `class_reader.rs` and `attribute.rs` are all inside `#[cfg(test)]` assertion
  arms.
- Panic-only fuzz targets cover the reader; see
  [`fuzzing-state.md`](fuzzing-state.md) for what CI does and does not do with
  them.

## Mutation harness

`reader/tests/mutation_harness.rs`. Deterministic and exhaustive — no
`rand`, no seed, no wall-clock bound; identical results on every host and
every run. Not a coverage-guided fuzzer (see below).

One 218-byte valid seed class (constant pool exercising `Utf8`/`Class`
cross-references; one method with `Code`; a non-empty `exception_table` with
a real `catch_type`; nested `LineNumberTable` and `StackMapTable`; a
class-level `SourceFile`) is mutated in four systematic families:

| Family | Mutants | Invariant asserted |
|---|---:|---|
| Truncation at every length | 218 | must return `Err` |
| Single-byte substitution × {`00`,`01`,`7F`,`80`,`FF`} | 1 090 | must not panic |
| `u16` at every offset → {`0`, `1`, `u16::MAX`} | 651 | must not panic |
| `u32` at every offset → {`0`, `0x10000`, `0x80000000`, `u32::MAX`} | 860 | must not panic |
| **Total** | **2 819** | |

`mutation_coverage_totals_are_exact` pins these numbers so a change to the
seed is a visible edit rather than a silent change in coverage.

Two deliberate design points:

- **The substitution families do not assert `Err`.** Flipping a byte inside
  `max_stack` or a `Utf8` payload leaves a perfectly valid class; asserting
  `Err` there would assert a falsehood. The invariant that holds for *every*
  mutation is "terminates, without panicking, with a `Result`" — which is
  exactly what a memory-safety bug breaks. Truncation is different: the
  reader consumes the input exactly and rejects trailing bytes, so every
  strict prefix is unconditionally invalid and `Err` is the only correct
  answer.
- **Each mutant is driven past `read_class`.** `read_class` alone would only
  exercise the eager path; ~70 % of attribute bodies stay
  `LazyAttribute::Raw` and are never touched. The harness force-decodes every
  class, field and method attribute and then runs each `Code` body through
  `verified_code`, pulling the annotation/element-value recursion,
  `StackMapTable::parse`, `LineNumberTable`, the exception table and the
  instruction decoder into the blast radius. Mutants run under
  `catch_unwind` so a panic names the exact offset and substituted value.

## What remains

- **No coverage-guided fuzzing campaign has ever been run.** `fuzz/` contains
  18 `libfuzzer` targets (`fuzz_classfile`, `fuzz_constant_pool`,
  `fuzz_attribute_nesting`, `fuzz_stack_map`, `fuzz_instruction`,
  `fuzz_jimage`, `fuzz_verifier`, …) with seed corpora, but they need a
  nightly toolchain and `cargo fuzz`, they are not wired into CI, and there
  is no record of a campaign, no crash corpus, and no coverage report. The
  mutation harness above is exhaustive over its four families but explores a
  single seed's neighbourhood only — it cannot find bugs that need a
  structurally different class file to reach. **Running the existing targets
  is the highest-value remaining work in this lane**, and it is unstarted.
- **Instruction-boundary alignment is not checked by the reader.** JVMS
  §4.7.3 requires each exception-table PC to be the index of an *opcode*, not
  the middle of a multi-byte instruction. Establishing that needs the
  bytecode decoded, which `reader` defers to `quickened` / the bytecode
  verifier by design. The range checks added here are the part obtainable
  without decoding; the alignment half is still the verifier's job and was
  not audited by this lane.
- **`max_stack` / `max_locals` are read and stored but never checked** against
  the method descriptor's argument slots or against the actual stack depth.
  That is verifier territory (JVMS §4.9.2), not parser territory, but it is
  worth stating that nobody has confirmed the verifier does it.
- **`ConstantPool::validate` remains unwired** (above).
- **`jimage.rs` was swept but not fuzzed here.** Its header, section offsets
  and location records are bounds-checked (`jimage.rs:277,390,500-518,702-714`).
  One residual: `find_resource` compares `end as usize > resources.len()`
  after a `u64` `checked_add`, so on a hypothetical 32-bit target the `as
  usize` narrowing could wrap. Not reachable on any supported host; noted
  rather than fixed.
- **The `PREALLOC_CAP`-bounded reservations in `attribute.rs`** could be
  tightened to the remaining-bytes bound used elsewhere. Bounded, not
  exploitable, deliberately left.

## Files

- `reader/src/attribute.rs` — `validate_exception_range`, `validate_catch_type`,
  wired into `validate_attribute_shape` and `decode_code_body`.
- `reader/src/constant_pool.rs` — `Module`/`Package` `name_index` validation.
- `reader/tests/exception_table_ranges.rs` — 15 regression tests, each
  failing before the fix, with must-accept twins at every boundary.
- `reader/tests/mutation_harness.rs` — the 2 819-mutant sweep.
