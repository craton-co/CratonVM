# FIXED: every JIT analysis decoded bytecode with its own private copy

**Status: FIXED 2026-09-12** for the decoding layer. Found by the 2026-09-12 JIT
review as finding #68, "Two optimizing front ends with duplicated analyses and
eight CFG decoders".

The finding's second half — the optimizing passes themselves exist in both
tiers — is tracked in `optimizing-passes-still-exist-in-both-tiers-20260912.md`.

## What was duplicated

Before this fix, every pass that walked raw bytecode derived instruction
lengths, branch targets, switch tables, reachability and loop headers from its
own private code. That was 5 length decoders and 17 control-flow decoders.

### Instruction-length decoders

| File | Function |
|---|---|
| `x64/licm.rs` | `bytecode_len_at` (re-exported as `x64::bytecode_len_at`) |
| `regalloc.rs` | `bc_len` |
| `scev.rs` | `bytecode_len` |
| `loop_analysis.rs` | `inst_len_at` |
| `null_check_elim.rs` | `op_len` |

### Branch, switch and CFG decoders

| File | Function |
|---|---|
| `regalloc.rs` | `branch_target`, `switch_targets` |
| `x64/licm.rs` | `compute_branch_targets`, `branch_targets_at`, `opcode_falls_through`, `oop_dataflow_successors`, `compute_reachable_pcs(_with_roots)`, `instruction_start_map`, `MethodCfg` with `dom_intersect` |
| `x64/stack_kinds.rs` | `successors`, `branch_target`, `branch_target_wide`, `switch_targets` |
| `null_check_elim.rs` | `rel16`, `rel32`, `switch_targets` |
| `x64/bce.rs` | the offset decoding inside `collect_i16_branch_targets` and `branch_edges` |
| `x64/escape_analysis.rs` | `detect_loops`, and the edge scan in `find_bypassable_loop_headers` |
| `loop_analysis.rs` | `detect_loops`, `branch_targets`, `loop_has_other_exit` |
| `ir.rs` | `parse_switch`; `find_branch_targets` and `find_loop_headers` (test-only) |
| `lib.rs` | `decode_for_local_liveness`'s branch and switch arms |
| `vm/src/runtime/interpreter/jit_bridge.rs` | `inline_bytecode_length` / `inline_instr_length` |

### Where the copies had drifted

Each drift below was a live or latent miscompile:

- **`loop_analysis`:** its length table had no `jsr` (`0xa8`) or `ret` (`0xa9`)
  entry, stepped both one byte, and applied no `tableswitch` count cap.
- **`licm`:** its length decoder jumped to the end of the code on a malformed
  switch. `compute_branch_targets` stopped marking targets at the first
  truncated switch header, so every later target was missed.
- **`regalloc`:** `branch_target` omitted `jsr`'s target while it included
  `jsr_w`'s.
- **`stack_kinds`:** it read a `tableswitch` count with no cap.
- **`null_check_elim` and `licm`:** a truncated branch offset decoded as offset
  0, a self-loop.
- **x64 escape analysis:** a switch whose count overflowed was treated as empty
  rather than opaque.
- **Earlier drifts:** `ldc`/`ldc_w`/`ldc2_w` missing from one table
  (CM-FASTMATH), and a null-check pass that ignored switch and handler edges
  another decoder already knew about.

## The fix: `jit/src/bytecode_analysis.rs`

One module owns every one of those facts.

- **`insn_len` / `step`.** The instruction-length table covers `wide`, both
  switch paddings, the 5-byte invokes and the wide branches. `step` advances one
  byte over an undecodable instruction, so every walk terminates and stays in
  bounds.
- **`decode_at` / `decode_method`.** Strict decoding: the whole instruction
  must fit, and `wide` may modify only an opcode JVMS §6.5 allows.
- **Control transfer.**
  - `offset_branch_target`, `is_offset_branch`, `is_subroutine_op`,
    `falls_through`, `is_exit`.
  - `switch_table` is STRICT: the whole table fits, the caps hold, and every
    target is in range.
  - `switch_targets_lenient` is LENIENT: it skips what does not decode.
  - `explicit_targets` is strict and refuses `jsr`/`ret`.
  - `normal_successors` is strict; `lenient_successors` is lenient.
- **Whole-method maps.** `instruction_starts`, `branch_target_map`,
  `reachable_pcs` and `back_edges`.
- **`InsnCfg`.** An instruction-granularity CFG with optional exception edges,
  reverse post-order, Cooper/Harvey/Kennedy dominators, dominance loop headers
  (rotated loops included) and natural loop bodies.

The strict/lenient split is deliberate, and each function documents which
policy it follows. A transform must not guess, so it gets a refusal. A dataflow
analysis where extra edges are the safe direction, such as liveness or poison
propagation, gets the lenient answer.

Every caller in the table above now reads these functions, and every private
decoder is deleted. Each module keeps its own refusal policy at the call site:

- `bce` still refuses a method containing a switch, `jsr`, `ret` or a wide
  branch;
- escape analysis still considers only 16-bit back edges;
- `regalloc` still gives `ret` a fall-through edge for liveness.

For well-formed bytecode behaviour is unchanged. For malformed input every
change moves toward the conservative answer.

Bytecode liveness stays in `regalloc.rs`. It was already the only liveness over
bytecode, and its CFG is now built from these decoders.

## Regression coverage

`jit/src/bytecode_analysis.rs` tests:

- **Against the class reader.** Every fixed-width opcode and every `wide` form
  matches `cratonvm_reader`'s decoder byte for byte, and so does the
  instruction walk of a whole method.
- **Switches.** Padding is checked at eight alignments for both switch kinds.
  Malformed switches refuse strictly and step one byte.
- **Branches and fall-through.** Offset branches of both widths, including
  negative and out-of-range targets. Subroutines refuse, and exits do not fall
  through.
- **Reachability.** Dead code is excluded, extra roots are honoured, and `jsr`
  makes the method opaque.
- **`InsnCfg`.**
  - top-tested, rotated and nested loops, with their dominance headers and
    natural bodies;
  - handler edges, including a handler pc inside an instruction.
- **Back edges.** A switch counts once per distinct backward target.

`jit/tests/single_bytecode_decoder_ratchet.rs`:
- **The count.** Functions in `jit/src` that decode a switch by hand are pinned
  at 5: the two emitters, `jit_scan`, and two test fixtures that assemble switch
  bytecode.
- **The rule.** A test checks the rule against each padding spelling and against
  comments.

The migrated passes keep their own tests, unchanged. The two IR tests that
pinned the deleted test-only finders now pin `branch_target_map` and
`back_edges` over the same `invokedynamic` fixtures.
