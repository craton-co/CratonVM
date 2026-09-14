# Java comparison opcode lowering

`lcmp`, `fcmpl`, `fcmpg`, `dcmpl`, and `dcmpg` have bit-exact PTX lowering.
The emitter uses `setp` and `selp`, including explicit NaN handling so the
`l` and `g` variants retain Java's distinct `-1` and `+1` results.

The analyzer admits value-producing comparison opcodes. A compare that feeds
a branch inside an admitted counted-loop body is handled by
[lowering-branches.md](lowering-branches.md); comparison branches outside the
SIMT loop model remain CPU fallbacks.
