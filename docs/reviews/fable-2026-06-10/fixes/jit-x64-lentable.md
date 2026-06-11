# Fix: jit-x64-lentable — `wide` (0xc4) missing from `bytecode_len_at`

## Finding
Report `jit.md` B3 (LOW): neither bytecode length table (`x64.rs::bytecode_len_at`,
`regalloc.rs::bc_len`) had an arm for the `wide` prefix opcode `0xc4`. Round 2's
`jit-lentables` added the wide arm to `regalloc.rs::bc_len`; the `x64.rs` twin
(`bytecode_len_at`, ~line 1601) was still missing it. Without the arm, `0xc4`
falls through to `_ => 1`, a 3-or-5-byte under-count that desyncs every
PC-stepping consumer (branch-target precompute, `dup2_top_cat2`, DCE,
loop/unroll analysis) after any `wide` instruction.

## Root cause
`bytecode_len_at` enumerates fixed-length opcodes but never decoded the `wide`
prefix. Per JVMS §6.5 a `wide` instruction is:
- `wide <load/store/ret> <indexbyte1> <indexbyte2>` → **4 bytes**
- `wide iinc <indexbyte1> <indexbyte2> <constbyte1> <constbyte2>` → **6 bytes**
The two forms are distinguished by the modified opcode byte at `pc + 1`: only
`iinc` (0x84) takes the 6-byte form.

This is currently **latent**: `jit_scan` rejects `0xc4` (reaches
`_ => return None`), so no method containing `wide` is ever compiled. The arm is
defense-in-depth and keeps the two hand-maintained length tables in lockstep, so
a future change that teaches `jit_scan` to accept `wide` does not silently
reopen the desync.

## Exact change
`jit/src/x64.rs`, in `bytecode_len_at`, added a `0xc4` arm immediately after the
`0xb9 => 5` (invokeinterface) arm, IDENTICAL in logic to the `bc_len` twin:

```rust
0xc4 => {
    if pc + 1 < code.len() && code[pc + 1] == 0x84 {
        6 // wide iinc
    } else {
        4 // wide <load/store/ret>
    }
}
```

The `pc + 1 < code.len()` bounds check guards the read of the modified-opcode
byte (a `wide` placed at the very end of `code` would otherwise index OOB);
falling to the 4-byte form on truncation matches `bc_len`. A doc comment mirrors
the one in `regalloc.rs` and cross-references the twin.

Added inline test `test_bytecode_len_wide` (next to `test_bytecode_len_invoke`)
asserting: `wide iload/istore/ret` = 4, `wide iinc` = 6, and the truncated
`[0xc4]` case = 4 (bounds check, no panic).

## Files touched
- `jit/src/x64.rs` — added `0xc4` arm to `bytecode_len_at`; added
  `test_bytecode_len_wide`.

## Tests added
- `test_bytecode_len_wide` — covers both wide forms plus the truncated-prefix
  bounds-check fallback.

## Follow-up & risk
- **Risk: minimal.** The arm is unreachable in practice (`jit_scan` still rejects
  `0xc4`), so runtime behavior is unchanged; the table is now merely correct for
  the day `wide` is accepted. Logic is byte-for-byte identical to the audited
  `bc_len` arm.
- Report Feature-Suggestion 2 (centralize the three length tables —
  `bytecode_len_at`, `bc_len`, and the `aarch64_backend.rs:2469` inline switch —
  into one `pub(crate) fn`) would eliminate this drift class entirely; the
  aarch64 copy still lacks the `wide` arm. Out of scope here (other-owned files);
  noted for a follow-up.
- `find_modified_locals` and `find_induction_variable` also do not decode the
  `wide` prefix (report B3); likewise latent and out of scope for this fix.
