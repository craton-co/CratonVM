# Machine instructions and register allocation

The optimizing JIT lowers bytecode through a Sea-of-Nodes graph and scheduled
blocks to x86-64 machine code. The baseline compiler remains a separate backend;
see [backend boundaries](two-backends-decision.md).

`jit/src/x64/isel.rs` defines instruction patterns, constraints, tiles, and
`MInst`. `jit/src/ir_lower.rs::MirPlan` records block selections and the tile
owning each selected node. Absorbed producers do not have independent tiles.
Allocated integer instructions include register/immediate ALU operations,
address expressions, moves, and supported comparisons; unsupported shapes retain
the existing lowering path.

The allocation and emission contracts must agree on physical registers, frame
homes, operand widths, caller clobbers, parallel copies, and safepoint/deopt
locations. A coverage or location mismatch must decline the artifact rather
than emit a partial method. Encoder tests anchor selected instructions against
the existing x64 emitters, including extended registers and special addressing.

Selection controls are declared in `types/src/flags.rs` and read at their
owning sites in `jit/src/ir_lower.rs`. Consult [configuration](../CONFIG.md)
for available switches; the presence of a switch is not a claim that every
method uses the selected encoder.

See [instruction selection](../jit/instruction-selection.md),
[linear scan allocation](../jit/linear-scan-regalloc.md),
[deopt metadata](../jit/deopt-metadata.md), and [precise maps](precise-jit-maps-default.md).
