# Class-file parser hardening (`reader/`)

> Status as of 2026-08-01, branch `feat/c2-review-remediation`.
> Scope: the P0 Security lane's parser half — "complete verifier hardening
> for adversarial class files; fuzz all binary parsers".

Threat model: an attacker supplies the `.class` bytes. Every length, count,
index and offset in the file is attacker-controlled. The parser must reject
non-conforming input with a precise error, without panicking, without
allocating attacker-chosen amounts of memory, and without reading out of
bounds.

## The report's premise was mostly already satisfied

The lane was scoped as if `reader/` were unhardened. It is not. Three prior
commits (`96f90473d` "Harden reader malformed input handling", `9c279636e`
"Polish reader format validation", `21c8ce8da` "Add reader parser robustness
regressions") plus several audit rounds had already closed most of the
listed attack classes. Verified by reading, not assumed:

| Attack class | State before this pass |
|---|---|
| Length-driven allocation | **Closed.** `reader/src/limits.rs` is a dedicated module whose `ensure_count_fits` / `bounded_capacity` bound every reservation by *bytes actually remaining*, not by a magic constant. `read_constant_pool` (`class_reader.rs:322`) rejects "65 535 entries in a 40-byte file" before touching the allocator. Interfaces/fields/methods/attributes all do the same (`class_reader.rs:160,185,203,636`). `StackMapTable::parse` reserves from `remaining` at `stack_map.rs:150`. |
| Arithmetic on attacker input | **Closed.** `checked_span`, `checked_end`, `wire_len_to_usize` in `limits.rs`; `checked_add` in `buffer.rs:103,118`, `instruction.rs:281,307`, `stack_map.rs:253`, `jimage.rs:500-516`. Boundary tests exist at `usize::MAX`, `u32::MAX` and `i32::MIN`-as-unsigned. |
| Panics as a vulnerability | **Closed** on the paths swept. The `unwrap()`s in `buffer.rs:40,52,64,80` are `try_into` on a slice whose length was just proven by `get()`, and are unreachable. No `expect()`/`panic!`/`unreachable!` outside `#[cfg(test)]` except `attribute.rs:668`, which is genuinely unreachable (the `Raw` arm was rewritten to `Decoded` three lines above). Raw slicing survives only where a preceding bounds check dominates (`jimage.rs:396,715`). |
| Modified UTF-8 (JVMS §4.4.7) | **Closed, and correct in both directions** — this is the item the report most expected to find broken. The parser does *not* call `String::from_utf8`. It uses `cesu8::from_java_cesu8`, with a hand-written fallback `decode_java_mutf8_to_utf16` (`class_reader.rs:256`) that emits raw UTF-16 units so lone surrogates round-trip (ANTLR `_serializedATN` is the real-world producer). It rejects a bare `0x00` (must be `C0 80`), rejects 4-byte UTF-8 as illegal, and rejects bad continuation bytes. `ConstantPool` carries a `wide_utf8` side table so `ldc` materialises the exact `char[]`. Tests at `class_reader.rs:747-801`. |
| Structural limits | **Mostly closed.** `code_length ∈ 1..=65535` at `attribute.rs:797,1802`. Attribute length must match parsed content *exactly* — `attribute.rs:944` (top level) and `:1633` (nested) both compare `consumed != length`; the class file itself must have zero trailing bytes (`class_reader.rs:220`). Switch entry counts capped and bounded by the remaining code (`instruction.rs:542,591,603`). |
| Constant-pool index validity | **Partly closed.** See below — this is where the real gap was. |

Two claims in the lane brief did not hold up:

- **"Bound `n` by the bytes remaining, not by a magic constant."** Already
  true for every table on the hot path. The ~25 `Vec::with_capacity(n.min(PREALLOC_CAP))`
  sites in `attribute.rs` (Module, annotations, Record, type-annotation
  paths) use the weaker *constant* bound of 1024 elements. That is still a
  hard cap — the amplification is bounded at ~16 KB per reservation and each
  loop iteration must consume input to continue — so it is not exploitable
  and was left alone rather than churned.
- **"Check that self-referential and cyclic constant-pool chains terminate."**
  There is nothing to check. Cycles are *structurally impossible*: every
  cross-reference in the pool is type-constrained to a strictly lower stratum
  (`Fieldref`/`Methodref` → `Class` → `Utf8`; `NameAndType` → `Utf8`;
  `MethodHandle` → ref → `Class` → `Utf8`). A `Class` whose `name_index`
  points at another `Class` fails the "must point to Utf8" test, so the chain
  cannot close. No cycle detector was added, and none is needed.

## What was actually broken, and fixed

### 1. `exception_table` entries were entirely unvalidated (the real find)

`decode_code_body` parsed each entry as four raw `u16`s and stored them
verbatim. `start_pc`, `end_pc`, `handler_pc` and `catch_type` were never
compared against `code_length` or against the constant pool — JVMS §4.7.3
constrains all four and HotSpot's `ClassFileParser::parse_exception_table`
enforces all four.

This matters because **`handler_pc` is a jump target**. The JIT reads it
straight out of this table — `jit/src/lib.rs:11670`, `let handler_pc =
entry.handler_pc as usize;`, fed into a code walk — and the interpreter uses
it to reposition `pc` while unwinding. A `handler_pc` of `0xFFFF` in a
four-byte method was an attacker-chosen out-of-range bytecode index handed to
code entitled to assume the parser had already rejected it.

Fixed in `reader/src/attribute.rs`:

- `validate_exception_range` — enforces `start_pc < end_pc <= code_length`
  and `handler_pc < code_length`, exactly HotSpot's rule set. Called from
  `validate_attribute_shape`'s `Code` arm, so violations fail at
  `read_class` time rather than at first downstream decode, **and** from
  `decode_code_body`, which is reachable independently (`decode_attribute`
  is `pub`, and nested `Code` bodies skip the eager walk).
- `validate_catch_type` — enforces "zero, or a `CONSTANT_Class`". Only
  callable from `decode_code_body`; the eager shape walk has no constant
  pool. This one check closes three of the four distinct constant-pool
  index hazards at once: index `0` (reserved sentinel), an out-of-range
  index, and an index landing on the **unusable second slot of a
  `CONSTANT_Long`/`Double`** — all three are `Tombstone` or `None`, so the
  positive "must be a `ClassReference`" test rejects them. The fourth
  hazard, wrong tag, is what the test checks directly.

Every rejection names the field and cites JVMS §4.7.3.

Compatibility checked by reading: all five in-tree class generators
(`classloading/src/proxy_gen.rs`, `native-builtins/src/cglib_enhancer.rs`,
`native-builtins/src/jboss_module_loader.rs`, `native-builtins/src/lang_class.rs`,
`classloading/tests/wp2_3_define_class_backend.rs`) emit
`exception_table_length = 0`. The one existing decoder test with a non-empty
table (`attribute.rs:3834`) uses `start_pc=0, end_pc=1, handler_pc=0,
catch_type=0` against `code_length=1`, which remains legal. Any real jar that
runs on HotSpot passes, because these are HotSpot's own rules.

### 2. `ConstantPool::validate` skipped `Module` / `Package`

`Module` and `Package` were the last reference-bearing tags still falling
into the `_ => {}` catch-all, so their `name_index` was the only
cross-reference in the pool that `validate` never inspected (JVMS §4.4.11,
§4.4.12 both require a `Utf8`). Now covered.

**`validate` is still not called on the parse path**, by deliberate prior
decision — see `reader/tests/wp_validate_wire_up.rs`, which pins that choice.
The reader's field/method/attribute parsers resolve indices to `Utf8` /
`ClassReference` directly and bail with `InvalidConstantPool` when they do
not find what they expect, so the reachable cross-references are checked at
use. `validate` covers the residue: entries no one dereferences. Wiring it in
is a policy change with an O(N)-per-class cost and was out of scope here.

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
