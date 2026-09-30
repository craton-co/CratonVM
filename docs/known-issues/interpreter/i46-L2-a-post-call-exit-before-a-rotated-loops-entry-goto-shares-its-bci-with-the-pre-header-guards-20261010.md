# A post-call exit site before a rotated loop's entry `goto` shares its bci with the loop's pre-header guards

**Status: open — filed 2026-10-10 by interpreter round i1 wave 46, lane L2,
while admitting field reads as post-call successors
(`i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md`,
"Progress (wave 46)"). Read from the code, not run. Attribution only: no
wrong answer is known; the invariant it breaks is the single-pass tier's
"one non-rethrow reason per bci".**

## Evidence (a trace through named functions)

* `x64/op_invoke.rs::post_call_exit_successor_admitted` admits a `goto` /
  `goto_w` successor with an empty stack, on the argument that "a `goto`
  polls against its HEADER's map, so its own bci stays free".
  `emit_post_call_exit_site` refuses a successor that is a branch target,
  but a rotated loop's entry `goto` (`goto COND; BODY: ...; COND: if<cond>
  BODY`, the ecj / kotlinc shape) is not one.
* At that `goto`, the walk emits the rotated header's relocatable
  pre-header (`x64/bytecode_walk.rs`, `rotation_preheader_by_goto` →
  `emit_relocatable_preheader(rotated_header, goto_pc)`): the speculative-BCE
  entry guards (`emit_speculative_bce_guards`), the `aaload` and field hoist
  guards (`emit_field_array_hoist`), each a reason-2 stub at `at` = the
  `goto`'s pc, and their snapshot `emit_deopt_snapshot_at_guard(at)`, a
  `REEXECUTE` `BoundsCheck` point at that bci.
* The post-call site's `branch_mode_exit_target` ran first (at the call),
  filing its `REEXECUTE` `OsrExit` map at the same bci; its check
  `deopt_point_pcs.contains(&branch_pc)` cannot see a point filed after it.

So a call right before a rotated loop with pre-header guards leaves two
non-rethrow points at one bci. `osr_exit::deopt_reason_at_bci` answers
`Ambiguous` there, and a sink that has no stashed cause (every production
stash carries one today, so none is known) would charge a guard trap as
`UncommonTrap` with the wildcard speculation id instead of `BoundsCheck`.
The machine code is correct: the two stubs bake different boxes
(`osr_exit_box_ptr_by_bci` for reason 7, `deopt_box_ptr_by_bci[(bci, 2)]`
for reason 2), and both describe the frame at the `goto` with an empty
stack.

## What would fix it

Refuse the post-call site when its successor is a rotated loop's entry
`goto`: in `Compiler::emit_post_call_exit_site`, `||
self.rotation_preheader_by_goto.contains_key(&next)` beside the
branch-target test. The frame then leaves at the loop's first
exit-capable back edge, as for any call without a site. A jit unit test
needs an ecj-shaped rotated loop with a hoist or a speculative-BCE guard
after a call, asserting no site's map lands at the `goto` (the driver's rotation detection, `rotation_entries` in
`x64/driver.rs`, decides whether a test body qualifies; assert the
precondition in the test, as the post-call tests assert their header maps).
