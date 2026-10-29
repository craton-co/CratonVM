> **j27 wave 9 (lane W9VER, 2026-10-29): VERIFIED (Windows) under the opt-in switches, retirement candidate (the switches stay opt-in; the default still answers `OPAQUE_FRAME` for compiled frames).** Release LTO-off builds of the tree at `47479dc6e0` plus the other wave-9 lanes' WIP: `cratonvm-cli --features cratonvm-vm/experimental-debug` (`C:\craton\j27-ver-bin\cvm-ver-dbg.exe`) and the vm test exe with `--features experimental-debug`; HotSpot 25.0.3 as the reference. Tests: `j27_jvmtiframe_` 9, `j27_w8intr_` 2, `j27_w8rev_an_obsolete_compiled_body_is_read_without_the_table` 1 passed. Probe: `J27JvmtiCompiledFrameLocals.c` built with MSVC 2022 (`cl /LD`, `C:\craton\j27-ver-bin\native\j27jcfl.dll`), `javac -g`, `-agentpath:<dll>`. HotSpot prints the 18 facts of the probe header (measured; equal to the header). CratonVM with `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1 CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS=1` prints the same 18 lines in `--jdk-only` and `--compatible`, and `--nojit` matches at the defaults; `CRATONVM_DBG_JITC=1` shows the engagement (`debugger home frame J27JvmtiCompiledFrameLocals.mid:(IJDLjava/lang/String;Z)I bci=606: write door exit-after-call`, and for `main` at bci 65, while `main` runs OSR'd). At the defaults (both modes) 13 rows answer `err=32` (OPAQUE_FRAME), the documented opt-in state. The JDWP half (item 3) has no Windows probe here; the host's JDI run still decides it.

> **j27 wave 8 (lane W8INTR, 2026-10-28): item 3 (JDWP reads of an OSR'd frame) FIXED IN CODE under the same opt-in, PENDING HOST VERIFICATION.** `publish_debugger_frames_under` (the OWNING thread's publisher, `vm/src/runtime/interpreter.rs`) now calls the new `compiled_frame_locals::publish_osr_home_locals` right after `publish_frame_snapshot`, still before the thread can reach a safepoint (the window the compiled-home rows already use). It reads the published listing back as the C table reads another thread's (`native_env::published_rows`, now `pub(crate)`; one row per `FrameEntry`, frame index from the bottom), takes each OSR'd row (`runs_compiled`, with a frame index; new pure helper `osr_home_rows`), and reads its OSR body's homes with the same `own_home_values` / `HomeActivation::OsrBody` the C table uses (single exact activation by class, JDWP method id and bci, nothing inlined at the point, every local vouched; else nothing). The values are published as the frame's locals (`snapshot_local`, pinned); the row stays in `opaque_frames` with `false`, so `StackFrame.GetValues` / `ThisObject` answer (`commands::frame_has_no_readable_locals`) and every write / pop still refuses it (`OPAQUE_FRAME`, unchanged). The blocked-window publishers (not the owner) are untouched. Switch: the existing `CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS` (despite its name it now also gates this JDWP read), and the homes exist only under `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1`. Tests (`cargo test -j 5 -p cratonvm-vm --features experimental-debug --lib j27_w8intr_`): `compiled_frame_locals::j27_w8intr_jdwp_osr_home_rows_tests::{j27_w8intr_only_an_osr_frame_of_the_listing_is_a_jdwp_home_row, j27_w8intr_an_osr_row_without_a_frame_index_is_not_read}`. Host verify: a JDI run over `J27OsrStateRows`-style OSR loop suspended at a breakpoint in a callee of the loop (`ThreadReference.frame(1).getValues(...)` on the OSR'd caller) answers HotSpot's values with both switches `=1`, and the JDWP `OPAQUE_FRAME` (32) error as before with them unset; plus the JDI battery (`tools/jdi/run-jdi-conformance.sh`, both modes) unchanged with both switches on and off. No JDI probe is written (needs the JDI harness).

> **j27 wave 8 (lane W8REV, 2026-10-28): REVIEWED + item 5 FIXED IN CODE (under the same opt-in), PENDING HOST VERIFICATION.** Re-read both modules: the row match (class, JDWP method id, bci, no inline chain, a single candidate), the slot / tag / table order against `checked_local`, the GC windows (table read before the foreign-JNI entry, homes and handle inside it; parked reads as global refs) and the write refusal all hold. One gap closed, "What remains" item 5: an activation compiled before its class's last redefinition was checked against the CURRENT `LocalVariableTable`, which may describe other bytecode (a wrong `INVALID_SLOT` / `TYPE_MISMATCH`, or a mistyped answer). Now `own_home_values` returns `HomeValues { values, compile_cp_stamp }` (`vm/src/runtime/interpreter/compiled_frame_locals.rs`), `declared_for` returns `RowTable { declared, redefined_at }` (the class's `RedefinitionHistory::latest_redefinition`, read under the same class-manager lock before the entry), and `RowTable::for_activation` drops the table for a stale or unstamped body once the class was redefined, the interpreter path's rule for an obsolete frame (`declared_local`). Nuance: an EMCP redefinition (identical bytecode) also reads table-less here, where an interpreter frame keeps the table; the answer can only be more permissive (a value instead of a table error). No new switch (`CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS` still gates the whole path). Test: `cargo test -j 5 -p cratonvm-vm --features experimental-debug --lib j27_w8rev_an_obsolete_compiled_body_is_read_without_the_table` (fails on the old code: there was no stamp to compare). Host verify: the JVMTIFRAME recipe below, plus one redefinition arm when an agent probe exists (none is written: it needs a C agent with `RedefineClasses` around a compiled frame; the L6 redefinition probes are the nearest).

> **j27 wave 5 (lane JVMTIFRAME, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** In code behind the new switch `CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS` (default OFF): the C JVMTI table's `GetLocalInt` / `Long` / `Float` / `Double` / `Object` and `GetLocalInstance` now answer for a compiled activation and for an OSR'd interpreter frame, on the current thread and on a suspended thread parked for the debugger. The values come from the activation's home slots, the reader JDWP's `StackFrame.GetValues` has used since interpreter round i6 wave 120. It needs `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1` too, because only a compile made under that switch records its homes. `SetLocal*`, `PopFrame`, `ForceEarlyReturn*`, `NotifyFramePop` and every read of a thread blocked in a native stay `OPAQUE_FRAME`. The frame listing (`GetFrameCount`, `GetStackTrace`, `GetFrameLocation`) already listed compiled and inlined frames in HotSpot's order, so nothing changed there. Probe: `tools/probes/jit/j27/J27JvmtiCompiledFrameLocals.{java,c}`. Verify: see "Verification recipe".

# The C JVMTI table answers OPAQUE_FRAME for a compiled frame's locals

Status: OPEN (PARTLY fixed under a default-OFF switch)
Area: `vm/src/jvmti/native_env.rs` (`listed_rows`, `read_local`, `get_local_instance`, `local_op_on_parked`, `declared_local`), `vm/src/jvmti/native_env/compiled_locals.rs` (new), `vm/src/runtime/interpreter/compiled_frame_locals.rs` (new), `vm/src/runtime/interpreter.rs` (`read_compiled_homes`, `compiled_home_value`, read only)
Found by: lane OSRHUNT (`j27-osrhunt-osr-state-review-FIXED-20261029.md`, row "debugger reads of an OSR'd frame")

## Evidence

HotSpot 25 serves every JVMTI local-variable function on any Java frame, compiled or interpreted:

* Reads (`VM_GetOrSetLocal`) take the value from the compiled frame's `ScopeDesc` (`compiledVFrame::locals`).
* Writes deoptimize the frame first, through `JvmtiDeferredUpdates` and the frame's deopt on return.
* With `can_access_local_variables` acquired at OnLoad, C2 keeps every local alive (`ciEnv::should_retain_local_variables`), so a read never meets a dead value.

CratonVM before this change:

1. **The listing was not the problem.** `frames_of` / `GetFrameCount` / `GetStackTrace` / `GetFrameLocation` use the shared listing (`listing_rows_of_current_thread` → `build_frame_listing`, `CRATONVM_JVMTI_C_LISTING_SHARED`, default ON). It lists:
   * compiled activations with their inlined callees, innermost first, between the interpreter frames;
   * an OSR'd interpreter frame at the compiled half's bci when the dedupe's authoritative arm knows it.

   OSRHUNT read this as "the frame listing hides compiled frames". It does not: compiled rows are listed, they just carry no interpreter frame.
2. **The locals were the problem.** `listed_rows` gives such a row `frame: None`:
   * a compiled activation (`ListingRow::Opaque`) has no `Frame`;
   * an OSR'd frame (`runs_compiled`) is filtered out on purpose, because its `Frame` holds the values of the compiled entry, not the current ones.

   So `read_local` / `get_local_instance` / `local_op_on_parked` answered `OPAQUE_FRAME` (32) for both.
3. JDWP already read compiled activations' homes for a parked thread (`capture_blocked_compiled_view_with_homes`, `ListingRow::CompiledHome`). The C table did not, because its listing captured the view without homes. Neither side read an OSR'd frame's homes.

## Design (HotSpot-faithful, smallest correct piece first)

Two options were weighed:

* **(a) Deoptimize at the request**, then serve from the interpreter frame.
  * CratonVM has no deopt of a frame that is in the middle of a stack. Its deopt exits rebuild a frame only when the compiled code reaches a trap, a back-edge exit or a post-call exit.
  * The one door a debugger write can use is the forced post-call exit of i43-L1 stage 3b (`jit_bridge::force_compiled_home_exit`, JDWP only, under `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`). It changes the frame only when the call returns, so it serves writes, not reads.
  * Using it for a read would withdraw the body (make it not entrant) on every GetLocal. HotSpot does not do that for a read.
* **(b) Read the compiled frame's recorded state.** HotSpot does this for reads, and this VM already has its equivalent:
  * Under `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`, a single-pass compile made while a debugger observes locals stores every local to its home slot `[rbp - (idx + 1) * 8]` at every safepoint.
  * It also records each local's kind per safepoint bci (`CompiledMethod::debugger_home_locals`).
  * `interpreter::read_compiled_homes` reads them on the owning thread. It accepts a reference word only where the activation's active oop map names it, so it is a precise root a moving collection rewrote.
  * A C agent that acquires `can_access_local_variables` at OnLoad arms `debugger_observes_locals` (`DebuggerGates::c_agent_observes_locals`, i6 wave 132), so its compiles record homes too.

**Chosen: (b) for reads, built now. (a) stays the path for writes** (the i43-L1 stage-3 door, not yet reachable from the C table).

As built:

* `ListedRow::home` (new). For a row of the shared listing asked with frames, it names the activation a home read may serve:
  * `HomeActivation::Compiled { chain }`: a compiled row, by its entry-chain index;
  * `HomeActivation::OsrBody { frame }`: an OSR'd frame.

  It also carries the row's class id, JDWP method id and location, and the activation found must match all three.
* `compiled_frame_locals::own_home_values` runs on the owning thread.
  1. It walks `active_compiled_frames()` and splits the result exactly as the listing does (`drop_osr_continuations_keeping`): kept activations are compiled rows, and the authoritative arm's dropped OSR continuations are the OSR'd frames.
  2. It takes the single activation that matches the row's chain index or frame index, class, method and bci, with nothing inlined at that point. Two matches (a recursion) answer nothing.
  3. It reads that activation's homes.
  4. Fail closed: the switch off, no record at that bci, any `Unknown` local, an unvouched reference, no frame pointer, an inline chain at the point, or the heuristic dedupe arm all leave `OPAQUE_FRAME`.
* `native_env::compiled_locals` checks each read in the interpreter frame's order (`checked_local`, measured against HotSpot 25.0.3 in L1W44):
  1. `INVALID_SLOT` outside the locals;
  2. `TYPE_MISMATCH` when a reference and a primitive meet;
  3. then, from the `LocalVariableTable` at the row's location: `INVALID_SLOT` for a slot no entry covers, `TYPE_MISMATCH` for another declared kind.

  A never-written slot reads as `int` 0. `GetLocalInstance` reads slot 0 without the table, as the interpreter path does, after the static-method `INVALID_SLOT`. Writes answer `OPAQUE_FRAME`. `declared_local`'s table loop is now `declared_in_code`, shared with this path.
* GC safety:
  * On the current thread, the table is read before the foreign-JNI entry, because it locks the class manager. The homes are then read, and the local handle made, inside the entry, so no collection runs between them.
  * On a parked thread, the read runs on that thread between two polls, and an object comes back as a global ref, as every parked read does.
* The blocked-window path (`local_op_through_window`) is unchanged and still answers `OPAQUE_FRAME`. Its compiled view was captured when the window opened, and references read later are not valid (i18-L3 stage 4's open problem).

Why opt-in: it is unmeasured on a binary. It also rides on `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`, itself default OFF, whose positive control never reached its compiled path on the host (i43-L1 "Run", 2026-10-22). This probe is built to reach it: the caller compiles and the loop OSRs before the read, and the agent attaches at OnLoad.

## What remains (not built)

1. **Writes** (`SetLocal*`) to a compiled or OSR'd frame. They would go through the i43-L1 stage-3b door:
   * `force_compiled_home_exit` (verdict `Leaves`);
   * a pending write applied in `deopt_resume::build_deopt_frame_or_refusal`.

   It exists for JDWP's `SetValues` only (`debug::inspect::set_compiled_home_values`, keyed on the JDWP snapshot's frame ids). The C table needs its own record path keyed on `(frame_rbp, cm_ptr, call bci)` (`CompiledHomeBody`). An OSR'd frame needs a door after its call (the OSR body's post-call exit), which `debugger_write_door` does not yet classify for OSR artifacts.
2. **`PopFrame` / `ForceEarlyReturn*` / `NotifyFramePop`** on such frames. These are i43-L1 stages 3c and 3d. 3d-A needs a pop with no interpreter caller and a dispatch-loop exit from its entry frame. HotSpot's rules to keep:
   * `NOT_SUSPENDED` for another thread not suspended;
   * `OPAQUE_FRAME` for a native frame and for a pop with no Java caller.
3. **(FIXED IN CODE, j27 wave 8, lane W8INTR, for the owning thread's publisher; see the top block.)** **JDWP reads of an OSR'd frame.** `publish_frame_snapshot` still publishes such a frame opaque, with no locals. The same `own_home_values` with `HomeActivation::OsrBody` would serve it, but only where the publisher is the owning thread (`publish_debugger_frames_under`). The blocked-window publishers are not.
4. **The optimizing tier.** IR bodies record no homes, so their activations, IR OSR bodies included (`CRATONVM_JIT_OSR_OPTIMIZING`), stay opaque. HotSpot's twin is a per-call-site locals map built from the post-call `FrameState` (i43-L1 Triage option 1).
5. **(FIXED IN CODE, j27 wave 8, lane W8REV: read table-less, see the top block.)** **An obsolete compiled body** (class redefined after the compile) is read with the current method's table. Its homes are its own, but the `LocalVariableTable` may describe other bytecode. The interpreter path reads an obsolete frame as table-less. A compiled row carries no such bit yet (`ActiveCompiledFrame::compile_cp_stamp` against the class's redefinition count would give it).
6. **Default flip.** Both switches are ON candidates once the probe matches HotSpot in every arm and the JVMTI and JDI batteries are unchanged with them on.

## Verification recipe (host)

* Build: `cargo build -j 5 --release -p cratonvm --features experimental-debug`. The new code is compiled only with that feature.
* Tests:
  * `cargo test -j 5 -p cratonvm-vm --features experimental-debug --lib j27_jvmtiframe_` (the two new modules: `compiled_frame_locals::j27_jvmtiframe_home_row_tests`, `native_env::compiled_locals::j27_jvmtiframe_checked_home_value_tests`);
  * `cargo check -j 5 -p cratonvm-vm` without the feature, to make sure nothing leaked out of the gate.
* Probe (header of `J27JvmtiCompiledFrameLocals.java`), each arm diffed against the HotSpot run:
  * `--nojit`: HotSpot's lines exactly (every frame interpreted).
  * JIT arms (`--jdk-only`, `--compatible`) with the switches off: `err=32` for the `mid-*` local rows and the `main-*` rows. These are the failing rows the change targets. The other rows and `fact sum` are HotSpot's.
  * Fix arm: `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1 CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS=1`, both modes. HotSpot's lines exactly.

  With `CRATONVM_DBG_JITC=1`, confirm that `mid` compiled and look for the `debugger home frame ... write door` line, which proves the homes were read. If `main-*` stays `err=32`, the OSR body is an IR body (item 4), or the dedupe took its heuristic arm. Rerun with `CRATONVM_JIT_OSR_OPTIMIZING=0`.
* Batteries, unchanged with both switches on and off:
  * the C JVMTI listings, `tools/jvmti/run-jvmti-shims.sh` (the L1 `L1W44JvmtiLocalsAndStack` and `L1W46JvmtiOtherThreadLocalsAndMonitors` probes in particular);
  * the JDI battery, `tools/jdi/run-jdi-conformance.sh`, both modes.

  The recipes are in `docs/internal/reviews/interpreter-i6-run-results-20261224.md`, and the JNI-library gate memory has the MSVC / gcc notes.
