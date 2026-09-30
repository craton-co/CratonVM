# JIT round 14 wave 2, lane deopt: proposals

Status: OPEN (proposal book; ideas, not work items, until the owner triages them)
Area: deopt stash identity and own-source resumes: `jit/src/deopt.rs`, `jit/src/x64/deopt_stubs.rs`,
`jit/src/ir_lower.rs` (IR deopt stubs), `vm/src/runtime/interpreter/deopt_resume.rs`,
`vm/src/jit/helpers.rs` (call-site service), OSR arms
Severity: proposals
Found by: round 14 wave 2 lane deopt

Ranked by expected value per cost. Wave 2 landed R13RP6-1: the x64 framed trap stashes its body's
own template (`deopt::peek_last_deopt_trap_source`), resolved through the compile id the stub
emitter writes into the artifact's `DeoptEpochGuard`.

## R14DP-1. The call-site service rebuilds its template from the stash's source on EVERY trap

**What.** `helpers::try_resume_trapped_callee` resolves the stash key through the call site's
loader, walks `find_method_recursive`, and allocates a fresh `CachedBytecodeMethod` per trap. Six of
its refusals ("stash class not found from the call site's loader", "declaring class mismatch",
...) exist only because a NAME is all it has, and each one leaves an orphaned frame for an outer
sink. When the stash carries the body's own template (x64 framed traps of published, stamped
bodies), use it whenever `source.declaring_class_id` is the class the key resolves to or the class
was never redefined: exact identity, no class-manager lock, no allocation.
**Benefit.** Fewer orphaned frames (each a whole-method re-run elsewhere) and a cheaper trap.
**Cost.** One branch at the top of the `cached` block (the own-source arm landed this wave is the
template).
**Risk.** Low: the template IS what the body was compiled from; the translation check is only
needed when the class was redefined.
**First step.** Under `CRATONVM_DBG_DEOPT=1`, count how often the service refuses by name while
`peek_last_deopt_trap_source()` is `Some` on the R13/R14 battery.

## Round 14 wave 3 (lane resume): R14DP-1 landed

Pending build. `deopt_resume::callsite_trap_current_template(shared, key_class, key_method,
key_desc)` answers the stashed template when it names the stash key's method, the method is not
`ACC_SYNCHRONIZED`, and its class was never redefined (then the template IS the current bytecode);
`helpers::try_resume_trapped_callee` takes it as `cached` before its name resolution (no
class-manager lock, no `find_method_recursive`, no allocation, and none of the loader-dependent
refusals). A redefined class keeps the existing paths (the superseded body's
`callsite_trap_own_source`, else the name path). Kill switch
`CRATONVM_DEOPT_CALLSITE_STASH_TEMPLATE` (default ON). Test
`deopt_resume::r14w3_resume_tests::the_call_site_service_takes_a_never_redefined_body_s_own_template`.
The census (first step) was not run: the lane cannot run the VM.

## R14DP-2. The optimizing tier names its trapping body too (R13R5-1, narrowed)

**What.** `ir_deopt_entry(point, rbp, regs)` has no guard argument, so an IR body's trap stashes no
source, and the call-site service still re-runs a superseded optimizing body of a redefined class
from entry. `ir_lower`'s deopt stub can pass a per-artifact `DeoptEpochGuard` exactly as the x64
stub does (a fourth argument; the Lowerer knows its `compile_id` since it assigns
`cm.compile_id` at ~38481), and `ir_deopt_entry` then calls the same `trap_source_of_guard`.
Reading the TLS compile-id mirror instead is NOT sound: after a callee returns, the mirror may
still name the callee, whose artifact another thread can be dropping.
**Benefit.** R13RP6-1 for the optimizing tier, which is where hot callees live.
**Cost.** One more imm64 per shared deopt tail in `ir_lower`, a signature change of
`ir_deopt_entry` (every stub calling it).
**Risk.** Medium (stub ABI in the IR lowerer; a missed call site passes garbage in the 4th
register -- `trap_source_of_guard` would then dereference it, so a null-safe transition needs all
emitters changed in one step).
**First step.** Grep every `ir_deopt_entry` call emission in `ir_lower.rs` and count the shapes.

## R14DP-3. Keep the stash source across take-inspect-restash

**What.** `restash_last_deopt_with_point` drops `StashEntry::source`, so a frame the call-site
service declines (identity mismatch, not materialisable) reaches the next sink without it. A
`take_last_deopt_with_source` / `restash_last_deopt_with_source` pair would keep it.
**Benefit.** The door that receives a declined callee frame as a FOREIGN stash could resume a
superseded body in its own code instead of taking the whole-body replay verdict.
**Cost.** Two small functions, one call-site change in the service.
**Risk.** Low.
**First step.** Census, under `CRATONVM_DBG_DEOPT=1`, how many restashes carry a source.

## Round 14 wave 3 (lane resume): R14DP-3 landed

Pending build. `cratonvm_jit::deopt::take_last_deopt_with_source` /
`restash_last_deopt_with_source` keep `StashEntry::source` across take-inspect-restash; the
call-site service (the only production restash) now takes and restashes with them, so a frame it
declines reaches the next sink still naming its body. Kill switch
`CRATONVM_DEOPT_RESTASH_KEEPS_SOURCE` (default ON; `0` restashes without the source, as before).
Test: the wave-2 test `deopt::..::the_guard_names_the_trapping_body_and_its_own_bytecode` gained the
round trip. No sink yet USES a source on a foreign stash at a door (the benefit sentence above);
that is a later step.

## R14DP-4. OSR arms for loops inside a `synchronized` block

**What.** `IrBuilder::build_opaque_osr_merge` declines a header with a held monitor, so a
handler-reached loop inside `synchronized (x) { ... }` has no optimizing OSR entry and runs the
single-pass OSR body (`r11w11-irexc-osr-entry-as-a-real-predecessor-CLOSED-20260929.md`, closed this
wave). The OSR door already transfers the lock record for ordinary entries; an opaque arm would
need the same monitor seeding (`Op::OsrMonitor` or the entry stub re-reading the interpreter's
lock record into the frame's monitor slot) before the jump.
**Benefit.** Retry / parse loops under a lock (legacy `Vector` / `Hashtable` users, JDBC drivers).
**Cost.** A builder arm plus the stub's monitor seeding.
**Risk.** Medium (monitor state at OSR entry; `holdsLock` and JMM).
**First step.** Count such headers with `CRATONVM_DBG_JITC=1` on the R11-R13 batteries.

## R14DP-5. Inner-scope templates for chains (CH3-1, the remaining half)

**What.** The four steps on `r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`
("Left", wave-2 section): keep each spliced body's template at the optimizing publish, take it in
`materialise_inner_scopes` for a redefined scope, restamp every pushed frame.
**Benefit.** Closes residual 1 of the chain flip verdict (a hot swap under the chain arm).
**Cost.** A table on `CompiledMethod`, the planner's `InlineSite` gaining `max_stack`, one
per-frame stamp in `InlinedChainFrame`.
**Risk.** Low-medium; agent-only reachability.
**First step.** Add `max_stack` to `InlineSite` and build the table behind the chain-snapshot
switch, unused.

## Round 14 wave 3 (lane chain): R14DP-5 landed

Pending build. Doors only (`real_frame_deopt_resume_or_throw_and_despeculate`), without the
`InlineSite` / `InlinedChainFrame` changes the "Cost" line expected:

- `jit/src/lib.rs`: `SpliceScopeSource` rows on `CompiledMethod::splice_scope_sources`, built by
  `splice_scope_sources` from the combined buffer (a spliced body is appended byte-for-byte, so
  `combined[base..base + code_len]` IS the callee's own code), one row per key (a disagreeing key is
  dropped), only when chain snapshots are recorded and `splice_sources_read_at_compile_stamp`
  (no redefinition began between the compile scope's stamp and the planner's last resolver read).
- `vm/src/runtime/interpreter/deopt_resume.rs`: `chain_inner_scope_own_sources` builds each
  redefined inner scope's template (current identity and modifiers, retained code and
  `max_locals`, no exception table, `max_stack` bounded by `2 * code_len`), refuses unless every
  such scope has one and `rebuilt_body_translates` from the body's `compile_cp_stamp`;
  `materialise_inner_scopes` takes it; `restamp_inner_own_source_frames` restamps those frames
  after the push (the per-frame stamp is positional, no struct change).
- Switch `CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE` (default ON, both halves). Tests
  `lib.rs::r14_chain_splice_despec_tests::splice_scope_sources_keep_each_body_once_and_drop_disagreeing_keys`,
  `deopt_resume::r14w2_deopt_callsite_own_source_tests::a_chain_whose_inner_class_was_redefined_takes_the_spliced_body`.
- Left: the tier-up sink (`interpreter.rs`), the call-site service by point and both OSR-exit chain
  transfers still refuse such a chain (details on the r13w5 page).
