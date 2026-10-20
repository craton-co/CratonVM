# gci i40/os: IR keep-set shared-colour kill -- open JIT residuals 1-3

Status: FIXED (JIT round 21 wave 1, lane keepset, pending orchestrator verification)
Area: JIT optimizing tier -- dead-home keep set, ref-slot colouring, spliced back-edge mode exits
Found by: gci i40/os (residuals), resolved by JIT round 21 wave 1 lane keepset

> **STATUS (2026-10-16, gci orchestrator): OPEN (JIT, not GC).** Moved out of
> `docs/internal/gc/gci-i41rv-waves-i39-i40-review-residuals-FIXED-20261016.md` (its item 3) when that GC page
> retired. The kill itself was found sound by reading and its premises are pinned by
> `gci_i41rv_the_shared_colour_kill_premises_hold`; what stays open are i40/os's residuals 1 (reach union inside one
> splice), 2 (`def_pc` shared colours) and 3 (the spliced back-edge mode exit's header-snapshot range, a
> deopt-correctness question). Not built. Report: `docs/internal/gc-round-gci-20261011/i40-os-report.md`.

## JIT round 21 wave 1, lane keepset: verdicts and fix

### Residual 3: a real deopt-correctness defect (fixed, default ON)

The spliced back-edge mode exit (`Lowerer::spliced_back_edge_mode_exit_state`, reached through
`CRATONVM_JIT_IR_SPLICED_HEADER_POLL_EXITS` / `CRATONVM_JIT_IR_ENTRY_SPLICED_HEADER_POLL_EXITS`, both default ON)
reads the chain snapshot recorded at the loop header's pc. It reads it **at the latch**, with each header φ replaced by
its input on that edge. Every value comes from its home word through `frame_value_for`, which never checks whether the
word was recycled or zeroed. Neither liveness that guards those words counted that read:

- **Keep set** (`snapshot_colours_live_after_each_call`). At a javac loop header (`iload`) the only steps at the
  header pc are its φs. Since gci i47w/ks47 (`CRATONVM_JIT_IR_KEEP_SET_SPLICED_EXITLESS_STEPS`, default ON) a φ at a
  spliced pc reads no snapshot, but it still marks the pc as read. So the header snapshot also dropped out of the
  bytecode-reachability union, and the latch's poll step read nothing.
  - Result: a value named only by that snapshot was dead at the poll.
  - Example: a callee local the loop reads only through a computation LICM hoisted to the pre-header, as in `b` of
    `acc = acc*31 + (b == t ? 1 : 2)`.
  - `outer_dead_home_offsets` then zeroed its home in the poll's slow path (`CRATONVM_JIT_IR_POLL_SLOW_PATH_DEAD_HOME_CLEAR`).
    The slow path does this right before the exit reads the frame state, so the interpreter resumed the callee with
    `b == null`.
  - The same liveness also dropped the value at the loop's earlier GC points.
  - The ks47 soundness argument ("a chain snapshot is read only by a node that can exit at its raw pc") missed this
    one reader.
- **Colouring** (`chain_read_colour_ranges`). It counts nodes at the snapshot's pc as readers. A header φ is such a
  node, so a value defined before the loop stays live around the whole loop whenever the header has a φ. In that
  common case nothing changes. A φ-less spliced loop had no reader, so the word could go to another value.

Fix: the latch is made a reader of exactly what the exit reads (`spliced_latch_exit_reads`, `jit/src/ir_lower.rs`).

- The keep set reads it at the latch's poll step. Header φs are read through their edge inputs, never the φ home, so
  gcd d5/u's φ-entry kill keeps its effect.
- The compiling method's locals that bytecode liveness calls dead at the invoke are narrowed out of that read, as
  `sp_bits` narrows own-pc snapshots. Zeroing a dead local is harmless.
- The colouring reads it at the latch's edge position, without narrowing.
- Switch `CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS`, correctness, **default ON**, `=0` restores both.

### Residual 1: real over-retention (fixed, default ON)

The bytecode-reachability union keeps every snapshot no step reads. Inside a splice it credits them to the enclosing
own invoke, so they apply at every site of that inlined call (start == D). i40/os named two producers. Both have
readers that are known exactly, and the keep set now places those reads itself:

- **A post-call chain snapshot** (`post_call_of`). Its only reader is its call's post-call exit, right after the
  call. It now gets a step after the call's own step, so the call's kill of its result comes first.
- **An unread guard-chain snapshot at a spliced `Merge` / `Region` pc.** The prune keeps these for the
  spliced-header exits. A chain snapshot is resolved only by:
  - a node at its raw pc, whose step marks the pc as read;
  - a node naming it;
  - the header exit above, which is now read at the latch.

  So such a snapshot has no reader left to model.

Under `CRATONVM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS` (retention, **default ON**, `=0` keeps them in the union; inert
when the residual-3 switch is `=0`), both kinds count as read and leave the union. With them out, nothing that stays
in `splice_named` comes from those producers. The combined-pc walk the report proposed is therefore not needed. It
is not built.

### Residual 2: no-op, not a defect (not built)

A colour with more than one occupant has no occupant that a flat snapshot names. `plan_slots` pins every value a
flat or post-call-only snapshot names, and `verify_slot_colouring` refuses a shared pinned colour. So the only
snapshots that name a shared colour are chain snapshots, which resolve to the enclosing invoke. Avoiding one own-pc
definition's bci in `refine_reach_by_def_kills` therefore cannot remove such a colour, and since residual 1 those
snapshots are mostly out of the union anyway. Building it would kill nothing.

### Switches

| Switch | Kind | Default | Reader |
|---|---|---|---|
| `CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS` | correctness | ON (`=0` off) | `ir_spliced_latch_exit_reads_enabled` |
| `CRATONVM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS` | retention | ON (`=0` off) | `ir_keep_set_modelled_chain_reads_enabled` |

Rows in `types/src/flag_groups.rs` (`// r21 keepset`) and `types/tests/flag-surface.txt`.

### How to verify

- **Unit tests.** Run `cargo test -j 5 -p cratonvm-jit --lib r21_keepset_` (4 tests, module
  `dead_ref_slot_clear_tests`):
  - `r21_keepset_the_spliced_latch_exit_reads_the_header_state_by_edge_inputs`: reads by edge input, the exit's
    shape, outer-local narrowing, and the switch defaults;
  - `r21_keepset_a_spliced_latch_poll_keeps_what_its_mode_exit_reads`: the defect. The default arm keeps `k` and the
    φ's edge input at the poll; `=0` keeps nothing;
  - `r21_keepset_the_colouring_counts_the_latch_exit_and_is_unchanged_with_a_header_phi`: colours and ranges are
    byte-identical on/off when the header has a φ;
  - `r21_keepset_a_post_call_chain_snapshot_is_read_after_its_call`.

  Also run the existing `dead_ref_slot_clear_tests` and `gci_i5ir_` tests, whose keep-set signature gained an
  argument.
- **Probe** `tools/probes/jit/r21/R21KeepsetSplicedLoopLocal.java`. It needs the JDWP surface, so build with
  `--features experimental-debug`. Run:
  - `javac -g -d out ...`
  - `cratonvm --java-home $JDK --jdwp-port 5793 -Xmx512m -cp out R21KeepsetSplicedLoopLocal wait`
  - `java -cp out R21KeepsetSplicedLoopLocal debug 5793`

  Expected on HotSpot and on the fixed default: `top frame: sum`, `b null: false`, `b.v: 7`, `t null: false`,
  `t.v: 7`. The kill-switch control `CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS=0` should print `b null: true` and no
  `b.v` row when the compare is hoisted. If it prints the HotSpot rows, the build did not reach the shape. Check with
  `CRATONVM_DBG_IR_SLOTS=1`:
  - the default arm prints `[ir-slots] spliced latch exit reads: block .. colours [..]`;
  - the `=0` arm prints `[ir-slots] back-edge dead-home clears at block ..` naming `b`'s word.
- **Retention.** Re-run the gci soundness rows of `vi40-os.list` (S block: `GenR5W2OsrDeadSlotProbe`,
  `GenR5W3OsrHolderProbe`, the `oome-throwable-kept` KP rows) on both switches. They should stay SAME. The residual-3
  read adds liveness only for values the exit really reads, and residual 1 removes only reads that nothing performs.

### Review notes (not defects)

- The opt-in gci i5/ir poll step (`CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS`) still reads `named[header_pc]` as a whole,
  header φ homes included. That over-keeps, under an opt-in switch only. If that switch is ever promoted, it could
  reuse the exact latch read.
- The flat back-edge mode exit was checked and is sound: a flat header snapshot pins every value it names, and its φ
  steps at an own pc read it.
- For a φ-less flat header, the snapshot is unread, so the union reaches it from the latch's bci.

## Original record (i41/rv item 3) IR keep-set shared-colour kill (i40/os): sound by reading; premises pinned

Question (brief): can `keep_set_step_kill` kill a slot still read on another path, at a deopt or an OSR exit? By
reading, no. The kill removes colour c at a `Ref` call / allocation / load D only on paths that pass D; liveness at a
site S still counts every reader reached from S without passing D (other paths, back edges, deopt / poll / exit reads,
which are all step reads). On a path through D the home is rewritten at D (`value_home_droppable` admits `Int` / `Long`
only), and no other occupant can be read after D: its colouring range would cover D, and `LiveRange::overlaps` is
endpoint-inclusive. Exception paths: call-site and implicit exception edges are explicit control flow
(`Op::CallThrew` -> `If` -> handler block, `jit/src/ir.rs` ~18377), so a value the handler reads is live through D's
block in the liveness core and cannot share D's colour; the call's own frame state (its exceptional exit) is in D's own
reads, which the at-site set adds after the kill. A `Ref` call / `LoadStatic` never receives a recycled colour
(`fresh_only`), so at its own site the colour holds no older occupant. NEW unit test
`gci_i41rv_the_shared_colour_kill_premises_hold` pins the two premises next to the kill.
Still open from i40/os (unchanged, not built): its residuals 1 (reach union inside one splice), 2 (`def_pc` shared
colours) and 3 (the spliced back-edge mode exit's header-snapshot range, a deopt-correctness question).
