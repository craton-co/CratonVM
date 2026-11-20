// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! ARM64 (AArch64) JIT backend compilation pipeline.
//!
//! Translates JVM bytecode into a sequence of `Arm64Instruction` pseudo-ops that
//! represent the compilation result.  A separate encoding step (using `aarch64.rs`)
//! can later lower these to raw machine code bytes.
//!
//! # SUPPORT STATUS — read before enabling this backend
//!
//! This is **not** a working second-tier JIT. It is an arithmetic-only
//! prototype. `ARCHITECTURE.md` calls it "partial coverage"; the audit
//! below (2026-07-26) is what that actually means. Do not ship an
//! `aarch64` build on the assumption that it is x64-equivalent.
//!
//! ## What it can compile
//!
//! Whole-method compilation only (no OSR). A method compiles **iff** every
//! one of its bytecodes is in the supported set below; a single unsupported
//! opcode sets `Arm64CompileResult::success = false`, which makes
//! `emit_machine_code` return `None` and `jit::try_compile` return `None`.
//! The VM then permanently bail-lists the method and interprets it. That
//! fallback is clean — an unsupported method is never mis-executed.
//!
//! Counts, for the 202 opcode values in `0x00..=0xc9`: **193** have a match
//! arm, **9** do not (table below). THIRTY-TWO of the 193 are shared-memory
//! opcodes the ordering gate lets through — `0x2e..=0x35`, `0x4f..=0x56` and
//! the whole of `0xb2..=0xc1`: the four field opcodes (round 9 waves 10 and
//! 15-17), the five `invoke*` forms (wave 22), the three allocations (wave
//! 18), `arraylength` (wave 12), `athrow` (wave 20), `checkcast`/`instanceof`
//! (wave 23) and the sixteen array element accesses (wave 13, with
//! `aaload`/`aastore` in wave 19). EVERY ONE of the thirty-two lowers only
//! when the caller wired what it needs — a resolved static of an initialized
//! class; an exact `NullPointerException` path; both that and an exact
//! `ArrayIndexOutOfBoundsException` path; a resolved instance field and a
//! wired `jit_putfield_*` / `jit_getfield` / `jit_putfield_object`; a resolved
//! class and a wired allocation helper; a resolved `JitInvokeInfo` and a wired
//! `jit_invoke_dispatch`; an interned target class name and a wired
//! `jit_checkcast` / `jit_instanceof_check`; and, for every one that can trap,
//! an EMPTY exception table — and refuses in its own arm otherwise. One of
//! the thirty-two refuses UNCONDITIONALLY: `invokedynamic` (`0xba`), whose
//! call site is not a class, so `JitInvokeInfo::invoke_kind` has no value for
//! it. None of the thirty-two is counted among the opcodes that lower below.
//!
//! Of the other 161, four lower CONDITIONALLY: `idiv`/`ldiv`/`irem`/`lrem`
//! (round 9 wave 11) compile only when the `ArithmeticException` throw path is
//! wired and exact (see the safety notes), and refuse otherwise. Three more
//! always refuse — `ldc`/`ldc_w` (`0x12`/`0x13`) and `ldc2_w` (`0x14`), for
//! want of a constant pool. So **154** opcodes lower unconditionally, 158 with
//! the throw path wired. For comparison, `x64.rs` has an arm for 193 of the same
//! 202 and lacks only `frem`, `drem`, `jsr`, `ret`, `wide`, `goto_w`, `jsr_w`.
//! (`pop2`, `dup2_x1` and `dup2_x2` were on that list until the commons-math
//! throughput fix and the `dup2_x2` fix added x64 arms for them — see
//! `bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816-FIXED.md`
//! and
//! `dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md`.
//! `x64::tests::scan_admitted_opcodes_are_lowered_or_declared` now fails if a
//! scan-admitted opcode ever loses its x64 arm again.)
//!
//! **The operand stack is ONE typed stack, and each entry records where its
//! value is (2026-09-12).** An [`Operand`] carries its kind (`I32`, `I64`,
//! `Ref`, `F32`, `F64`) and its location: a scratch register, or the frame word
//! reserved for its DEPTH. There used to be two stacks of bare registers -- one
//! for int/long/reference, one for float/double -- plus a `register -> spill
//! slot` map, and that design produced three separate miscompiles: the
//! allocator wrapped onto a live register and popping the new value reloaded
//! the old one (`a - (b+1+2+3+4)` was -4 for `(100, 5)`); the float allocator
//! round-robined V0-V7 with no liveness check at all; and the shuffles could
//! only refuse any form involving a float. Every shuffle now resolves its JVMS
//! form from the entries' categories (a `long` is one entry and two JVM
//! slots) and refuses only the forms the JVMS does not define. At every branch
//! each live entry is put in its depth slot, and each branch target rebuilds
//! the model from the shape recorded for it, so a value on the stack across a
//! merge (`c ? x : y`) arrives from both paths in the same place.
//!
//! What lowers: constants (`*const_*`, `bipush`, `sipush` — the `ldc` family
//! has arms but refuses, there being no constant pool here), local load/store for
//! int/long/float/double/reference, `iinc`, int/long/float/double arithmetic
//! and bitwise ops (integer division and remainder only with the throw path
//! wired, round 9 wave 11), the numeric conversions
//! (`i2l` … `i2s`), `fcmp*`/`dcmp*`/`lcmp`, all
//! `if*`/`if_icmp*`/`if_acmp*`/`goto`, `tableswitch`, `lookupswitch`, the
//! stack shuffles (`dup*`, `swap`, `pop*`), `nop`, and all `*return`.
//!
//! ...and, with what the caller wires, the first of the heap: a resolved
//! primitive `getstatic` (wave 10), `arraylength` (wave 12), every primitive
//! array element load and store (wave 13), a primitive `putfield` (wave 15),
//! `getfield` (wave 16), `putstatic` (wave 17) and `new`/`newarray`/
//! `anewarray` (wave 18). Those need, respectively, the static-field table, an
//! exact `NullPointerException` path, that plus an exact
//! `ArrayIndexOutOfBoundsException` path, the instance-field table plus that
//! same NPE path and a wired `jit_putfield_*` / `jit_getfield`, and a resolved
//! class with a wired allocation helper; each refuses in its own arm
//! otherwise. Since round 9 waves 22-23 the list continues: every `invoke*`
//! but `invokedynamic`, and `checkcast`/`instanceof`. So the admissible
//! population is no longer "leaf methods that touch nothing" — it is methods
//! with an empty exception table that lock nothing.
//!
//! ## What it CANNOT compile — the entire object model
//!
//! These opcodes have **no lowering at all** and bail the method. The rows
//! marked CONDITIONAL are here only for the sites nothing is wired for; they
//! ARE wired in a real aarch64 build (`lib.rs` supplies the site tables and
//! the exception-table word), so they lower there:
//!
//! | Area | Opcodes |
//! |------|---------|
//! | Array elements (CONDITIONAL) | `0x2e..=0x35`, `0x4f..=0x56` — lower with both array throw paths wired (wave 13); `0x32` `aaload` and `0x53` `aastore` additionally need `jit_aaload` / `jit_aastore_type_check` + `jit_aastore` and the context (wave 19) |
//! | Field access (CONDITIONAL) | `0xb2` `getstatic` and `0xb3` `putstatic` — lower a resolved static of an initialized class (waves 10, 17 and 19); `0xb5` `putfield` and `0xb4` `getfield` — lower a resolved instance field through `jit_putfield_*` / `jit_getfield` / `jit_putfield_object` (waves 15, 16 and 19) |
//! | Dispatch (CONDITIONAL) | `0xb6` `invokevirtual`, `0xb7` `invokespecial`, `0xb8` `invokestatic`, `0xb9` `invokeinterface` — lower through `jit_invoke_dispatch` with a resolved `JitInvokeInfo` and an empty exception table (wave 22) |
//! | Dispatch | `0xba` `invokedynamic` — has an arm, refuses unconditionally: a call site is not a class, and `JitInvokeInfo::invoke_kind` has no value for one |
//! | Allocation (CONDITIONAL) | `0xc5` `multianewarray` — lowers at ANY arity through `multianewarray_n` (ABI v14), given a resolved site (h23c). The dimension buffer is the operand area itself, so it costs one `SUB` |
//! | Allocation (CONDITIONAL) | `0xbb` `new`, `0xbc` `newarray`, `0xbd` `anewarray` — lower through `jit_new_object` / `jit_newarray` / `jit_anewarray_object`, the first helpers here that can SAFEPOINT (wave 18) |
//! | Array header (CONDITIONAL) | `0xbe` `arraylength` — lowers with an exact NPE path wired (wave 12) |
//! | Exceptions (CONDITIONAL) | `0xbf` `athrow` — lowers through `jit_throw_exception` with an empty exception table (wave 20) |
//! | Type checks (CONDITIONAL) | `0xc0` `checkcast` and `0xc1` `instanceof` — lower through `jit_checkcast` / `jit_instanceof_check` with an interned target class name and an empty exception table (wave 23) |
//! | Monitors (CONDITIONAL) | `0xc2` `monitorenter` and `0xc3` `monitorexit` — lower through `jit_monitor_enter` / `jit_monitor_exit`, the same shape `ir_lower` uses on x64; the lock-word CAS is the helper's (h23) |
//! | Misc | `0xc4` `wide`, `0xc8` `goto_w`, `0xa8`/`0xa9`/`0xc9` `jsr`/`ret`/`jsr_w` |
//!
//! **Round 9 wave 22 ended "no method containing a bytecode call compiles".**
//! [`Arm64Backend::emit_invoke`] set `self.failed` unconditionally from this
//! backend's first commit, and the reason -- no way here to turn a
//! constant-pool index into an entry point -- is STILL TRUE and no longer the
//! question: every site goes through `jit_invoke_dispatch`, which resolves its
//! own target at run time. See that function for the whole argument.
//!
//! In practice the admissible population is now: any method whose every
//! opcode has a lowering above. They may call, `throw`, cast, test, read and
//! write statics, instance fields and array elements of every type including
//! references, allocate an object or a one-dimensional array, and lock.
//!
//! **The empty-exception-table condition is gone (h23, 2026-09-22.)** It used
//! to do most of the work here -- it was what kept `synchronized` out, through
//! javac's generated `any -> monitorexit; athrow` handler -- and it was
//! described as the one condition a lowering could not buy its way out of,
//! because the interpreter's drain raised a pending exception with an unknown
//! pc. [`Arm64Backend::emit_stamp_throw_bci`] is how it was bought out of:
//! every trap edge stamps its own throw-site bci before leaving through the
//! epilogue, so the drain searches this method's own table from the right
//! site. `emit_monitor_op` is what then made `synchronized` a lowering rather
//! than a population question.
//!
//! ### The refusal is a RULE, not an accident
//!
//! All of the above used to be emergent: those opcodes bailed because nobody
//! had written a lowering, and the fact that this kept the backend *sound* on a
//! weakly-ordered machine — it emitted no `DMB`, no `LDAR`, no `STLR`
//! anywhere — was invisible to anyone about to write one.
//! [`opcode_touches_shared_memory`] is now consulted before the opcode
//! dispatch and refuses every one of them that is not in
//! [`opcode_has_ordered_lowering`]. So adding a `getfield` arm cannot, by
//! itself, produce a `volatile` read lowered to a plain `LDR`: the opcode has
//! to be put on that list first, and each entry is a reviewed claim about ONE
//! lowering rather than a switch. Round 9 wave 14 removed the blanket
//! `ARM64_CAN_ORDER_MEMORY` that used to sit in front of the list; see
//! [`ARM64_LOWERS_ACQUIRE_RELEASE`] for why one `bool` for both halves was
//! both too weak and unreachable.
//!
//! **Round 9 wave 10: the first lowering, `getstatic`.** It is the one opcode
//! [`opcode_has_ordered_lowering`] lets past the gate, because its lowering is
//! complete on its own: a primitive static of an already-initialized class,
//! resolved by the caller into an [`Arm64StaticField`], read with `LDAR` when
//! `volatile` and a plain `LDR` otherwise, with no receiver, no exception path
//! and no call. It lowers ONLY sites handed to
//! [`Arm64Backend::set_static_field_info`]; an unresolved site or a reference
//! static refuses in the `0xb2` arm. `jit/src/lib.rs` supplies that table
//! (wave 10b), so a real aarch64 build lowers it. The `MemLoad`/`MemStore`/
//! `DmbIsh` pseudo-ops carry width and ordering explicitly for the lowerings
//! that follow.
//!
//! **Round 9 wave 12: the NPE throw path, and `arraylength` on top of it.**
//! [`Arm64Backend::emit_npe_throw_stubs`] is the `NullPointerException` twin
//! of wave 11's `ArithmeticException` stub — one out-of-line stub per JEP-358
//! action code, calling `helpers.jit_npe_with_action` and leaving through the
//! epilogue with the `i64::MIN` deopt sentinel, which the interpreter's
//! JIT-return drain turns into the throw. It is subject to the same exactness
//! condition as wave 11's: the drain throws with an UNKNOWN pc, so the
//! lowerings that use it compile only when the caller says this method's
//! exception table is empty ([`Arm64Backend::set_exception_table_empty`]).
//! `arraylength` (`0xbe`) is its first user and the second opcode
//! [`opcode_has_ordered_lowering`] lets past the gate.
//!
//! **Round 9 wave 13: array elements.** The `ArrayIndexOutOfBoundsException`
//! path ([`Arm64Backend::emit_aioobe_stubs`], one stub per SITE because the
//! helper takes the index, the length, the array and the bci) joins the NPE
//! one, and on the two together the fourteen primitive array element accesses
//! lower: a null check, an UNSIGNED bounds compare against the header's length
//! word (which catches a negative index in the same branch, as x64's `JAE`
//! does), a scaled address (`ADD Xd, Xn, Xm, LSL #log2(size)`), and one load
//! or store of the element's own width. `bastore` reads the array header's
//! kind/element byte and masks `& 1` only for a real `boolean[]`, which is
//! what x64 does and what JVMS §6.5 requires. None of these accesses is
//! ordered, and that IS the lowering: an array element is ordinary memory the
//! JMM owes nothing about, and every address here is derived from the array
//! reference, so ARMv8's address-dependency rule already orders it after the
//! load that produced that reference.
//!
//! **Round 9 wave 15: a call from the MIDDLE of a method, and `putfield` on
//! it.** Every call this backend emitted before was one of two shapes that
//! dodge the hard part: at method entry with an EMPTY operand stack
//! (`emit_frame_record`, the entry poll), or in a stub on the way OUT of the
//! frame (the three throw stubs), where nothing has to survive.
//! [`Arm64Backend::emit_helper_call`] is the general one — the CALLER allocates
//! the result register first, store every register-located operand to its slot,
//! marshal the arguments into X0-X7 (disjoint from the scratch pool the
//! sources come from, which it checks), `BLR` through X16, take the result out
//! of X0, reload. Its first user is [`Arm64Backend::emit_putfield`]: a null
//! check with `npe_action::NONE` (the helper's own guard returns WITHOUT
//! raising, so a `putfield` on null through it would silently drop the store),
//! JVMS §6.5 narrowing to the field's declared type, and
//! `jit_putfield_int`/`_long`/`_float`/`_double`, which take all three
//! arguments in integer registers and no VM pointer. A `volatile` field gets
//! `DMB ISH` on BOTH sides — the trailing one is the StoreLoad edge x64 pays
//! with `MFENCE`, and the LEADING one is the release edge x64 gets free from
//! TSO and this backend does not, because the store happens inside the helper
//! as a relaxed store and a `BLR` orders nothing. A reference field still
//! refuses (no SATB pre-barrier, no card mark).
//!
//! **Round 9 wave 16: the context ABI, and `getfield` on it.** `jit_getfield`
//! takes the VM pointer, which this backend's entry did not carry. It does
//! now, for a method that contains a `getfield`: the artifact is published
//! with `CompiledMethod::needs_context`, the VM enters through
//! `try_call_with_context`, the context arrives in X0 AHEAD of every Java
//! argument, the prologue homes it to a frame word (last in
//! [`Arm64SpillArea`], so nothing moved) and the argument homing shifts by
//! one. The flag and the shift come from the same field, because a body
//! compiled without a context and called with one reads its first Java
//! argument from the context pointer.
//!
//! [`Arm64Backend::emit_getfield`] then null-checks, reads the context out of
//! its word, calls, and TESTS THE SENTINEL. `jit_getfield` returns `i64::MIN`
//! only after setting a pending exception, so leaving through the epilogue
//! with it raises rather than re-runs — which is why this lowering needs no
//! deopt point. For `int`/`boolean`/`byte`/`char`/`short`/`float` the sentinel
//! is unambiguous. For `long` and `double` it is NOT: a `long` field holding
//! `Long.MIN_VALUE` returns exactly `i64::MIN`, and so does a `double` field
//! holding `-0.0`, whose bit pattern IS `i64::MIN`. Those two ask
//! `helpers.dispatch_threw` on the equal branch, exactly as x64's `J`/`D` call
//! sites do, and keep the value when it answers 0. Every `alloc_reg` happens
//! before that branch, so both paths reach the join with the same operand
//! model. A reference `getfield` refuses: no narrow-oop decode and no
//! operand-stack oop story.
//!
//! **Round 9 wave 17: `putstatic`, the last of the four field opcodes.** A raw
//! store into the statics block races `set_static_shared`'s `grow_to`, which
//! copies the block under the write lock and republishes it, so a compiled
//! store into the old block between the copy and the republish is LOST. The
//! helper takes that lock, and taking a lock is a call — which is why the
//! read two paragraphs up can be two loads and the write cannot be its mirror
//! image.
//!
//! `jit_putstatic_*` CAN run `<clinit>`, i.e. can run Java, safepoint and move
//! objects — and this call sequence records no oop map. What makes that
//! sound is that the class is already initialized at COMPILE time: the caller
//! resolves a `base_cell` through `resolve_static_base`, which answers only
//! for an initialized class, and [`Arm64Backend::emit_putstatic`] refuses a
//! site without one even though it never reads the cell. Initialization is
//! monotonic, and the declaring class is additionally recorded in
//! `static_init_classes`, so the `<clinit>` branch is unreachable from here.
//! The sentinel is still tested, against `i64::MIN` specifically rather than
//! against zero: `jit_putstatic_*` returns `i64::MIN` only after setting a
//! pending exception, so leaving with it THROWS rather than re-running the
//! method, and a spurious bail on some other non-zero value would leave with
//! the sentinel and no exception — which the interpreter reads as a deopt,
//! re-running the method and double-executing this very store. A `volatile`
//! static gets `DMB ISH` on both sides, for the same reason `putfield` does.
//!
//! **Round 9 wave 18: a call that can SAFEPOINT, and allocation on it.**
//! `emit_helper_call` records no oop map, and says so as an obligation each
//! caller discharges for ITS helper -- the three field helpers each have a
//! specific argument for why they cannot safepoint.
//! [`Arm64Backend::emit_helper_call_at_safepoint`] is for the ones that have
//! no such argument. It is the loop-header poll's sequence, literally the same
//! code: store the operands to their depth slots, store the register-homed
//! reference LOCALS to their safepoint homes, stamp the id, call, record the
//! map at the return address, reload. The reload is the point, not
//! housekeeping -- a callee-saved register survives the call inside the
//! CALLEE's save area, where only a conservative walk sees it, and a
//! conservative walk marks without rewriting.
//!
//! Two frame consequences. A method that ALLOCATES now gets the safepoint
//! homes and the id word whether or not `CRATONVM_JIT_ARM64_SAFEPOINTS` is on,
//! because it stops inside a moving callee either way. And the homes are sized
//! by the number of register-homed LOCALS rather than by how many distinct
//! callee-saved registers the allocator used: the two differ when two locals
//! with disjoint live ranges share one register, and the poll had been
//! refusing such a method rather than homing it.
//!
//! [`Arm64Backend::emit_allocation`] lowers `new`, `newarray` and `anewarray`
//! on it. `newarray` needs no resolution (its `atype` is an operand byte); the
//! other two need the class the caller resolved into an [`Arm64NewSite`]. All
//! three return `0` having stashed a pending exception -- `OutOfMemoryError`,
//! `NegativeArraySizeException`, a failed `<clinit>` -- so the null check
//! leaves through the epilogue for the drain to raise, and the lowering
//! therefore requires an empty exception table. No fence is owed: the object
//! is unreachable by construction until a later store publishes it, and that
//! store carries its own ordering.
//!
//! **Round 9 wave 19: the reference stores.** A reference store owes two
//! barriers this backend emits nowhere — the SATB pre-write barrier (log the
//! OLD value, or a concurrent marker loses the only path to a still-live
//! object) and the card mark — and it owes whatever ENCODING the collector is
//! using: a compressed-oops slot is a 4-byte `(addr - base) >> 3`, an armed
//! ZGC slot is a coloured word that is not an address at all. So all four
//! reference accesses go through the helper that knows both:
//! `jit_putfield_object`, `jit_putstatic_object`, `jit_aaload` and
//! `jit_aastore`.
//!
//! `aastore` is the first bytecode here to make TWO calls.
//! `jit_aastore` returns `()`, so a compiled caller cannot see that it refused
//! the store — its `ArrayStoreException` travels by the pending-signal
//! channel and would surface only at method return, after this frame had run
//! on. `jit_aastore_type_check` answers `0` or the sentinel, so the arm calls
//! it first, exactly as x64's inline lowering does. Two calls means the array
//! and the value have to survive the first: they are LEFT ON THE OPERAND
//! STACK and reached with [`Arm64Backend::materialize_entry`], because the
//! spill-and-reload around a call preserves exactly the entries the model
//! still holds — and, being reference operands, both are named in the map, so
//! a collector that moves them during the type check hands the store their new
//! addresses.
//!
//! The guards stay this backend's own for both array forms: the helpers report
//! a null array or a bad index through the pending-signal channel, which
//! surfaces only at method return, while an inline `CBZ` and an unsigned
//! bounds compare keep the exception at the bytecode that caused it.
//!
//! **Round 9 wave 21: the frame base an allocating method never published.**
//! Wave 18 gave an allocating method the safepoint homes and the id word on
//! the argument that an allocation stops the frame inside a moving callee
//! whether or not polls are on — and left [`Arm64Backend::emit_frame_record`]
//! gated on polls. The runtime reads the safepoint id at
//! `[frame_base - sp_id_slot_off]` and each map slot at `[frame_base - off]`,
//! and it learns `frame_base` only from that call, so an allocating method
//! recorded maps that `PreciseFrameInfo::exact_rbp == 0` made unreadable. It
//! is the same defect wave 18's own argument was written to prevent, one level
//! up. Both now come from one predicate,
//! [`Arm64Backend::wants_safepoint_frame`], and a published base is a term of
//! `fully_oop_covered` — which the wave then WIDENED to an allocation-only
//! method, because with the base published the claim is finally about
//! something. See `publish_compiled_method`.
//!
//! **Round 9 wave 22: a CALL.** Every `invoke*` but `invokedynamic` lowers,
//! through `jit_invoke_dispatch`. What ended the three-page-old blockage was
//! not call-target resolution but noticing that the helper does its own: it
//! takes the site's `JitInvokeInfo` and resolves the target at run time, so a
//! direct call, an inline cache, an outgoing Java argument ABI and a
//! self-recursion guard — the four pieces the page split the work into — are
//! all on the far side of ONE helper call, which this backend has had since
//! wave 18. The cost is the speed of a direct call, which is the right trade
//! against not compiling the method.
//!
//! The argument buffer is the OPERAND AREA itself. The helper reads
//! `args[0]` at the lowest address, one word per operand VALUE; this frame's
//! operand words are indexed by depth and ascend in address with it; so the
//! deepest argument already sits at the base of a contiguous ascending run of
//! exactly the right words, and the spill the call performs anyway fills it.
//! That also makes the oop story better than x64's, where an outgoing
//! reference argument is staged where no frame-slot map can name it and the
//! method loses `fully_oop_covered` permanently for it. Here the arguments are
//! still operand entries when the map is taken.
//!
//! **Round 9 wave 23: `checkcast` and `instanceof`,** through `jit_checkcast`
//! and `jit_instanceof_check`. Both take the target class as an interned NAME
//! rather than an id, both can allocate (resolving the target on first use
//! creates its `java/lang/Class` mirror) and so both record a map, and the
//! object under test stays on the operand stack across the call for the reason
//! `aastore` established. None of x64's inline fast paths is emitted: they
//! read the receiver's class id out of the header, which this backend has no
//! other reason to do.
//!
//! See `docs/known-issues/jit/aarch64-backend-runs-no-java-on-a-real-machine.md`.
//!
//! Consequences worth stating plainly:
//! - **No inline caches.** x64 has MIC/PIC inline caches for
//!   `invokevirtual`/`invokeinterface`. This backend has the generic
//!   slow-path helper and nothing in front of it, so every call pays a full
//!   dispatch — correct, and slower than x64 by the width of an inline cache.
//! - **No inline TLAB bump allocation** (x64 has one). `new` lowers since
//!   wave 18, but always through `jit_new_object`; the inline bump is a
//!   performance refinement that also raises a question this backend has not
//!   answered (whether `tlab_post_init` may move the object it is handed,
//!   which x64's arrangement assumes it cannot).
//! - **No exception handling**, no `exception_table` consultation, no
//!   handler dispatch.
//!
//! ## Safety-critical gaps (these are the reason for the warning above)
//!
//! A full mechanism-by-mechanism comparison against the x86-64 backend lives in
//! `docs/jit/aarch64-parity.md`. The short version:
//!
//! - **GC safepoint polls: BUILT, and OPT-IN
//!   (`CRATONVM_JIT_ARM64_SAFEPOINTS`, default-OFF).** Updated 2026-09-03. x64
//!   emits a cooperative poll of `helpers.safepoint_flag_addr` at method entry
//!   and at every loop back-edge; this backend emitted none, and the header
//!   used to say it "cannot: no helper address is plumbed in, and taking a poll
//!   needs a CALL, which `emit_invoke` refuses". Both halves of that are now
//!   addressed: [`Arm64Backend::set_helpers`] plumbs the table in, and the poll
//!   emits its own `BLR` rather than going through `emit_invoke` (which refuses
//!   *bytecode* invokes because it has no call-target resolution — a different
//!   problem).
//!
//!   [`Arm64Backend::emit_safepoint_poll`] emits the x64 shape: materialize the
//!   flag address, `LDRB` **one byte** of it (the flag is an `AtomicBool`, and
//!   a 64-bit load would fold the `GcBarrier` counters after it into the test),
//!   `CBZ` past the slow path, spill the caller-saved operand registers, `BLR`,
//!   record the oop map at the return address, reload. It runs at method entry
//!   and at each loop header, and with it on
//!   [`Arm64Backend::label_for_pc`] no longer refuses backward branches — that
//!   refusal existed precisely because a compiled loop with no poll is a region
//!   a stop-the-world request can never interrupt, so **loops compile again**.
//!
//!   **It is default-OFF and that is deliberate.** No CI runner or developer
//!   host in this repository can EXECUTE aarch64, so the evidence for it is
//!   instruction-word assertions and pseudo-op structure — everything a
//!   non-aarch64 host can honestly prove, and not the same as "it works".
//!   Default-on would be publishing an unexecuted calling sequence into a GC's
//!   stop-the-world protocol. With it off, this backend is byte-identical to
//!   before: no poll, and backward branches still refused.
//! - **Oop maps: the WRITER works; there is no safepoint to call it at.**
//!   Updated 2026-09-03. [`Arm64Backend::mark_top_operand_as_oop`] is called
//!   from three opcode arms (`aconst_null`, `aload`, `aload_0..3`), so
//!   references really do flow through these frames. Its consumer,
//!   [`Arm64Backend::emit_oop_map_for_safepoint`], used to key its map as
//!   `instruction_count * 4` — wrong for this pseudo-op stream, since `Label`
//!   and `Comment` emit nothing, `ConstantPoolEntry` emits 8 bytes and
//!   `MovImm`/`AddImm`/`CmpImm` and out-of-range `Ldr`/`Str` expand to 1–4
//!   words — and the 2026-08-01 audit made it fail the method closed rather
//!   than let a caller inherit that.
//!
//!   It is now keyed the way that audit prescribed: off the ENCODER's byte
//!   offset. The compiler records an [`Arm64PendingOopMap`] against the
//!   pseudo-op INDEX of the instruction following the safepoint — a distinct
//!   type, so an unresolved PC cannot be mistaken for a resolved one — and
//!   [`emit_machine_code_with_oop_maps`] translates it once the encoder knows
//!   where each pseudo-op landed. A map it cannot place discards the method.
//!   [`publish_compiled_method`] then attaches the result to the artifact,
//!   which the `cfg`-gated caller previously did not do at all.
//!
//!   **The first caller arrived 2026-09-03**: the safepoint poll above records
//!   a map at its `BLR`'s return address, naming the operand slots it spilled.
//!   With polls off (the default) `pending_oop_maps` is still empty and the GC
//!   walker still takes its conservative fallback, exactly as before.
//!
//!   **Reference LOCALS are named too, as of 2026-09-03.** A frame-homed one
//!   is named where it already lives. A REGISTER-homed one (X19-X28) is stored
//!   to a home slot reserved for it, named, and reloaded after the call: those
//!   registers are callee-saved, so the value survives on its own, but it
//!   survives inside the CALLEE's saved-register area where only the
//!   conservative walk can see it -- and a conservative walk marks without
//!   being able to REWRITE. A relocating collector could not otherwise move an
//!   object whose only root was a register local. Which locals hold references
//!   comes from the flow-sensitive `compute_local_oop_masks` shared with x64,
//!   not from a whole-method approximation, because naming a primitive would
//!   hand a relocating collector a non-pointer to rewrite.
//!
//!   **The safepoint-id slot landed 2026-09-04**, and with it the frame-base
//!   publication it is useless without. Each poll stamps its site's bci (or
//!   `ENTRY_POLL_BC_PC`) into a reserved frame word, the prologue stamps
//!   `SP_ID_UNSET_BC_PC` there first so an uninitialised slot cannot read as a
//!   valid id, every map carries the same value as its `bytecode_pc`, and
//!   `emit_frame_record` publishes FP through `helpers.frame_record` so the
//!   runtime can locate the frame to read it. The collector can therefore
//!   select the map for the site a frame is ACTUALLY standing at
//!   (`find_oop_map_for_safepoint_id`) rather than a union over the method.
//!
//!   **`fully_oop_covered` IS COMPUTED as of 2026-09-04**, from four terms
//!   each of which can sink it: an id slot exists; at least one safepoint was
//!   emitted (a method with no poll is not "covered", it is unobserved); every
//!   safepoint published a map, so every id resolves; and no safepoint failed
//!   to describe what was live at it. It can only be true with
//!   `CRATONVM_JIT_ARM64_SAFEPOINTS` on, so a default build is unchanged and
//!   keeps its conservative scan.
//!
//!   Getting there required fixing something that would have made the claim
//!   unsound: the operand oop MARKS were not kept in lockstep with the operand
//!   stack. `push_operand` pushed no mark and `pop_operand` popped none, so a
//!   mark outlived the value it described and was re-read for whatever later
//!   occupied that index -- naming a primitive as a reference (a relocating
//!   collector rewrites a non-pointer) or losing a reference (with the
//!   conservative scan suppressed, a use-after-free). They are lockstep now,
//!   `dup` carries its mark to both copies, and references enter only through
//!   `aconst_null` and `aload*`, both of which mark -- so the marks are exact
//!   by construction.
//!
//!   **What the claim rests on, stated because it is the whole risk: none of
//!   this has ever been EXECUTED.** No host in this repository runs aarch64.
//!   The evidence is instruction encodings, pseudo-op structure and the
//!   construction arguments above. `CRATONVM_DBG_VERIFY_OOP_MAPS` -- the
//!   runtime oracle that walks a live frame and refutes a coverage claim it can
//!   disprove -- is what turns that into evidence, and it should be armed on
//!   the first aarch64 run before this flag is trusted.
//! - **No deoptimization and no OSR.** Neither word appears in this file.
//!   There is no frame reconstruction, no uncommon-trap stub, no
//!   `osr_pc_to_native` table. There is nothing to tier down *from* (this is
//!   the only tier), so a deopt cannot occur — but equally, no speculative
//!   optimization may ever be added here without building that first.
//! - **Stack bang (2026-09-12).** The prologue touches every page the frame
//!   crosses before it moves SP, as x64 does, so stack exhaustion faults on the
//!   guard page. Frames of 4096 bytes or more used to be refused instead, and
//!   could not have been allocated anyway: a wide `SUB SP` had no encoding
//!   until the extended-register ADD/SUB was added.
//! - **Float locals are not homed in FP registers.** `regalloc::ARM64_LOCAL_FPS`
//!   offers `D8`–`D15`, which AAPCS64 makes callee-saved, and this backend's
//!   prologue/epilogue save only GPRs — so homing a float local there destroyed
//!   the caller's copy. The allocator's FP assignments are ignored (2026-08-01);
//!   float locals live in frame slots.
//!
//! ## The `int` representation (2026-09-12)
//!
//! An `I32` operand is held SIGN-EXTENDED in its 64-bit register: the X
//! register equals the sign extension of its low 32 bits. That is how the VM
//! passes an `int` argument (`x as i64`) and how `iconst` materializes one, and
//! it makes every 64-bit reader -- the VM's read of X0, `CBZ X`, an `i2l` -- see
//! the right value. Every `int` producer is a W-form instruction (whose result
//! is the JVMS 32-bit wrapped value, and whose variable shifts take the
//! distance MOD 32) followed by `SXTW`; `f2i`/`d2i` use the 32-bit saturating
//! `FCVTZS W`; `l2i` is `SXTW`; `i2l` is a relabelling; and `int` compares,
//! zero tests and switch keys read the W register regardless. The previous
//! lowering used the X forms of the `long` ops, so `Integer.MAX_VALUE + 1` was
//! 2147483648 and `1 << 32` was 4294967296.
//!
//! `float` is an S register and `double` a D register, end to end: constants,
//! arithmetic, compares, conversions, locals (four bytes of a frame word for a
//! `float`) and returns (moved bit-exactly into X0, where the VM reads every
//! result).
//! - **Loops were infinite self-branches** until the 2026-07-26 audit. Branch targets
//!   were discovered lazily as each branch was decoded, so a back-edge target
//!   (already walked past) never got a label bound, and the encoder left the
//!   displacement-0 placeholder — `B .`. Fixed by giving
//!   [`Arm64Backend::compile_method_with_info`] a discovery pass; any label
//!   that is still unbound now bails the method rather than emitting a
//!   self-branch. See `emit_machine_code`.
//! - **`idiv`/`ldiv`/`irem`/`lrem` bailed** as of this audit. `idiv`'s
//!   divide-by-zero guard branched to `BRK #1`, which raises SIGTRAP — the
//!   process dies instead of throwing `ArithmeticException` (nothing in the
//!   VM converts SIGTRAP). `irem`/`lrem` had no zero check at all, and
//!   AArch64 `SDIV` by zero yields 0 rather than trapping, so `x % 0`
//!   silently returned `x`.
//!
//!   **Round 9 wave 11: the throw path exists.** A `CBZ` of the divisor
//!   branches to one out-of-line stub per method that `BLR`s
//!   `helpers.throw_arithmetic` (the helper x64's reason-3 deopt stub calls:
//!   it sets the pending-arithmetic and deopt signals and returns the
//!   `i64::MIN` sentinel) and leaves through the shared epilogue; the
//!   interpreter's JIT-return drain raises the `ArithmeticException` without
//!   re-running the method. The drain throws with an UNKNOWN throw pc, which
//!   is exact only when this frame has no handler, so the four opcodes lower
//!   only when the caller also said the exception table is empty
//!   ([`Arm64Backend::set_exception_table_empty`]); otherwise, or with no
//!   helper wired, they are refused as before. See
//!   `Arm64Backend::emit_int_div_rem` and the `0x6c`/`0x6d`/`0x70`/`0x71`
//!   arm.
//!
//! ## Reachability
//!
//! `jit/src/lib.rs` dispatches here from exactly one place: the
//! `#[cfg(target_arch = "aarch64")]` block at the top of `try_compile_inner`,
//! which returns unconditionally (the IR pipeline and the x64 backend are
//! bypassed entirely on that target) -- and which compiles NOTHING unless
//! `CRATONVM_JIT_ARM64` is set (see [`arm64_jit_enabled`]). The VM's two other
//! compile doors that call `x64::compile_with_param_slots` directly, the eager
//! first call (`vm/src/runtime/interpreter.rs`) and OSR (`compile_osr_artifact`
//! in `vm/src/runtime/interpreter/jit_bridge.rs`), return without compiling on
//! any target but x86-64, so an aarch64 build cannot publish x86-64 bytes.
//!
//! Both this module and `aarch64.rs` are compiled unconditionally on every
//! host (`pub mod` in `lib.rs`, no `cfg`), so their unit tests — including
//! every instruction-encoding test — run in ordinary x86-64 CI. The code is
//! not rotting; it is simply far smaller in scope than its name suggests.
//!
//! ## Calling convention
//!
//! The VM calls compiled code as `extern "C" fn(i64, ..) -> i64`: ONE 64-bit
//! integer register per argument (X0-X7, `this` first; more than eight
//! arguments refuse the method), and every result read out of X0. A `float`
//! argument arrives as its zero-extended bit pattern and a `double` as its
//! bits, and FP results are moved bit-exactly into X0. Argument `i` is homed
//! in JVM local `compute_param_jvm_slots(..)[i]`, which differs from `i` after
//! any `long`/`double`. Callee-saved: X19-X28, FP (X29), LR (X30). SP is
//! 16-byte aligned at all times.
//!
//! ## Stack layout (after the prologue)
//!
//! ```text
//! [FP + 8]         saved LR          \  the AAPCS64 frame record
//! [FP]             saved caller FP   /  (STP X29, X30, [SP, #-16]!; ADD X29, SP, #0)
//! [FP - 8] ..      callee-saved GPRs (register-homed locals)
//! ..               frame-homed locals, then one word per operand-stack depth,
//!                  then the safepoint homes and the safepoint-id word
//! [SP]             frame bottom
//! ```
//!
//! See [`Arm64FrameLayout::compute`]. The record used to sit at `[FP-16]`, with
//! FP equal to the caller's SP, which no unwinder or frame-pointer walk expects.

use cratonvm_jit_api::npe_action;
use std::collections::HashMap;
#[cfg(test)]
mod i109_l2_cold_field_site_tests;
// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;

mod arm64_register;
#[allow(unused_imports)]
pub use arm64_register::*;
#[allow(unused_imports)]
use arm64_register::*;
mod mark_top_operand_as_oop;
#[allow(unused_imports)]
pub use mark_top_operand_as_oop::*;
#[allow(unused_imports)]
use mark_top_operand_as_oop::*;
mod to_fpreg;
#[allow(unused_imports)]
pub use to_fpreg::*;
#[allow(unused_imports)]
use to_fpreg::*;
