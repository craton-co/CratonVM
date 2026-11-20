# The unrolled OSR gap, `bci_at` totality, and the narrowed switch rule

Follow-up to [`loop-transforms.md`](loop-transforms.md) and
[`loop-transform-wiring.md`](loop-transform-wiring.md). Three changes to
`jit/src/x64/licm.rs`, none of which changes what the transform emits — they
change what it *answers* and what it *admits*.

## 1. `osr_entry_pc` refuses the unrolled back-edge gap

`LoopXform::osr_entry_pc(bci)` used to answer `Some(bci + steady * body_len)`
for every bci in `[header, back_edge_end)`. For `LoopXformKind::Unroll`
`steady` is `0`, so the back-edge instruction's own bytes answered `Some(bci)`.

That is not a conservative answer, it is a wrong one. The back-edge `goto` is
emitted in the **last** copy only; unroll's steady state is copy `0`, which
stops just before it. Output pc `back_edge` is `header + body_len` — the first
byte of copy 1, i.e. the header:

```
unroll.osr_entry_pc(19) == Some(19)     // before
unroll.bci_at(19)       == Some(2)      // the header, not the back edge
```

An OSR entry there resumes the interpreter's "about to execute the back edge"
frame at the top of a fresh body copy and runs one whole iteration too many —
the same class of bug as the LICM pre-header bypass.

`osr_entry_pc` now returns `None` for `bci in back_edge..back_edge_end` when
`kind == Unroll`. `None` already meant "do not enter compiled code here", so
consumers need no new case, and refusing costs nothing: the interpreter's very
next bci after the back edge is the header, which always has an entry.

Peel has no such gap — its steady-state copy is the last one, which carries
the back edge — and every other bci is answered exactly as before.

### Required edit outside `licm.rs`

`jit/src/x64.rs`, in
`loop_unroll_admission::osr_entry_lands_on_the_steady_state_copy_not_a_peeled_prefix`,
asserts the buggy behaviour on purpose so it stays visible. One assertion must
flip (it is the line immediately after the `// KNOWN GAP, asserted so it stays
visible rather than latent:` comment block, ~`x64.rs:39237`):

```rust
-            assert_eq!(unroll.osr_entry_pc(back_edge), Some(back_edge));
+            assert_eq!(unroll.osr_entry_pc(back_edge), None);
```

Everything around it stays true and should be kept:

* `assert_eq!(unroll.bci_at(back_edge), Some(header));` — unchanged; `bci_at`
  was never the buggy half, and this is now the *reason* for the `None`.
* both `peel.` assertions — peel has no gap.
* the `body_starts` loop above it — those bcis are `< back_edge`, so they are
  outside the gap and still answer with the steady-state copy.

The `// KNOWN GAP …` comment block (`x64.rs:~39230–39236`), which tells the
reader a consumer "must refuse OSR at `back_edge..back_edge_end` for an
unrolled method", should become a note that the *rewriter* now does that
itself.

## 2. `bci_at` totality is proved, not asserted in passing

Every deopt bci, oop-map `bytecode_pc` and exception-range record a compiled
method emits has to translate through `bci_at`, or the method publishes
transformed PCs as interpreter bcis. The property is now a test —
`bci_at_is_total_over_every_fixture_and_factor` — over every admissible
fixture, both kinds and every factor `1..=LOOP_XFORM_MAX_COPIES`:

1. `bci_of.len() == code.len()` (the map covers the output byte for byte);
2. every output pc that begins an instruction resolves, and resolves to a bci
   that begins an instruction in the **original**;
3. the instruction found there is the *same* instruction — same opcode, same
   length — and every interior byte maps to the matching interior byte, so a
   pc that is not an instruction start cannot resolve to a plausible-looking
   bci belonging to some other instruction;
4. the poll proof is re-checked on that output (every backward branch polled,
   and the back edge among them) for every fixture, not just `shape_a`;
5. `osr_entry_pc` is a partial inverse of `bci_at` wherever it answers.

Totality holds for every fixture and factor checked, including the two new
switch-bearing ones.

## 3. `SwitchInMethod` is now a per-switch question

The old rule refused the transform if a `tableswitch`/`lookupswitch` appeared
**anywhere** in the method. Two PC-relative facts motivate it, and neither is
re-encoded by this rewriter:

* switch operands are aligned to a 4-byte boundary measured from the start of
  the method's code (JVMS §6.5), so moving a switch can change its *length*;
* switch jump offsets are 4-byte fields, and the rewriter rewrites 2-byte
  branch offsets only (`0x99..=0xa7 | 0xc6 | 0xc7`).

Both are properties of a switch that **moves**, or one whose targets move
relative to it. So the refusal narrows to exactly that. A switch is admitted
when all three hold:

| Check | Why |
|---|---|
| `!(header <= pc < back_edge_end)` | a switch in the region is duplicated; the copies sit at different alignments and would each need their own relocated targets |
| `switch_pad(shift(pc)) == switch_pad(pc)` | the padding recomputed at the shifted PC, so the verbatim bytes are still a valid encoding and the length is unchanged |
| `shift(t) - shift(pc) == t - pc` for every target `t` | the un-rewritten 4-byte offset still names the same instruction |

`shift(p) = if p >= back_edge_end { p + delta } else { p }` — the same map the
exception ranges use, so the prefix and the header do not move and the suffix
moves by `delta = copies * body_len`.

Consequences:

* a switch **before** the loop branching only before the loop, or to the
  header, is admitted (nothing about it moves);
* a switch **after** the region is admitted iff `delta % 4 == 0` and all its
  targets are also after the region. The `shape_switch_after_loop` fixture has
  `body_len == 14`, so it is admitted at even `k` and refused at odd `k` —
  which is why the check recomputes padding instead of asking "does it move";
* a switch **inside** the region is still refused;
* a switch that branches **across** the region is still refused, because its
  4-byte offset would have to change and this rewriter never writes one.

### Soundness argument

The rewriter's whole layout — `out_len = code_len + delta`, the span table and
`bci_of` — assumes every instruction outside the region keeps its length and
every instruction after the region shifts by exactly `delta`. The three checks
are precisely the conditions under which a copied-verbatim switch satisfies
that, so an admitted switch is byte-identical *and* correctly encoded at its
new PC. Nothing else about a switch is PC-dependent: its operand values
(`default`, `low`/`high`, `npairs`, the match keys) are absolute data.

The output is still re-validated on the emitted bytes — `MethodCfg::build`
(exact walk, every branch target on an instruction boundary) and
`all_backward_edges_are_polled` — so a mistake in the rule fails closed rather
than publishing a mis-encoded switch. Poll preservation is unaffected:
`0xaa`/`0xab` are poll-bearing opcodes, and the transform neither creates nor
moves a backward switch edge relative to its own PC.

### Effect on the native unroller

`plan_loop_unroll` is currently used only as the admission oracle for the
native byte-copy unroller (`x64.rs::plan_native_unroll`). Narrowing this rule
therefore admits *native* unrolling of methods that contain a switch outside
the loop. That is not a widening past what the backend already did: the gate
this oracle replaced (`code[back_edge] == 0xa7` plus a body-size band) had no
switch test at all, and the switch question is a bytecode-encoding question
that does not arise for a machine-code duplicator. A switch inside the loop
body remains refused under both rules.

## Reconcile

Two documents still describe the pre-change behaviour:

* `docs/jit/loop-transform-wiring.md`, section "Known gap in
  `LoopXform::osr_entry_pc`" — the gap is fixed; the section should record the
  refusal and the `x64.rs` assertion flip instead of requesting them.
* `docs/jit/loop-transforms.md` — the preconditions table row "no
  `tableswitch`/`lookupswitch` in the method | `SwitchInMethod`" and the
  paragraph beginning "Two limitations are conservative rather than
  fundamental" now overstate the refusal; and its OSR paragraph should note
  that `osr_entry_pc` answers `None` across the unrolled back edge.
