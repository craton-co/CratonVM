# Fix note: jit-aarch64-lentable

## Task (carry-over)
Add the `wide` prefix (`0xc4`) arm to the "inline bytecode-length switch" in
`jit/src/aarch64_backend.rs` (cited at ~line 2469), to bring it into lockstep
with the other two length tables (`x64.rs::bytecode_len_at` and
`regalloc.rs::bc_len`, which got the `0xc4` arm in Rounds 2-3): 4 bytes for the
wide load/store/ret family, 6 bytes for `wide iinc` (modified opcode at
`pc + 1 == 0x84`), with a `pc + 1` bounds check.

## Outcome: NO CODE CHANGE — target no longer exists (already resolved upstream)

There is **no inline bytecode-length switch in `aarch64_backend.rs`** to bring
into lockstep. The only structure in this file that ever fit that description —
the `detect_neon_patterns` / `NeonVectorizablePattern` bytecode scanner — was
**already removed in an earlier round of this same 2026-06-10 JIT cleanup pass**,
before this task ran. It is now an explanatory NOTE:

- `jit/src/aarch64_backend.rs:2383-2393` — comment block recording that the dead
  `detect_neon_patterns` scanner (dead: only called from its own unit tests; and
  buggy: hardcoded placeholder local indices) was removed rather than gated,
  since nothing non-test referenced it.

Verification that no length table remains in the file:
- Grepped `0xc4`, `\bwide\b`, and small-integer length arms
  (`=> 2,` / `=> 3,` / `=> 4,` / `=> 5,` / `=> 6,`) across the whole file: the
  only hits are two unrelated comments at lines 942/951 ("iload (wide index)" /
  "istore (wide index)"), which refer to the 1-byte local-index form of
  iload/istore — NOT the `wide` (`0xc4`) prefix. No length-returning `match`
  on an opcode byte exists.

## Why `wide` (0xc4) is correctly handled regardless

The file's single bytecode walk is the dispatch loop in
`Arm64Backend::compile_method_with_info` (`while pc < bytecode.len()` at
`aarch64_backend.rs:908`). It advances `pc` **inline within each opcode arm**
(e.g. `pc += 1` / `pc += 2` per handler) rather than via a centralized
length-table switch. The `match opcode` (starts `aarch64_backend.rs:920`) has no
arm for `0xc4`, so a `wide` prefix falls to the `_ =>` arm
(`aarch64_backend.rs:1651-1659`), which emits an `unsupported opcode` comment +
`Brk` and sets `success = false`, bailing the whole method out of aarch64
compilation. So aarch64 never compiles a `wide`-containing method, and there is
no PC-stepping length consumer that could desync. The lockstep concern that
motivated the x64.rs / regalloc.rs `0xc4` arms does not apply here because this
file has no separate length table.

## Reference (for confirmation the twins are correct)
`regalloc.rs::bc_len` `0xc4` arm (`jit/src/regalloc.rs:105-111`), which the task
asked to copy:

```rust
0xc4 => {
    if pc + 1 < code.len() && code[pc + 1] == 0x84 {
        6 // wide iinc
    } else {
        4 // wide <load/store/ret>
    }
}
```

This logic is already present and correct in both `regalloc.rs::bc_len` and the
`x64.rs::bytecode_len_at` twin. The aarch64 carry-over is moot because the table
it referenced was deleted.

## Files changed
None. (Owned file `jit/src/aarch64_backend.rs` left untouched — no length table
to edit; adding one would mean fabricating a non-existent switch, contrary to the
"copy the exact logic" / minimal-surgical-edit constraint.)

## Compile risk
None — no code change.
