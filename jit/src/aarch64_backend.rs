// SPDX-License-Identifier: Apache-2.0
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
//! See `docs/known-issues/jit/aarch64-backend-runs-no-java-on-a-real-machine-20260922.md`.
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

// ---------------------------------------------------------------------------
// Arm64Register
// ---------------------------------------------------------------------------

/// Lightweight register identifier for the backend pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Arm64Register(pub u8);

impl Arm64Register {
    // Argument / result registers
    pub const X0: Self = Self(0);
    pub const X1: Self = Self(1);
    pub const X2: Self = Self(2);
    pub const X3: Self = Self(3);
    pub const X4: Self = Self(4);
    pub const X5: Self = Self(5);
    pub const X6: Self = Self(6);
    pub const X7: Self = Self(7);

    // Scratch / temporary registers
    pub const X8: Self = Self(8);
    pub const X9: Self = Self(9);
    pub const X10: Self = Self(10);
    pub const X11: Self = Self(11);
    pub const X12: Self = Self(12);
    pub const X13: Self = Self(13);
    pub const X14: Self = Self(14);
    pub const X15: Self = Self(15);
    pub const X16: Self = Self(16); // IP0
    pub const X17: Self = Self(17); // IP1
    pub const X18: Self = Self(18); // Platform register

    // Callee-saved registers (for locals)
    pub const X19: Self = Self(19);
    pub const X20: Self = Self(20);
    pub const X21: Self = Self(21);
    pub const X22: Self = Self(22);
    pub const X23: Self = Self(23);
    pub const X24: Self = Self(24);
    pub const X25: Self = Self(25);
    pub const X26: Self = Self(26);
    pub const X27: Self = Self(27);
    pub const X28: Self = Self(28);

    // Special registers
    pub const FP: Self = Self(29);
    pub const LR: Self = Self(30);
    pub const SP: Self = Self(31);
    pub const XZR: Self = Self(31); // Context-dependent zero register

    // NEON SIMD registers (encoded as 32+n)
    pub const V0: Self = Self(32);
    pub const V1: Self = Self(33);
    pub const V2: Self = Self(34);
    pub const V3: Self = Self(35);
    pub const V4: Self = Self(36);
    pub const V5: Self = Self(37);
    pub const V6: Self = Self(38);
    pub const V7: Self = Self(39);

    /// Returns `true` if this is a callee-saved general-purpose register (X19-X28).
    pub fn is_callee_saved(self) -> bool {
        (19..=28).contains(&self.0)
    }

    /// Returns `true` if this register is used for integer argument passing.
    pub fn is_arg_reg(self) -> bool {
        self.0 <= 7
    }

    /// Raw numeric index.
    pub fn index(self) -> u8 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Arm64Condition
// ---------------------------------------------------------------------------

/// ARM64 condition codes mapping to the NZCV flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm64Condition {
    /// Equal (Z=1)
    Eq,
    /// Not equal (Z=0)
    Ne,
    /// Signed less than (N!=V)
    Lt,
    /// Signed less or equal (Z=1 or N!=V)
    Le,
    /// Signed greater than (Z=0 and N=V)
    Gt,
    /// Signed greater or equal (N=V)
    Ge,
    /// Unsigned higher (C=1 and Z=0)
    Hi,
    /// Unsigned lower or same (C=0 or Z=1)
    Ls,
    /// Carry set / unsigned higher or same (C=1)
    Cs,
    /// Carry clear / unsigned lower (C=0)
    Cc,
    /// Minus / negative (N=1). After `FCMP` this is "less than" and is FALSE on
    /// unordered, which is what `fcmpg`/`dcmpg` need (`Lt` is true on it).
    Mi,
    /// Plus / positive or zero (N=0)
    Pl,
    /// Overflow (V=1). After `FCMP`: unordered.
    Vs,
    /// No overflow (V=0)
    Vc,
    /// Always
    Al,
}

// ---------------------------------------------------------------------------
// Arm64Instruction
// ---------------------------------------------------------------------------

/// The access width of an [`Arm64Instruction::MemLoad`] /
/// [`Arm64Instruction::MemStore`] (round 9 wave 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm64MemWidth {
    /// One byte: `LDRB`/`STRB`, `LDARB`/`STLRB`.
    B8,
    /// Two bytes: `LDRH`/`STRH`, `LDARH`/`STLRH`.
    H16,
    /// Four bytes: `LDR Wt`/`STR Wt`, `LDAR Wt`/`STLR Wt`.
    W32,
    /// Eight bytes: `LDR Xt`/`STR Xt`, `LDAR Xt`/`STLR Xt`.
    X64,
}

/// A `getstatic` site resolved by the caller (round 9 wave 10).
///
/// The same facts the x64 single-pass tier's inline `getstatic`
/// (`x64/objects.rs` `try_emit_inline_getstatic`) works from:
///
/// * `base_cell` -- the address of the class's never-freed statics-base
///   POINTER cell, i.e. `DirectHelperTable::resolve_static_base(class_id,
///   field_index)`. The compiled read is `[[base_cell] + field_index *
///   SLOT_SIZE + payload]`, exactly x64's two dependent loads plus the cell
///   read. The resolver answers only for a class that is already
///   initialized, and initialization is monotonic, so no class-init check is
///   emitted -- and `class_id` is also recorded on the artifact
///   (`CompiledMethod::static_init_classes`) as x64 does;
/// * `type_tag` -- the field descriptor's first byte. Only primitives lower;
///   anything else refuses the method;
/// * `is_volatile` -- whether the read must be an `LDAR` (JMM acquire) rather
///   than a plain `LDR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64StaticField {
    pub class_id: u32,
    pub field_index: usize,
    pub base_cell: u64,
    pub type_tag: u8,
    pub is_volatile: bool,
}

/// Every `getstatic` (`0xb2`) and `putstatic` (`0xb3`) site in `bytecode` as
/// `(pc, opcode, constant-pool index)`, found by an instruction-length walk
/// (never a byte scan, which would read an operand byte as an opcode). Stops
/// at the first undecodable instruction. The caller resolves each index into
/// an [`Arm64StaticField`] and hands the map to
/// [`Arm64Backend::set_static_field_info`].
///
/// Both opcodes, since round 9 wave 17: the two need the same four facts, and
/// `base_cell` is what BOTH of them treat as the caller's proof that the
/// declaring class is already initialized — the read because it dereferences
/// that cell, the write because it may not let `<clinit>` run inside its
/// helper.
pub fn static_field_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if op == 0xb2 || op == 0xb3 {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// A `getfield`/`putfield` site resolved by the caller (round 9 wave 15).
///
/// The instance twin of [`Arm64StaticField`], and deliberately much smaller:
/// a static read is a pair of dependent loads off a never-freed cell this
/// backend can bake as an immediate, while an instance access goes through a
/// runtime helper, which needs only the slot index and the field's type.
///
/// * `field_index` -- the resolved slot, the same number
///   `cp_field_resolver` hands the x64 tier;
/// * `type_tag` -- the field descriptor's first byte. Only primitives lower;
///   a reference field refuses the method, because its store owes the SATB
///   pre-barrier and the card write-barrier that only `jit_putfield_object`
///   runs;
/// * `is_volatile` -- whether the access owes the JMM's fences. For a store
///   THROUGH A HELPER that is a `DMB ISH` on BOTH sides; see
///   [`Arm64Backend::emit_putfield`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64InstanceField {
    pub field_index: usize,
    pub type_tag: u8,
    pub is_volatile: bool,
}

/// Every `getfield` (`0xb4`) and `putfield` (`0xb5`) site in `bytecode` as
/// `(pc, opcode, constant-pool index)`.
///
/// The instance-field twin of [`static_field_sites`], and an instruction-length
/// walk for the same reason: a byte scan would read a `0xb4` that is some
/// other instruction's operand as an opcode and resolve a constant-pool
/// index that site never names. The caller resolves each index into an
/// [`Arm64InstanceField`] and hands the map to
/// [`Arm64Backend::set_instance_field_info`].
pub fn instance_field_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if op == 0xb4 || op == 0xb5 {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// A `new` (`0xbb`) or `anewarray` (`0xbd`) site resolved by the caller
/// (round 9 wave 18).
///
/// The aarch64 half of what `cp_new_resolver` reports as
/// [`crate::JitNewSite::Resolved`]. Only the resolved case is carried: a
/// `Deferred` site names a class nothing has loaded yet, and the CP-indexed
/// helper that resolves it at run time (`new_object_cp`) is a second entry
/// point this backend does not lower.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64NewSite {
    /// For `new`, the class to allocate. For `anewarray`, the COMPONENT
    /// class — the array's own class is derived from it by the helper.
    pub class_id: u32,
    /// The instance field count the header's `num_slots` is set from. Read
    /// only by `new`; an `anewarray`'s length is an operand, not a constant.
    pub num_fields: usize,
}

/// Every `multianewarray` (`0xc5`) site: `(pc, cp_index)`, h23c.
///
/// An INSTRUCTION walk, like every other site scan here: a `0xc5` byte that is
/// some other instruction's operand is not an allocation, and resolving it
/// would hand the backend a constant-pool index that site never names. The
/// arity byte is deliberately NOT returned -- the backend reads it off the
/// opcode when it lowers, so the two can never disagree about which byte the
/// count came from.
pub fn multianewarray_sites(bytecode: &[u8]) -> Vec<(usize, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if op == 0xc5 {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// Every allocation site in `bytecode` as `(pc, opcode, operand)`:
/// `new` (`0xbb`) and `anewarray` (`0xbd`) with their constant-pool index,
/// `newarray` (`0xbc`) with its `atype` byte.
///
/// An instruction-length walk, for the same reason as [`static_field_sites`]
/// and [`instance_field_sites`]: a `0xbb` that is some other instruction's
/// operand is not an allocation, and resolving it would name a class this
/// method never mentions.
///
/// Two callers, and they want different halves of it. `jit/src/lib.rs`
/// resolves the `0xbb`/`0xbd` indices into [`Arm64NewSite`]s; `compile_pass`
/// asks only whether the list is EMPTY, because a method that allocates needs
/// a frame with safepoint homes and an id word whether or not polls are on.
/// Whether any INSTRUCTION in `bytecode` has an opcode `pred` accepts.
///
/// An instruction-length walk, like the three site tables above and for the
/// same reason: a byte that matches inside another instruction's operand is
/// not that instruction. Used by `compile_pass` for the two decisions it must
/// make from the bytecode alone, before the frame is sized -- whether this
/// method takes the VM context, and whether it makes a call that can
/// safepoint.
pub fn any_opcode(bytecode: &[u8], pred: fn(u8) -> bool) -> bool {
    let mut pc = 0usize;
    while pc < bytecode.len() {
        if pred(bytecode[pc]) {
            return true;
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    false
}

pub fn allocation_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        match op {
            0xbb | 0xbd => {
                if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                    sites.push((pc, op, u16::from_be_bytes([hi, lo])));
                }
            }
            0xbc => {
                if let Some(&atype) = bytecode.get(pc + 1) {
                    sites.push((pc, op, u16::from(atype)));
                }
            }
            _ => {}
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// Every `invoke*` in `bytecode`, as `(pc, opcode, constant-pool index)`.
///
/// An instruction-length walk, like the other site tables. The three widths
/// matter here more than anywhere else: `invokeinterface` (`0xb9`) is FIVE
/// bytes (index, an argument count, a zero) and `invokedynamic` (`0xba`) is
/// five as well (index, two zeros), so a naive three-byte stride reads the
/// trailing bytes as an opcode.
pub fn invoke_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if matches!(op, 0xb6..=0xba) {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// Every `checkcast`/`instanceof` in `bytecode`, as `(pc, opcode, cp index)`.
pub fn typecheck_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if matches!(op, 0xc0 | 0xc1) {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// One `checkcast`/`instanceof` site (round 9 wave 23): the target class as
/// the two things `jit_checkcast` / `jit_instanceof_check` take, a name and a
/// length.
///
/// A NAME rather than a class id, because that is the helpers' ABI -- and
/// deliberately the process-INTERNED name (`intern_typecheck_target_*`), not a
/// string this artifact owns: the helpers memoize on `(ptr, len)` in
/// thread-locals that outlive any one compilation, so a per-artifact copy
/// would miss the memo on every call and dangle in it afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64TypecheckSite {
    /// Address of the interned name bytes.
    pub name_ptr: u64,
    /// Their length. Zero is "unresolved", which the helper answers `false`
    /// to -- a WRONG answer rather than a missing one, so this backend
    /// refuses such a site instead of passing it on.
    pub name_len: usize,
}

/// One `invoke*` site, as the CALLER resolved it (round 9 wave 22).
///
/// The whole of call-target resolution, reduced to what
/// [`Arm64Backend::emit_invoke`] needs. Nothing here is an entry point: the
/// target is named, not addressed, and `jit_invoke_dispatch` resolves it at
/// run time through the `JitInvokeInfo` this points at. That is the reason
/// this backend can have calls at all without the four pieces x64 needs -- see
/// the wave-22 section of the module header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64InvokeSite {
    /// Address of the [`crate::JitInvokeInfo`] describing this site.
    ///
    /// Baked into the code as an immediate, so the box has to outlive the
    /// artifact: `lib.rs` parks it in `CompiledMethod::_jit_invoke_infos`,
    /// which is what every x64 dispatch site does with its own.
    pub info_ptr: u64,
    /// Operand-stack entries this site consumes: one per declared parameter
    /// (a `long` or `double` is ONE, this VM's compact convention), plus the
    /// receiver for every kind but `invokestatic` and `invokedynamic`.
    ///
    /// The same number `JitInvokeInfo::num_jit_args` carries, because the
    /// helper indexes the argument buffer by it.
    pub num_jit_args: usize,
    /// The descriptor's return tag: `V` for void, `L`/`[` for a reference.
    pub return_type: u8,
}

/// Backend IR instruction set for the ARM64 pipeline.
///
/// These are pseudo-instructions that map 1:1 to real ARM64 ops but carry
/// higher-level label references instead of raw offsets.
#[derive(Debug, Clone)]
pub enum Arm64Instruction {
    // -- Arithmetic --
    Add {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    AddImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    /// `ADD Xd, Xn, Xm, LSL #shift` -- the scaled-index form (round 9 wave
    /// 13). `shift` is `log2(element size)`, 0..=3 for a Java array.
    AddLsl {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        shift: u8,
    },
    Sub {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SubImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    Mul {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SDiv {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Neg {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    Madd {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },
    Msub {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },

    // -- Logical --
    And {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Orr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Eor {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Lsl {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Lsr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Asr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },

    // -- Compare --
    Cmp {
        rn: Arm64Register,
        rm: Arm64Register,
    },
    CmpImm {
        rn: Arm64Register,
        imm: i32,
    },
    Tst {
        rn: Arm64Register,
        rm: Arm64Register,
    },

    // -- Move --
    Mov {
        rd: Arm64Register,
        rm: Arm64Register,
    },
    MovImm {
        rd: Arm64Register,
        imm: i64,
    },
    MovK {
        rd: Arm64Register,
        imm: u16,
        shift: u8,
    },

    // -- Load / Store --
    Ldr {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Zero-extending BYTE load. Distinct from [`Self::Ldr`] because the
    /// safepoint flag is a one-byte `AtomicBool` and the 64-bit form would
    /// fold the `GcBarrier` fields after it into the test.
    Ldrb {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Str {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Ldp {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Stp {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Pre-index STP with writeback: `STP rt1, rt2, [rn, #offset]!`.
    ///
    /// Bug-fix (ARM64 BUG #2): the AAPCS64-idiomatic prologue store. The base
    /// register `rn` is updated to `rn + offset` as part of the instruction,
    /// which both saves the pair AND allocates the (first 16 bytes of the)
    /// stack frame atomically. `offset` uses the small fixed −16, always inside
    /// the imm7 range, so it is robust for arbitrarily large frames (unlike a
    /// signed-offset STP at `frame_size-16`).
    StpPre {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Post-index LDP with writeback: `LDP rt1, rt2, [rn], #offset`.
    ///
    /// Bug-fix (ARM64 BUG #2): the matching epilogue restore. Loads the pair
    /// from `[rn]` then updates `rn = rn + offset`, mirroring [`StpPre`].
    LdpPost {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    LdrLiteral {
        rt: Arm64Register,
        label: u32,
    },

    // -- Branch --
    B {
        label: u32,
    },
    BCond {
        cond: Arm64Condition,
        label: u32,
    },
    Bl {
        label: u32,
    },
    Br {
        rn: Arm64Register,
    },
    Blr {
        rn: Arm64Register,
    },
    Ret,
    Cbz {
        rt: Arm64Register,
        label: u32,
    },
    Cbnz {
        rt: Arm64Register,
        label: u32,
    },

    // -- Conversion --
    ScvtfDouble {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    FcvtzsInt {
        rd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP move (bit-pattern transfer between GP and FP registers) --
    /// FMOV Vd, Xn — move GP register to FP register (bit-pattern, no conversion).
    FmovToFp {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// FMOV Xd, Vn — move FP register to GP register (bit-pattern, no conversion).
    FmovFromFp {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// FMOV Vd, Vn — move between FP registers.
    FmovFp {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP negate --
    FnegDouble {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP single-precision ops --
    FaddSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FsubSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FmulSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FdivSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FcmpSingle {
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FnegSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    /// SCVTF Sd, Wn — convert 32-bit int to single-precision float.
    ScvtfSingle {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// FCVTZS Wd, Sn — convert single-precision float to 32-bit int (truncate toward zero).
    FcvtzsSingle {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// FCVT Dd, Sn — convert single to double.
    FcvtSingleToDouble {
        vd: Arm64Register,
        vn: Arm64Register,
    },
    /// FCVT Sd, Dn — convert double to single.
    FcvtDoubleToSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    /// LDR (FP) — load from [base + offset] to FP register.
    FpLdr {
        vt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
        is_double: bool,
    },
    /// STR (FP) — store FP register to [base + offset].
    FpStr {
        vt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
        is_double: bool,
    },

    // -- NEON SIMD (double precision) --
    FaddDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FsubDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FmulDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FdivDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FcmpDouble {
        vn: Arm64Register,
        vm: Arm64Register,
    },

    // -- NEON SIMD (integer vector, 4x32) --
    /// LD1 {Vt.4S}, [Xn] — load 128-bit vector from memory.
    NeonLd1_4s {
        vt: Arm64Register,
        rn: Arm64Register,
    },
    /// ST1 {Vt.4S}, [Xn] — store 128-bit vector to memory.
    NeonSt1_4s {
        vt: Arm64Register,
        rn: Arm64Register,
    },
    /// ADD Vd.4S, Vn.4S, Vm.4S — vector integer add (4x i32).
    NeonAdd4s {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    /// MUL Vd.4S, Vn.4S, Vm.4S — vector integer multiply (4x i32).
    NeonMul4s {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },

    // -- 32-bit (W) integer forms --
    //
    // JVM `int` arithmetic. A W-form result is the JVMS 32-bit wrapped value;
    // the backend follows each producer with `Sxtw` to restore the
    // sign-extended form it keeps every `int` in.
    AddW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SubW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    MulW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    AndW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    OrrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    EorW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `LSLV Wd, Wn, Wm`: the amount is taken MOD 32, i.e. `ishl`'s `& 0x1f`.
    LslW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `LSRV Wd, Wn, Wm`, amount MOD 32.
    LsrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `ASRV Wd, Wn, Wm`, amount MOD 32.
    AsrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    NegW {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SDIV Wd, Wn, Wm` (`idiv`, round 9 wave 11). `INT_MIN / -1` is
    /// `INT_MIN`, as JVMS §6.5 `idiv` requires: AArch64 division never traps,
    /// and the lowering branches to the `ArithmeticException` stub on a zero
    /// divisor BEFORE this runs (a zero divisor would otherwise yield 0).
    SDivW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `MSUB Wd, Wn, Wm, Wa` = `Wa - Wn*Wm` (`irem`'s `a - (a/b)*b`).
    MsubW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },
    AddImmW {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    SubImmW {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    CmpW {
        rn: Arm64Register,
        rm: Arm64Register,
    },
    CmpImmW {
        rn: Arm64Register,
        imm: i32,
    },
    CbzW {
        rt: Arm64Register,
        label: u32,
    },
    CbnzW {
        rt: Arm64Register,
        label: u32,
    },
    /// `SXTW Xd, Wn`.
    Sxtw {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SXTH Xd, Wn`.
    Sxth {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SXTB Xd, Wn`.
    Sxtb {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `AND Xd, Xn, #imm`, as a bitmask immediate when encodable and through
    /// IP0 otherwise.
    AndImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: u64,
    },
    /// `CSET Xd, cond`: 1 when `cond` holds, else 0.
    Cset {
        rd: Arm64Register,
        cond: Arm64Condition,
    },
    /// `CNEG Xd, Xn, cond`: `-Xn` when `cond` holds, else `Xn`.
    Cneg {
        rd: Arm64Register,
        rn: Arm64Register,
        cond: Arm64Condition,
    },

    // -- FP width forms --
    /// `FMOV Sd, Wn` (bit pattern, no conversion).
    FmovToFpSingle {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `FMOV Wd, Sn` (bit pattern; zero-extends into Xd).
    FmovFromFpSingle {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// `FMOV Sd, Sn`.
    FmovFpSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },
    /// `SCVTF Dd, Wn` (`i2d`).
    ScvtfDoubleW {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SCVTF Sd, Xn` (`l2f`).
    ScvtfSingleX {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `FCVTZS Wd, Dn` (`d2i`, saturating at the 32-bit bounds).
    FcvtzsIntW {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// `FCVTZS Xd, Sn` (`f2l`).
    FcvtzsSingleX {
        rd: Arm64Register,
        vn: Arm64Register,
    },

    // -- Switches --
    /// `tableswitch`: `key - low` into IP1 (X17), an unsigned bounds check to
    /// `default`, then a PC-relative jump table (`ADR X16; LDRSW X17, [X16,
    /// W17, UXTW #2]; ADD X16, X16, X17; BR X16`) followed by one 32-bit
    /// offset per case. X16/X17 are never allocator-managed, so no case can
    /// land on the key's own register -- which the old compare chain, whose
    /// constants came from the scratch allocator, could (`CMP R, R`).
    TableSwitch {
        key: Arm64Register,
        low: i32,
        default: u32,
        targets: Vec<u32>,
    },
    /// `lookupswitch`: `CMP Wkey, #value` (constants too wide for an
    /// immediate go through IP0, never a scratch register), `B.EQ` per pair,
    /// then `B default`.
    LookupSwitch {
        key: Arm64Register,
        pairs: Vec<(i32, u32)>,
        default: u32,
    },

    // -- Shared-memory access (round 9 wave 10) --
    //
    // The heap/statics accesses, as distinct from the frame-slot `Ldr`/`Str`
    // above: the address is exactly `[rn]` (the ordered forms take no offset,
    // so every lowering computes the full address first) and the width and
    // the ordering are explicit, so a reviewer can see from the pseudo-op
    // alone whether a `volatile` access got its acquire/release form.
    /// `LDR{B,H} Wt` / `LDR Wt` / `LDR Xt`, or with `acquire` the matching
    /// `LDAR{B,H}` / `LDAR Wt` / `LDAR Xt`, from `[rn]`. Every form
    /// ZERO-extends into `rt`; a signed or FP result is the lowering's
    /// follow-up instruction's job (`Sxtw`, `FmovToFp*`).
    MemLoad {
        rt: Arm64Register,
        rn: Arm64Register,
        width: Arm64MemWidth,
        acquire: bool,
    },
    /// `STR{B,H} Wt` / `STR Wt` / `STR Xt`, or with `release` the matching
    /// `STLR{B,H}` / `STLR Wt` / `STLR Xt`, to `[rn]`. A `volatile` store owes
    /// a StoreLoad edge on top of the release; see [`Self::DmbIsh`].
    MemStore {
        rt: Arm64Register,
        rn: Arm64Register,
        width: Arm64MemWidth,
        release: bool,
    },
    /// `DMB ISH`: the full inner-shareable barrier (the aarch64 counterpart of
    /// x64's `MFENCE` after a `volatile` store).
    DmbIsh,

    // -- System --
    Nop,
    Brk {
        imm: u16,
    },

    // -- Pseudo-instructions --
    Label(u32),
    Comment(String),
    /// Raw 64-bit constant data embedded in the instruction stream (literal pool).
    /// The label is bound to the position of this data so that LdrLiteral can
    /// reference it.
    ConstantPoolEntry {
        label: u32,
        value: u64,
    },
}

impl Arm64Instruction {
    /// An upper bound on the bytes this pseudo-op encodes to.
    ///
    /// `Label` and `Comment` emit nothing, a literal emits 8 bytes, immediates
    /// and far frame accesses expand into a materialization through IP0, and
    /// a switch carries its own table. See `emit_machine_code_inner`.
    pub fn max_encoded_bytes(&self) -> usize {
        match self {
            Arm64Instruction::Label(_) | Arm64Instruction::Comment(_) => 0,
            Arm64Instruction::ConstantPoolEntry { .. } => 8,
            Arm64Instruction::MovImm { .. } => 16,
            Arm64Instruction::AddImm { .. }
            | Arm64Instruction::SubImm { .. }
            | Arm64Instruction::AddImmW { .. }
            | Arm64Instruction::SubImmW { .. }
            | Arm64Instruction::CmpImm { .. }
            | Arm64Instruction::CmpImmW { .. }
            | Arm64Instruction::AndImm { .. } => 20,
            Arm64Instruction::Ldr { .. }
            | Arm64Instruction::Str { .. }
            | Arm64Instruction::FpLdr { .. }
            | Arm64Instruction::FpStr { .. } => 24,
            // SUB + CMP (each up to 20 through IP0), B.HI, ADR, LDRSW, ADD, BR,
            // and a word per case.
            Arm64Instruction::TableSwitch { targets, .. } => 60 + 4 * targets.len(),
            // A CMP (up to 20) and a B.EQ per pair, then B.
            Arm64Instruction::LookupSwitch { pairs, .. } => 24 * pairs.len() + 4,
            _ => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// Arm64EntryConvention
// ---------------------------------------------------------------------------

/// The convention a compiled method is ENTERED with on aarch64 — which is
/// AAPCS64 restricted to integer registers, and not AAPCS64 (finding A9).
///
/// It was called `Arm64CallingConvention` and documented as "AAPCS64 calling
/// convention constants and helpers", and it is not that. AAPCS64 §6.4 assigns
/// the first eight floating-point and SIMD arguments to `V0`–`V7`; this type
/// names no FP argument registers at all, and `emit_argument_homing` puts
/// argument `i` in `INT_ARG_REGS[i]` whatever its Java type — a `float` arrives
/// as its IEEE-754 single bit pattern in an X register and a `double` as its
/// bits. Anything written against the old name and the old doc would have
/// passed a `double` in `D0` and this prologue would never have read it.
///
/// That is sound because nothing outside this VM calls a compiled method: the
/// interpreter's entry trampoline is the only producer of these arguments and
/// it marshals every one of them into an integer slot. It is a PRIVATE
/// convention that happens to agree with AAPCS64 wherever it says anything.
///
/// # What here really is AAPCS64
///
/// * [`Self::CALLEE_SAVED`] — X19–X28, and `STACK_ALIGNMENT` = 16, because the
///   compiled frame must be unwindable and re-entrant from ordinary C code (the
///   safepoint slow path is a `BLR` into the runtime).
/// * [`Self::LINK_REG`] / [`Self::FRAME_POINTER`] — X30 / X29, and the frame
///   record `Arm64FrameLayout` builds around them.
/// * [`Self::RED_ZONE`] = 0: AAPCS64 defines none, unlike System V on x86-64.
///
/// # What is NOT
///
/// * no FP/SIMD argument registers, as above;
/// * no stack argument area — `emit_argument_homing` refuses a ninth argument
///   outright rather than reading it from the caller's frame;
/// * D8–D15 are callee-saved under AAPCS64 and this prologue does not save
///   them, which is exactly why `regalloc::ARM64_LOCAL_FPS` must not be used to
///   home a float local here (see the note at the FP-local seeding loop).
///
/// Asserted by `the_entry_convention_passes_every_argument_in_an_x_register`.
pub struct Arm64EntryConvention;

impl Arm64EntryConvention {
    /// Argument registers X0-X7 — for EVERY argument, not only integers. See
    /// the type's doc: a `float`/`double` argument arrives here as its bit
    /// pattern, because this VM's entry trampoline marshals it that way.
    ///
    /// The name is kept for its call sites and because the registers really are
    /// AAPCS64's integer argument registers; what is not AAPCS64 is that they
    /// are the only ones.
    pub const INT_ARG_REGS: &'static [Arm64Register] = &[
        Arm64Register::X0,
        Arm64Register::X1,
        Arm64Register::X2,
        Arm64Register::X3,
        Arm64Register::X4,
        Arm64Register::X5,
        Arm64Register::X6,
        Arm64Register::X7,
    ];

    /// Callee-saved registers: X19-X28.
    pub const CALLEE_SAVED: &'static [Arm64Register] = &[
        Arm64Register::X19,
        Arm64Register::X20,
        Arm64Register::X21,
        Arm64Register::X22,
        Arm64Register::X23,
        Arm64Register::X24,
        Arm64Register::X25,
        Arm64Register::X26,
        Arm64Register::X27,
        Arm64Register::X28,
    ];

    pub const RETURN_REG: Arm64Register = Arm64Register::X0;
    pub const FRAME_POINTER: Arm64Register = Arm64Register::FP;
    pub const LINK_REG: Arm64Register = Arm64Register::LR;
    pub const STACK_POINTER: Arm64Register = Arm64Register::SP;

    /// Stack alignment requirement (16 bytes on ARM64).
    pub const STACK_ALIGNMENT: usize = 16;

    /// Red-zone size (ARM64 AAPCS64 does not define a red zone).
    pub const RED_ZONE: usize = 0;

    /// Get the register for the n-th integer argument, if available.
    pub fn int_arg_reg(n: usize) -> Option<Arm64Register> {
        Self::INT_ARG_REGS.get(n).copied()
    }

    /// Map the n-th local variable to a callee-saved register, if available.
    pub fn local_reg(n: usize) -> Option<Arm64Register> {
        Self::CALLEE_SAVED.get(n).copied()
    }
}

// ---------------------------------------------------------------------------
// Arm64FrameLayout
// ---------------------------------------------------------------------------

/// Describes the stack frame geometry for a compiled method.
pub struct Arm64FrameLayout {
    /// Total frame size in bytes (16-byte aligned).
    pub frame_size: i32,
    /// Byte offset from FP where callee-saved registers are stored.
    pub callee_save_offset: i32,
    /// Byte offset from FP where spill slots begin.
    pub spill_offset: i32,
    /// Number of spill slots.
    pub num_spills: usize,
    /// Which callee-saved registers must be preserved.
    pub saved_regs: Vec<Arm64Register>,
    /// How many locals got a dedicated register.
    pub num_reg_locals: usize,
}

impl Arm64FrameLayout {
    /// Compute the frame layout given method metadata.
    ///
    /// ```text
    /// [FP + 8]     saved LR          \  the AAPCS64 frame record, pushed by
    /// [FP]         saved caller FP   /  `STP X29, X30, [SP, #-16]!`
    /// [FP - 8] ..  callee-saved GPRs   (callee_save_offset = -callee_save_bytes)
    /// ..           spill area          (spill_offset = callee_save_offset - spill_bytes)
    /// [SP]         frame bottom        (FP - (frame_size - 16))
    /// ```
    ///
    /// `frame_size` counts the 16-byte record as well, and is 16-byte aligned.
    /// Everything this frame owns is BELOW FP. The record used to sit at
    /// `[FP-16]`/`[FP-8]` with FP equal to the caller's SP, which is not the
    /// frame record any unwinder or frame-pointer walk expects.
    pub fn compute(_num_locals: usize, num_spills: usize, saved_regs: &[Arm64Register]) -> Self {
        let num_reg_locals = saved_regs.len();

        // Callee-saved regs: round count up to even for STP pairing.
        let num_saved = saved_regs.len();
        let callee_save_bytes = ((num_saved + 1) / 2) * 16; // pairs of 8-byte regs

        let spill_bytes = num_spills * 8;

        // Total = frame record (16) + callee-save area + spill area, aligned.
        let raw = 16 + callee_save_bytes + spill_bytes;
        let frame_size = align_up(raw, 16) as i32;

        // Offsets are negative from FP, which points at the frame record.
        let callee_save_offset = -(callee_save_bytes as i32);
        let spill_offset = callee_save_offset - spill_bytes as i32;

        Self {
            frame_size,
            callee_save_offset,
            spill_offset,
            num_spills,
            saved_regs: saved_regs.to_vec(),
            num_reg_locals: num_reg_locals,
        }
    }
}

/// Round `value` up to the next multiple of `align`.
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

// ---------------------------------------------------------------------------
// Arm64SpillArea
// ---------------------------------------------------------------------------

/// How the spill area below [`Arm64FrameLayout::spill_offset`] is partitioned,
/// named once instead of four times (finding A11).
///
/// The area holds four regions, laid end to end from `spill_offset` upwards:
///
/// ```text
/// word 0                       .. locals                  frame-homed locals
/// locals                       .. locals+operands          operand stack
/// locals+operands              .. +safepoint_homes         safepoint homes
/// locals+operands+homes        .. +sp_id                   the safepoint-id word
/// ...+sp_id                    .. +context                 the VM context word
/// ```
///
/// # Why this is a type and not four additions
///
/// Before it was, each of the four accessors re-derived its own base by adding
/// up the sizes of the regions before it —
/// `local_spill_count() + max_stack + k` in `safepoint_home_for_reg_local`,
/// `local_spill_count() + depth` in `spill_offset_for_depth`,
/// `num_spills - 1` for the id word — and the total was restated a fifth time
/// where the frame was sized. Five expressions that must agree, with nothing
/// checking that they do, and every one of them bounds-checked only against the
/// GRAND total (`slot >= frame.num_spills`). An accessor that took the wrong
/// base therefore did not refuse: it read or wrote a word belonging to a
/// different region, which for the operand-versus-locals pair means a local
/// silently aliasing an operand.
///
/// The region bases are computed once here, each accessor is bounds-checked
/// against ITS OWN region, and `total()` is derived rather than restated.
///
/// # Why the counts are captured, not re-read
///
/// `local_spill_count()` walks `local_regs` on every call. The frame is sized
/// from one reading of it and then indexed against later readings; they agree
/// today only because nothing edits `local_regs` after the prologue. Capturing
/// the four counts at the moment the frame is sized makes that an invariant of
/// the type rather than an ordering property of the compile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Arm64SpillArea {
    /// Words held by locals with no register home.
    locals: usize,
    /// Words held by the operand stack — `max_stack`, one word per JVM slot.
    operands: usize,
    /// One word per register-homed local, so a safepoint can publish it to the
    /// collector. Zero when this compilation does not poll.
    safepoint_homes: usize,
    /// The word each poll stamps its safepoint id into: 1, or 0 when this
    /// compilation does not poll.
    sp_id: usize,
    /// The word the prologue homes the VM context pointer into: 1 when this
    /// compilation needs the context ABI (round 9 wave 16), else 0.
    ///
    /// LAST, so that adding it moved no existing word. Everything below it is
    /// addressed by an index this type computes, so the position is not load-
    /// bearing -- but a reader comparing two frame dumps across the change
    /// should not have to check that.
    context: usize,
}

impl Arm64SpillArea {
    /// Total words, which is what [`Arm64FrameLayout::compute`] is sized from.
    fn total(&self) -> usize {
        self.locals + self.operands + self.safepoint_homes + self.sp_id + self.context
    }

    /// Word index of frame-homed local slot `i`, or `None` when it is outside
    /// the locals region.
    fn local_word(&self, i: usize) -> Option<usize> {
        (i < self.locals).then_some(i)
    }

    /// Word index of operand-stack depth `depth`, or `None` when it is outside
    /// the operand region.
    ///
    /// Bounded by `operands` and not by the total: a depth at or past
    /// `max_stack` would otherwise land on a safepoint home.
    fn operand_word(&self, depth: usize) -> Option<usize> {
        (depth < self.operands).then_some(self.locals + depth)
    }

    /// Word index of the safepoint home for the `k`-th register-homed local,
    /// or `None` when it is outside the home region.
    fn safepoint_home_word(&self, k: usize) -> Option<usize> {
        (k < self.safepoint_homes).then_some(self.locals + self.operands + k)
    }

    /// Word index of the safepoint-id word, or `None` when none is reserved.
    fn sp_id_word(&self) -> Option<usize> {
        (self.sp_id != 0).then_some(self.locals + self.operands + self.safepoint_homes)
    }

    /// Word index of the VM context word, or `None` when this compilation does
    /// not take a context.
    fn context_word(&self) -> Option<usize> {
        (self.context != 0)
            .then_some(self.locals + self.operands + self.safepoint_homes + self.sp_id)
    }
}

// ---------------------------------------------------------------------------
// Arm64CodeBuffer
// ---------------------------------------------------------------------------

/// Accumulates `Arm64Instruction`s and manages labels.
pub struct Arm64CodeBuffer {
    instructions: Vec<Arm64Instruction>,
    labels: HashMap<u32, usize>,
    next_label: u32,
}

impl Arm64CodeBuffer {
    pub fn new() -> Self {
        Self {
            instructions: Vec::new(),
            labels: HashMap::new(),
            next_label: 0,
        }
    }

    /// Append an instruction.
    pub fn emit(&mut self, inst: Arm64Instruction) {
        self.instructions.push(inst);
    }

    /// Allocate a fresh label id.
    pub fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Bind `label` to the current instruction index and emit a `Label` pseudo-op.
    pub fn bind_label(&mut self, label: u32) {
        self.labels.insert(label, self.instructions.len());
        self.instructions.push(Arm64Instruction::Label(label));
    }

    pub fn instruction_count(&self) -> usize {
        self.instructions.len()
    }

    pub fn instructions(&self) -> &[Arm64Instruction] {
        &self.instructions
    }

    /// An UPPER BOUND on the encoded size, in bytes.
    ///
    /// Not `len * 4`: this is a pseudo-op stream, in which a `Label` or
    /// `Comment` encodes to nothing and a `MovImm`, a wide immediate, a far
    /// frame access or a switch expands to several words. See
    /// [`Arm64Instruction::max_encoded_bytes`].
    pub fn estimated_size(&self) -> usize {
        self.instructions
            .iter()
            .map(Arm64Instruction::max_encoded_bytes)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Arm64CompileResult
// ---------------------------------------------------------------------------

/// Is the aarch64 JIT on at all (`CRATONVM_JIT_ARM64`, **default-OFF**)?
///
/// `docs/PLATFORMS.md` says the JIT is disabled off x86-64, and until this
/// switch existed that was not true on aarch64: `try_compile_inner` compiled
/// with this backend unconditionally, publishing machine code that no host in
/// this repository had executed. It stays opt-in until the backend has run on
/// hardware. Read by the `#[cfg(target_arch = "aarch64")]` block of
/// `try_compile_inner`, and defined here without a `cfg` so every host
/// type-checks it.
pub fn arm64_jit_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_ARM64")
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

/// Does the aarch64 backend emit GC safepoint polls
/// (`CRATONVM_JIT_ARM64_SAFEPOINTS`, **default-OFF; opt-in**)?
///
/// # Why this one is opt-in when every sibling switch is default-on
///
/// It emits MACHINE CODE FOR AN ARCHITECTURE NO CI RUNNER OR DEVELOPER HOST
/// HERE CAN EXECUTE. Every other codegen switch in this workspace ships
/// default-on with a kill switch because a regression run can execute it and
/// say so; this one cannot be run at all until someone builds on an aarch64
/// host. The encodings below are asserted against known-good instruction words
/// and the structure is asserted against the emitted pseudo-op stream, which is
/// everything a non-aarch64 host can honestly prove -- and it is not the same
/// as "it works". Default-on would be publishing an unexecuted calling
/// sequence into a GC's stop-the-world protocol.
///
/// Turning it on does two things: a poll at method entry and at every loop
/// header, and -- because that is what the refusal was FOR -- it lifts
/// `label_for_pc`'s blanket refusal of backward branches, so loops compile
/// again. With it off, this backend is byte-identical to before.
pub(crate) fn arm64_safepoints_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_ARM64_SAFEPOINTS")
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

/// A safepoint's oop map as the COMPILER can know it: the frame slots are
/// final, but the PC is a PSEUDO-OP INDEX, not a byte offset.
///
/// The two cannot be the same value on this backend and that is the whole
/// reason this type exists. `Arm64Instruction` is a pseudo-op stream, not a
/// fixed-width one: `Label` and `Comment` emit nothing, `ConstantPoolEntry`
/// emits 8 bytes, and `MovImm` / `AddImm` / `CmpImm` / out-of-range `Ldr`/`Str`
/// expand to one to four words (`mov_imm64`, `emit_addsub_imm_safe`,
/// `emit_addr_into_ip0`). So the `instruction_count * 4` the writer used to
/// record was wrong for any method containing one of those, and a map keyed by
/// a wrong PC is worse than no map -- the GC reads the WRONG FRAME SLOTS at a
/// real safepoint and either misses a live reference or rewrites a primitive.
///
/// Keeping the unresolved form in its own type means an unresolved PC cannot be
/// mistaken for a resolved one by a later reader: there is no `OopMapEntry`
/// anywhere until [`emit_machine_code_with_oop_maps`] has run the encoder and
/// can say what the byte offset actually is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arm64PendingOopMap {
    /// Index into [`Arm64CompileResult::instructions`] of the pseudo-op that
    /// FOLLOWS the safepoint -- the same "return address" convention the x64
    /// backend uses for `OopMapEntry::native_pc_offset`.
    pub pseudo_index: u32,
    /// Frame-slot offsets (relative to FP) holding object references here.
    pub frame_slot_offsets: Vec<i16>,
    /// The safepoint id this map belongs to -- the value the frame's sp-id slot
    /// holds while this safepoint is the active one. Becomes
    /// `OopMapEntry::bytecode_pc`, which is what
    /// `find_oop_map_for_safepoint_id` matches on.
    pub safepoint_id: u32,
}

/// Output of the compilation pipeline.
pub struct Arm64CompileResult {
    pub instructions: Vec<Arm64Instruction>,
    pub frame: Arm64FrameLayout,
    pub labels: HashMap<u32, usize>,
    pub success: bool,
    /// T1.1.3 — this compilation's safepoint oop maps, PC-UNRESOLVED.
    ///
    /// Each entry records the frame-slot offsets (relative to FP on AArch64;
    /// x86-64 uses RBP) that hold object references at a GC-capable safepoint,
    /// keyed by pseudo-op index. [`emit_machine_code_with_oop_maps`] turns
    /// these into `crate::OopMapEntry` values keyed by real byte offsets --
    /// see [`Arm64PendingOopMap`] for why the compiler cannot do that itself.
    ///
    /// Empty unless `CRATONVM_JIT_ARM64_SAFEPOINTS` is on: the safepoint polls
    /// are this backend's only GC-capable points (it lowers no allocation, call
    /// or monitor), and each poll records exactly one map.
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
    /// Frame offset (positive) of the safepoint-id slot, or 0 when none was
    /// reserved. Published onto `CompiledMethod::sp_id_slot_off`, which the
    /// runtime reads as `[frame_base - off]`.
    pub sp_id_slot_off: i32,
    /// How many safepoints this compilation published a map for, and how many
    /// of those could not describe everything live at their site. The terms
    /// `fully_oop_covered` is computed from -- see `publish_compiled_method`.
    pub safepoint_count: usize,
    pub incomplete_oop_maps: usize,
    /// Declaring class ids of the `getstatic` sites this body reads directly
    /// (round 9 wave 10), sorted and deduplicated. Published as
    /// `CompiledMethod::static_init_classes`, the ensure-initialized
    /// obligation the x64 tier records for the same reason.
    pub static_init_classes: Vec<u32>,
    /// Whether this body takes the VM context pointer as an extra LEADING
    /// argument (round 9 wave 16). Published by
    /// [`publish_compiled_method`] as `CompiledMethod::needs_context`, which
    /// is what makes the VM call the context entry.
    ///
    /// It is not an extra argument the body may ignore: the context OCCUPIES
    /// X0, so a body compiled without one and called with one reads its first
    /// Java argument from the context pointer. The flag and the prologue must
    /// agree, which is why both come from the same field.
    pub needs_context: bool,
    /// Whether this compilation emitted safepoint POLLS (round 9 wave 18).
    ///
    /// It NO LONGER gates `fully_oop_covered` -- round 9 wave 21 widened that
    /// claim to an allocation-only method, see [`publish_compiled_method`].
    /// Kept because it is the one bit that distinguishes the two ways a frame
    /// here becomes safepoint-shaped, and a test that means to exercise the
    /// allocation route has to be able to say which one it got.
    pub polls_enabled: bool,
    /// Whether the prologue published this frame's BASE, through
    /// `helpers.frame_record` (round 9 wave 21).
    ///
    /// The fifth term of `fully_oop_covered`, and the one whose absence is
    /// silent. Everything precise this backend emits is addressed from the
    /// frame base: the runtime reads the safepoint id at
    /// `[frame_base - sp_id_slot_off]` and each map's slots at
    /// `[frame_base - offset]`, and it learns `frame_base` only from
    /// [`Arm64Backend::emit_frame_record`]. A compilation that emits maps but
    /// no frame record leaves `PreciseFrameInfo::exact_rbp` at `0`, which
    /// every precise path declines -- so its maps exist and cannot be read,
    /// and a coverage claim over them is a claim about nothing.
    ///
    /// It is a separate term rather than an assertion because the emitter can
    /// decline for a reason that is not this compilation's fault:
    /// `helpers.frame_record` is `0` when the VM was started with
    /// `CRATONVM_NO_PRECISE_JIT_MAPS=1` and no moving young generation. The
    /// method still compiles and still runs; it just may not claim coverage.
    pub frame_base_published: bool,
}

// ---------------------------------------------------------------------------
// Arm64Backend
// ---------------------------------------------------------------------------

/// The kind of value an operand-stack entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandKind {
    /// JVM `int` (and `boolean`/`byte`/`char`/`short`), held SIGN-EXTENDED in
    /// its 64-bit register. See "The `int` representation" in the module
    /// header.
    I32,
    /// JVM `long`.
    I64,
    /// An object reference.
    Ref,
    /// JVM `float`: an S register, or the low four bytes of a frame word.
    F32,
    /// JVM `double`: a D register, or a whole frame word.
    F64,
}

impl OperandKind {
    /// Lives in V0-V7 rather than X9-X15.
    pub fn is_fp(self) -> bool {
        matches!(self, OperandKind::F32 | OperandKind::F64)
    }

    /// Category 2 (JVMS 2.11.1): one entry here, two JVM stack slots.
    pub fn is_category2(self) -> bool {
        matches!(self, OperandKind::I64 | OperandKind::F64)
    }
}

/// Where an operand-stack entry is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandLoc {
    /// A scratch register: X9-X15 for integer kinds, V0-V7 for FP kinds.
    Reg(Arm64Register),
    /// The frame word reserved for the entry's DEPTH, as an FP-relative
    /// offset (see [`Arm64Backend::spill_offset_for_depth`]).
    Slot(i32),
}

/// One simulated operand-stack entry: what it is, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operand {
    pub kind: OperandKind,
    pub loc: OperandLoc,
    /// Holds an object reference the GC must see. Set for every `Ref` entry --
    /// `aconst_null`, `aload*`, `aaload`, `new`/`anewarray`, a
    /// reference-returning `invoke*`, a reference `getfield` and a reference
    /// `getstatic` all produce one -- and carried by the stack shuffles, so
    /// the marks are exact by construction.
    pub oop: bool,
}

impl Operand {
    /// An entry that lives in `reg`.
    pub fn in_reg(kind: OperandKind, reg: Arm64Register) -> Self {
        Self {
            kind,
            loc: OperandLoc::Reg(reg),
            oop: kind == OperandKind::Ref,
        }
    }
}

/// Kinds of the `<x>load`/`<x>store` families, in opcode order: `i l f d a`.
const LOCAL_KINDS: [OperandKind; 5] = [
    OperandKind::I32,
    OperandKind::I64,
    OperandKind::F32,
    OperandKind::F64,
    OperandKind::Ref,
];

/// Conditions of `ifeq..ifle` and `if_icmpeq..if_icmple`, in opcode order.
const IF_CONDS: [Arm64Condition; 6] = [
    Arm64Condition::Eq,
    Arm64Condition::Ne,
    Arm64Condition::Lt,
    Arm64Condition::Ge,
    Arm64Condition::Gt,
    Arm64Condition::Le,
];

/// The bytecode byte at `at`, or `None` past the end.
fn bc_u8(code: &[u8], at: usize) -> Option<u8> {
    code.get(at).copied()
}

/// The big-endian `i16` at `at`, or `None` if it runs past the end.
fn bc_i16(code: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
}

/// The big-endian `i32` at `at`, or `None` if it runs past the end.
fn bc_i32(code: &[u8], at: usize) -> Option<i32> {
    let b = code.get(at..at.checked_add(4)?)?;
    Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The bytecode pc a branch at `start_pc` with displacement `offset` targets.
///
/// Cast: a target before pc 0 wraps to a huge pc. No label is ever bound
/// there, so `emit_machine_code` refuses the method -- the same outcome as any
/// other branch into the middle of nowhere.
fn branch_target(start_pc: usize, offset: i32) -> usize {
    (start_pc as i64 + i64::from(offset)) as usize
}

// ── Shared memory and the ordering instructions this backend does not have ──

/// Does this backend's PRODUCTION code emit the acquire/release instructions
/// a `volatile` access lowers to?
///
/// **`true` since round 9 wave 10**, and like its sibling below this is a
/// statement about the code rather than a switch: the
/// [`Arm64Instruction::MemLoad`] / [`Arm64Instruction::MemStore`] encoding
/// arms call `aarch64.rs`'s `ldar*` / `stlr*` encoders, and `getstatic`'s
/// lowering reaches them with `acquire: true` for a `volatile` static.
/// [`the_ordering_constants_agree_with_the_code`] checks it against the file.
///
/// # What replaced the single `ARM64_CAN_ORDER_MEMORY` (round 9 wave 14)
///
/// There used to be ONE constant, `ARM64_CAN_ORDER_MEMORY`, and the gate in
/// `compile_pass` read it as a blanket permission: while it was `false` every
/// shared-memory opcode was refused, and flipping it to `true` would un-gate
/// all of them at once. Its own documentation said it could flip only once
/// BOTH the acquire/release and the exclusive-access halves had production
/// call sites -- i.e. once `monitorenter` lowered a lock-word CAS.
///
/// That coupling turned out to be wrong in both directions.
///
/// * It was too WEAK at the top: a single `bool` is exactly the kind of thing
///   a later change flips to unblock itself, and flipping it would have
///   allowed a `getfield` arm with a plain `LDR` -- the precise accident the
///   gate exists to prevent.
/// * It was too STRONG at the bottom: the exclusive half cannot arrive the
///   way it was expected to. Every `monitorenter` javac emits comes from a
///   `synchronized` block, and a `synchronized` block always carries the
///   compiler-generated `any -> monitorexit; athrow` handler, so the method's
///   exception table is NOT empty -- and every direct-throw lowering on this
///   backend required an empty one, because the interpreter's drain threw
///   with an unknown pc. A monitor lowering would therefore have been dead
///   code, and the constant it was supposed to unlock would have stayed
///   `false` forever while the acquire/release half it also governs had been
///   real since wave 10. (h23, 2026-09-22: the exception-table restriction is
///   gone -- `emit_stamp_throw_bci` -- and `monitorenter` IS lowered now. It
///   still does not flip the exclusive constant, because the CAS is the
///   helper's; see [`ARM64_LOWERS_EXCLUSIVE_ACCESS`]. The coupling was wrong
///   on its own terms either way.)
///
/// So the blanket permission is gone. The gate now consults ONLY
/// [`opcode_has_ordered_lowering`], a per-opcode list each entry of which is a
/// reviewed claim about ONE lowering. There is no longer any single edit that
/// opens the gate for an opcode nobody has written an ordered lowering for,
/// which is strictly stronger than what the boolean gave. These two constants
/// remain because the QUESTION they answer is still worth answering -- "which
/// ordering instructions does this backend actually emit?" -- and because
/// `docs/jit/aarch64-parity.md` §2.2's capability table is checked against
/// them.
pub(crate) const ARM64_LOWERS_ACQUIRE_RELEASE: bool = true;

/// Does this backend's PRODUCTION code emit the exclusive-access instructions
/// (`LDAXR`/`STLXR`, or the LSE `CASAL`)?
///
/// **`false`, and for a different reason since h23 (2026-09-22).**
///
/// The encoders exist (round 9 wave 9) and nothing calls them. Until h23 the
/// reason was that `monitorenter` could not be reached at all: a
/// `synchronized` block's javac-generated `any -> monitorexit; athrow`
/// handler makes the method's exception table non-empty, and every trapping
/// lowering here required an empty one. Both halves of that have since
/// changed -- `emit_stamp_throw_bci` removed the exception-table restriction,
/// and `emit_monitor_op` lowers both opcodes.
///
/// It stays `false` because that lowering calls `jit_monitor_enter` /
/// `jit_monitor_exit` and the compare-and-swap happens INSIDE the helper,
/// which is what `ir_lower`'s arm does on x64 too. An inline uncontended
/// thin-lock CAS with the helper as its fallback is the thing that would flip
/// this, and it is an open performance item, not a capability gap.
///
/// `Unsafe`'s atomics are the other caller. They arrive through an `invoke*`,
/// which this backend lowers as of round 9 wave 22 -- but through
/// `jit_invoke_dispatch`, so the exclusive access is the VM's, not this
/// backend's.
pub(crate) const ARM64_LOWERS_EXCLUSIVE_ACCESS: bool = false;

/// Does executing `opcode` read or write memory another thread can observe?
///
/// The single predicate every shared-memory lowering on this backend must
/// consult. It is deliberately an ALLOW-LIST OF THE HAZARD rather than of the
/// safe ops: an opcode nobody has classified is not in it, so the failure
/// direction for a *new* opcode is "allowed through", which is why the gate is
/// paired with the module header's coverage test rather than trusted alone.
///
/// Membership follows the JVM's own notion of the heap, not the narrower one of
/// "an instruction with a memory operand":
///
/// * array loads and stores, and `arraylength` — the array header and its
///   elements live in the shared heap;
/// * every field accessor, static or instance — the `volatile` and `final`
///   cases are the whole reason this predicate exists;
/// * every `invoke*` — a callee may do any of the above, so a barrier the
///   caller owes cannot be deferred past the call;
/// * every allocation — publication of a new object's header and its `final`
///   fields is the JMM's freeze action;
/// * `monitorenter` / `monitorexit` — acquire and release by definition;
/// * `athrow` — it transfers to a handler that may observe anything the
///   throwing frame published;
/// * `checkcast` / `instanceof` — both read the object header and may run a
///   class initialiser.
pub(crate) fn opcode_touches_shared_memory(opcode: u8) -> bool {
    matches!(
        opcode,
        0x2e..=0x35        // iaload .. saload
        | 0x4f..=0x56      // iastore .. sastore
        | 0xb2..=0xba      // getstatic, putstatic, getfield, putfield, invoke*
        | 0xbb..=0xbe      // new, newarray, anewarray, arraylength
        | 0xbf             // athrow
        | 0xc0 | 0xc1      // checkcast, instanceof
        | 0xc2 | 0xc3      // monitorenter, monitorexit
        | 0xc5 // multianewarray
    )
}

/// The shared-memory opcodes whose aarch64 lowering is COMPLETE -- **the whole
/// gate**, since round 9 wave 14 removed the blanket constant that used to sit
/// in front of it (see [`ARM64_LOWERS_ACQUIRE_RELEASE`]).
///
/// Every entry is a reviewed claim about ONE lowering: that it orders every
/// access it makes, and that it refuses -- in its own arm, with a named
/// reason -- every site it cannot settle. Adding an opcode here is the ONLY
/// way to let it past the gate, and doing so without the lowering fails
/// [`every_shared_memory_opcode_without_an_ordered_lowering_is_refused`],
/// which requires each listed opcode to name the refusal its own arm emits
/// when nothing is wired for it.
///
/// * `getstatic` (`0xb2`, round 9 wave 10), for a site the caller resolved
///   into an [`Arm64StaticField`]: a primitive static of an
///   already-initialized class, read as `[[base_cell] + cell]`, with `LDAR`
///   for a `volatile` field (the JMM acquire; ARMv8's `STLR`->`LDAR` is RCsc,
///   so no trailing `DMB` is owed by a READ) and a plain `LDR` otherwise. No
///   null check (a static has no receiver), no class-init check (the resolver
///   answers only for initialized classes), no call, no safepoint. An
///   unresolved site, a reference-typed static or a missing table refuses in
///   the `0xb2` arm.
/// * `arraylength` (`0xbe`, round 9 wave 12): a null check against the
///   per-action NPE stub ([`Arm64Backend::emit_npe_throw_stubs`]) and a plain
///   32-bit load of the header's length word.
/// * the array element accesses (`0x2e..=0x35`, `0x4f..=0x56`, round 9 wave
///   13) other than `aaload`/`aastore`, which have arms but always refuse: a
///   null check, an unsigned bounds check against the length word, a scaled
///   address and one plain access of the element's own width, plus
///   `bastore`'s runtime `boolean[]` mask.
///
/// The plain loads and stores in the last two are the ORDERED lowering, not an
/// omission. An array's length is written once before publication and its
/// elements are ordinary memory the JMM owes nothing about; and every address
/// in both is derived from the array reference, so ARMv8's address-dependency
/// rule (ARM ARM §B2.3.2, "dependency-ordered before") already orders the
/// access after the load that produced the reference. That is the guarantee
/// the JMM freeze action needs, and it is why no `LDAR` and no `DMB ISHLD` is
/// owed -- not because x64 emits none.
///
/// NOT listed, and why. Since wave 13 the reason is the SAME reason for every
/// one of them, which is itself the finding: each needs a helper call in the
/// MIDDLE of a method -- either as the fallback for a guard it cannot settle
/// at compile time, or as the operation itself -- and this backend has no such
/// call path. Every call it does emit (the safepoint poll, the three throw
/// stubs) either happens with an empty operand stack or is on its way OUT of
/// the frame, where nothing has to survive.
///
/// * `putstatic` -- a raw payload store races `set_static_shared`'s `grow_to`,
///   which copies the statics block under the write lock and republishes it,
///   so a compiled store into the old block between the copy and the
///   republish is LOST. x64 keeps the helper for this, and for the SATB
///   pre-barrier a reference store owes.
/// * `getfield`/`putfield` -- the NPE throw path exists now, but an instance
///   field has TWO layouts per OBJECT (the packed compact one and the legacy
///   16-byte tagged `Value` cell), chosen at allocation time, plus a
///   replaceable layout epoch. x64 emits a runtime branch whose every declined
///   path -- a compact receiver at a site with no baked compact offset, a
///   replaced layout, `field_index >= num_slots` -- falls back to
///   `jit_getfield` / `jit_putfield_*`. Without that fallback the lowering
///   would have to be total for every layout, and it cannot be: a compact
///   receiver at a site the resolver gave no compact offset for has no
///   computable address.
/// * the five `invoke*` forms (`0xb6..=0xba`, round 9 wave 22), through
///   `jit_invoke_dispatch`. The ordering claim is the SHAPE of this backend
///   rather than anything about the callee: no lowering here ever leaves a
///   fence owed. A `volatile` field or static emits its `LDAR`/`STLR` and its
///   `DMB ISH`s inline and completes; an array access owes nothing (ARM ARM
///   §B2.3.2); every reference store is inside the helper that runs the
///   barriers. There is no deferred-barrier state a call could outrun,
///   because there is no deferred-barrier state. The callee's own ordering is
///   the callee's, and `jit_invoke_dispatch` opens with
///   `jit_safepoint_flush_satb` -- the round-7 audit's requirement for a
///   helper whose callee may enter the GC barrier.
/// * `monitorenter`/`monitorexit` (`0xc2`/`0xc3`, h23 2026-09-22), through
///   `jit_monitor_enter` / `jit_monitor_exit`. The acquire and the release
///   are the HELPER's, exactly as they are for `ir_lower`'s arm on x64: the
///   lock word is CAS'd inside it, under its own barriers. Same ordering
///   claim as the calls above -- nothing here leaves a fence owed across a
///   `BLR` -- and see [`ARM64_LOWERS_EXCLUSIVE_ACCESS`], which stays `false`
///   because this lowering emits no exclusive-access instruction of its own.
/// * `checkcast`/`instanceof` (`0xc0`/`0xc1`, round 9 wave 23), through
///   `jit_checkcast` / `jit_instanceof_check`. Same ordering claim as the
///   calls above, and for the same reason: nothing here leaves a fence owed.
/// * `multianewarray` (`0xc5`, h23c 2026-09-22), through
///   `multianewarray_n`. Same ordering claim as the calls above -- an
///   allocation publishes a new object's header and its `final` fields, which
///   is the JMM's freeze action, and the helper performs it; nothing here
///   leaves a fence owed across the `BLR`.
pub(crate) fn opcode_has_ordered_lowering(opcode: u8) -> bool {
    matches!(opcode, 0x2e..=0x35 | 0x4f..=0x56 | 0xb2..=0xc3 | 0xc5)
}

/// The main ARM64 compilation pipeline.
///
/// Translates JVM bytecode into `Arm64Instruction` sequences using a
/// simulated operand stack (compile-time stack mapping).
pub struct Arm64Backend {
    buffer: Arm64CodeBuffer,
    frame: Option<Arm64FrameLayout>,
    /// Register assignment for each local variable (GPR); `None` means the
    /// local is frame-homed.
    local_regs: Vec<Option<Arm64Register>>,
    /// FP register assignment for float/double locals. Always `None`: see the
    /// comment on the loop that fills it in `compile_pass`.
    float_local_regs: Vec<Option<Arm64Register>>,
    /// The simulated operand stack, bottom first -- ONE stack for every kind.
    ///
    /// Each entry records where ITS value is: a scratch register, or the frame
    /// word reserved for its depth. This replaces a register-only stack plus a
    /// `register -> spill slot` map, which could not describe two entries that
    /// had used the same register: when the round-robin allocator wrapped onto
    /// a live register it spilled it, recorded `spill_map[R]`, and pushed R
    /// again for the NEW value, so popping the new value reloaded the OLD one.
    /// `a - (b+1+2+3+4)` returned -4 for `(100, 5)`. Floats lived on a second
    /// stack whose allocator had no liveness check and no spill at all.
    operand_stack: Vec<Operand>,
    /// Scratch registers the bytecode being lowered has popped and still reads.
    /// The allocator never hands one out, so a result register cannot overwrite
    /// an operand the same instruction has yet to consume. Cleared per bytecode.
    held: Vec<Arm64Register>,
    /// The operand-stack SHAPE (kind and oop mark per entry) at each branch
    /// target, recorded by the branches. Every path into a target leaves each
    /// entry in its depth slot, so the shape is all a target needs to rebuild
    /// the model. Recorded by pass 1 and read by pass 2, so a backward target
    /// is known before the walk reaches it.
    label_states: HashMap<usize, Vec<(OperandKind, bool)>>,
    /// Whether control falls into the instruction being lowered from the
    /// previous one (false after `goto`, `*return` and the switches).
    reachable: bool,
    /// Per-bci operand-stack kinds from the shared analysis. Consulted only to
    /// rebuild the model at a pc that no recorded branch describes.
    ///
    /// Populated with EMPTY metadata apart from the resolved `getstatic`
    /// types (round 9 wave 10), which costs nothing here: the analysis needs
    /// field types, call arities and constant-pool tags, and this backend
    /// refuses every method containing any other field access, a call of any
    /// kind, or any `ldc`.
    stack_kinds: crate::x64::stack_kinds::StackKindMap,
    /// Bytecode PC -> label mapping for branch targets.
    pc_labels: HashMap<usize, u32>,
    /// Bytecode PC of the instruction currently being lowered. Read by
    /// [`Arm64Backend::label_for_pc`] to tell a back-edge from a forward
    /// branch, and by the safepoint poll for its id and oop-local mask.
    cur_bytecode_pc: usize,
    /// Per-bytecode-pc "must be oop" local masks, and whether the dataflow
    /// reached each pc. Shared with x64 (`compute_local_oop_masks`). Empty when
    /// unsupported (>64 locals), which this backend treats as "no claim".
    local_oop_masks: Vec<u64>,
    local_oop_reached: Vec<bool>,
    /// Which parameter slots hold references on entry. The ENTRY poll answers
    /// from this. Zero unless [`Arm64Backend::set_method_descriptor`] was called.
    param_oop_mask: u64,
    /// The JVM local slot of each incoming argument, from the descriptor
    /// (`compute_param_jvm_slots`), or `None` for the identity layout
    /// `0..num_params` when no descriptor was supplied.
    param_jvm_slots: Option<Vec<usize>>,
    /// JVMS §6.5 `ireturn` narrowing for this method's declared return type
    /// (`Z`/`B`/`C`/`S`, from [`crate::narrowed_int_return_tag`]), or `None`.
    /// Set by [`Arm64Backend::set_method_descriptor`]; x64 applies the same
    /// narrowing (`emit_narrow_int_return`), so a compiled body never hands
    /// back a `boolean` of 2 or a `byte` of 300.
    return_narrow: Option<u8>,
    /// Resolved `getstatic` sites, keyed by the bytecode pc of the `0xb2`
    /// (round 9 wave 10). Empty unless [`Arm64Backend::set_static_field_info`]
    /// was called, in which case every `getstatic` refuses the method.
    static_fields: HashMap<usize, Arm64StaticField>,
    /// Resolved `getfield`/`putfield` sites, keyed by the bytecode pc of the
    /// `0xb4`/`0xb5` (round 9 wave 15). Empty unless
    /// [`Arm64Backend::set_instance_field_info`] was called, in which case
    /// every instance field access refuses the method.
    instance_fields: HashMap<usize, Arm64InstanceField>,
    /// Whether this compilation takes the VM context pointer (round 9 wave
    /// 16). Decided ONCE per pass, from the bytecode alone -- it must be the
    /// same in both passes, and it must be known before the frame is sized,
    /// because the context word is part of the frame.
    needs_context: bool,
    /// Whether this method contains an allocation (round 9 wave 18). Decided
    /// the same way and for the same reason: an allocation calls a helper that
    /// can SAFEPOINT, which needs the safepoint homes and the id word in the
    /// frame -- whether or not `CRATONVM_JIT_ARM64_SAFEPOINTS` reserved them
    /// for polling.
    allocates: bool,
    /// Resolved `new`/`anewarray` sites, keyed by the pc of the opcode (round
    /// 9 wave 18). Empty unless [`Arm64Backend::set_new_site_info`] was
    /// called, in which case every `new` and `anewarray` refuses the method.
    new_sites: HashMap<usize, Arm64NewSite>,
    /// Resolved `multianewarray` sites, keyed by the pc of the opcode (h23c):
    /// the packed `(holder_class_id | cp_idx << 32)` descriptor the helper
    /// resolves the array class from. A site absent from the map refuses the
    /// method, which is what an unwired caller gets for free.
    multianewarray_sites: HashMap<usize, i64>,
    /// Declaring class ids of every `getstatic` this compilation lowered,
    /// published as `CompiledMethod::static_init_classes`. Reset per pass.
    static_init_classes: Vec<u32>,
    /// Whether the caller vouched that this method's exception table is EMPTY
    /// (round 9 wave 11), via [`Arm64Backend::set_exception_table_empty`].
    /// `false` until told -- "unknown" must refuse, not assume. Gates the
    /// direct-throw lowerings (`idiv`/`irem`/`ldiv`/`lrem`): with no handler in
    /// this frame, the `ArithmeticException` the stub raises can only
    /// propagate to the caller, so no throw bci has to be recorded.
    exception_table_empty: bool,
    /// One out-of-line `ArithmeticException` stub per distinct bci (see
    /// [`Arm64Backend::arith_throw_label`]), emitted after the epilogue by
    /// [`Arm64Backend::emit_arith_throw_stub`]. Reset per pass.
    arith_throw_labels: Vec<(usize, u32)>,
    /// One out-of-line `ArrayIndexOutOfBoundsException` stub per SITE (round 9
    /// wave 13), emitted after the epilogue by
    /// [`Arm64Backend::emit_aioobe_stubs`]. Per site, not per action: the
    /// helper takes the index, the length, the array and the bci, and two
    /// sites share none of them.
    aioobe_stubs: Vec<Arm64AioobeStub>,
    /// One out-of-line `NullPointerException` stub per JEP-358 action code
    /// this pass needed (round 9 wave 12), emitted after the epilogue by
    /// [`Arm64Backend::emit_npe_throw_stubs`].
    ///
    /// A `Vec`, not a `HashMap`: the stubs are EMITTED from it, so their order
    /// is machine-code layout, and a `HashMap`'s iteration order would make
    /// two compilations of the same method produce different bytes.
    npe_throw_labels: Vec<(u8, usize, u32)>,
    /// Frame offsets of reference LOCALS at the safepoint being emitted, folded
    /// into the map by `emit_oop_map_for_safepoint`. Taken, not copied.
    pending_local_oop_slots: Vec<i32>,
    /// Frame offsets of reference OPERANDS the poll stored for its call. Taken,
    /// not copied, like the locals.
    pending_operand_oop_slots: Vec<i32>,
    /// Label for the shared epilogue.
    epilogue_label: u32,
    /// Number of parameter SLOTS for the current method.
    num_params: usize,
    /// Method invoke metadata: maps constant pool index to argument count.
    method_info: HashMap<u16, usize>,
    /// Set to true if a compilation error occurred (e.g. stack underflow).
    pub failed: bool,
    /// T1.1.3 — collected oop maps, PC-unresolved; see [`Arm64PendingOopMap`].
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
    /// Runtime helper addresses. Zeroed until [`Arm64Backend::set_helpers`] is
    /// called; `safepoint_flag_addr == 0` means "not wired" and the poll emits
    /// nothing, the same contract x64 uses.
    helpers: crate::JitRuntimeHelpers,
    /// Bytecode PCs that are the target of a BACKWARD branch -- loop headers.
    /// Discovered by pass 1 and read by pass 2, which polls at each one.
    back_edge_targets: std::collections::HashSet<usize>,
    /// Frame offset (POSITIVE; the slot is at `[FP - sp_id_slot_off]`) of the
    /// word each safepoint stamps its id into, or 0 when none is reserved.
    sp_id_slot_off: i32,
    /// How the spill area is partitioned — filled in when the frame is sized,
    /// and the single source of every word index below `spill_offset`. See
    /// [`Arm64SpillArea`]. All-zero before the prologue runs, which makes every
    /// accessor refuse rather than compute an offset into a frame that does not
    /// exist yet.
    spill_area: Arm64SpillArea,
    /// Safepoints this compilation published a map for, and how many of those
    /// maps could NOT describe everything live at their site. A count, not a
    /// set of pcs: two safepoints can share one bci.
    safepoint_count: usize,
    incomplete_oop_maps: usize,
    /// Set by the poll when it could not describe this site; consumed (taken)
    /// by the map writer.
    pending_map_incomplete: bool,
    /// Resolved `invoke*` sites, by bytecode pc (round 9 wave 22). A site
    /// absent from this map refuses the method in its own arm.
    invoke_sites: HashMap<usize, Arm64InvokeSite>,
    /// Resolved `checkcast`/`instanceof` sites, by bytecode pc (round 9 wave
    /// 23). A site absent from this map refuses the method.
    typecheck_sites: HashMap<usize, Arm64TypecheckSite>,
    /// Whether this method contains any `invoke*` (round 9 wave 22). Decided
    /// from the BYTECODE, not from `invoke_sites`, so the frame is the same
    /// shape whether or not the caller resolved the site -- an unresolved one
    /// refuses the method anyway, and both compile passes must agree on the
    /// frame before either knows that.
    calls: bool,
    /// Set by [`Self::emit_frame_record`] when it actually emitted the call
    /// that publishes FP. See `Arm64CompileResult::frame_base_published`.
    frame_base_published: bool,
    /// Whether this compilation emits safepoint polls, seeded from
    /// [`arm64_safepoints_enabled`] in `new()`. A field so a test can reach
    /// both arms of a process-latched switch.
    safepoints_enabled: bool,
}

/// Scratch registers available for the operand stack (X9-X15, 7 regs).
const SCRATCH_REGS: [Arm64Register; 7] = [
    Arm64Register::X9,
    Arm64Register::X10,
    Arm64Register::X11,
    Arm64Register::X12,
    Arm64Register::X13,
    Arm64Register::X14,
    Arm64Register::X15,
];

/// Float scratch registers for the operand stack (V0-V7, 8 regs).
const FLOAT_SCRATCH_REGS: [Arm64Register; 8] = [
    Arm64Register::V0,
    Arm64Register::V1,
    Arm64Register::V2,
    Arm64Register::V3,
    Arm64Register::V4,
    Arm64Register::V5,
    Arm64Register::V6,
    Arm64Register::V7,
];

/// Page size the stack bang probes at. AArch64 Linux, Windows and Darwin all
/// place at least this much guard below a thread's stack.
const STACK_BANG_PAGE_BYTES: i32 = 4096;

/// Probes beyond this many refuse the method (a 2 MiB frame), for the same
/// code-size reason as x64's `MAX_STACK_BANG_PROBES`.
const MAX_STACK_BANG_PROBES: usize = 512;

/// SP-relative distances (subtracted from SP) the prologue probes before it
/// moves SP down by `frame_below` bytes, or `None` when the frame needs more
/// probes than [`MAX_STACK_BANG_PROBES`].
///
/// The same scheme as x64's `stack_bang_frame_probe_disps`: one probe per page
/// the new frame crosses, plus the exact frame bottom when it is not
/// page-aligned. A frame smaller than a page needs none: it cannot step over
/// a guard page, so its own first store already lands on the guard.
fn stack_bang_probe_offsets(frame_below: i32) -> Option<Vec<i32>> {
    if frame_below < 0 {
        return None;
    }
    let mut offsets = Vec::new();
    let mut off = STACK_BANG_PAGE_BYTES;
    while off <= frame_below {
        if offsets.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        offsets.push(off);
        off = off.checked_add(STACK_BANG_PAGE_BYTES)?;
    }
    if frame_below >= STACK_BANG_PAGE_BYTES && frame_below % STACK_BANG_PAGE_BYTES != 0 {
        if offsets.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        offsets.push(frame_below);
    }
    Some(offsets)
}

/// One deferred `ArrayIndexOutOfBoundsException` site (round 9 wave 13).
///
/// The guard's `B.HS` is the only way in, so the three register fields name
/// where that guard left the helper's arguments -- not where they live in
/// general, which is a question with no answer once the bytecode is over.
#[derive(Debug, Clone, Copy)]
struct Arm64AioobeStub {
    label: u32,
    index: Arm64Register,
    length: Arm64Register,
    array: Arm64Register,
    bci: usize,
}

/// What one primitive array element opcode needs to know (round 9 wave 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arm64ArrayShape {
    /// The operand-stack kind the element loads to, or stores from.
    kind: OperandKind,
    /// The access width, i.e. the element's byte size.
    width: Arm64MemWidth,
    /// `log2(element size)`, the `LSL` amount the index is scaled by.
    shift: u8,
    /// The JEP-358 action code a null receiver reports.
    action: u8,
    /// Whether a sub-word LOAD sign-extends (`baload`, `saload`) rather than
    /// zero-extends (`caload`). Meaningless for a store, which truncates.
    signed: bool,
}

/// The array LOAD opcodes this backend lowers, and what each one is.
///
/// `aaload` (`0x32`) is deliberately absent: a reference element is a narrow
/// oop when compressed oops are on, must be marked as an oop on the operand
/// stack, and becomes a GC root this backend would then owe a map for at every
/// later safepoint. None of that is an ordering question, which is why it is a
/// separate piece of work rather than a row here.
fn array_element_load_shape(opcode: u8) -> Option<Arm64ArrayShape> {
    let (kind, width, shift, action, signed) = match opcode {
        // iaload
        0x2e => (
            OperandKind::I32,
            Arm64MemWidth::W32,
            2,
            npe_action::ALOAD_INT,
            true,
        ),
        // laload
        0x2f => (
            OperandKind::I64,
            Arm64MemWidth::X64,
            3,
            npe_action::ALOAD_LONG,
            true,
        ),
        // faload
        0x30 => (
            OperandKind::F32,
            Arm64MemWidth::W32,
            2,
            npe_action::ALOAD_FLOAT,
            true,
        ),
        // daload
        0x31 => (
            OperandKind::F64,
            Arm64MemWidth::X64,
            3,
            npe_action::ALOAD_DOUBLE,
            true,
        ),
        // baload -- `byte[]` sign-extends; a `boolean[]` element is 0 or 1, so
        // the two narrowings agree and one lowering serves both.
        0x33 => (
            OperandKind::I32,
            Arm64MemWidth::B8,
            0,
            npe_action::ALOAD_BYTE,
            true,
        ),
        // caload -- `char` is unsigned, so the zero-extending load IS the value.
        0x34 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ALOAD_CHAR,
            false,
        ),
        // saload
        0x35 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ALOAD_SHORT,
            true,
        ),
        _ => return None,
    };
    Some(Arm64ArrayShape {
        kind,
        width,
        shift,
        action,
        signed,
    })
}

/// The array STORE opcodes this backend lowers, and what each one is.
///
/// `aastore` (`0x53`) is deliberately absent, and for more reasons than
/// `aaload`: a reference store owes the SATB pre-write barrier (round-7's
/// use-after-free fix), a card mark, and the runtime component-type check that
/// raises `ArrayStoreException` -- three mechanisms this backend has none of.
fn array_element_store_shape(opcode: u8) -> Option<Arm64ArrayShape> {
    let (kind, width, shift, action) = match opcode {
        // iastore
        0x4f => (
            OperandKind::I32,
            Arm64MemWidth::W32,
            2,
            npe_action::ASTORE_INT,
        ),
        // lastore
        0x50 => (
            OperandKind::I64,
            Arm64MemWidth::X64,
            3,
            npe_action::ASTORE_LONG,
        ),
        // fastore
        0x51 => (
            OperandKind::F32,
            Arm64MemWidth::W32,
            2,
            npe_action::ASTORE_FLOAT,
        ),
        // dastore
        0x52 => (
            OperandKind::F64,
            Arm64MemWidth::X64,
            3,
            npe_action::ASTORE_DOUBLE,
        ),
        // bastore
        0x54 => (
            OperandKind::I32,
            Arm64MemWidth::B8,
            0,
            npe_action::ASTORE_BYTE,
        ),
        // castore
        0x55 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ASTORE_CHAR,
        ),
        // sastore
        0x56 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ASTORE_SHORT,
        ),
        _ => return None,
    };
    Some(Arm64ArrayShape {
        kind,
        width,
        shift,
        action,
        // A store truncates whatever the width is; there is no extension to
        // choose. Fixed `true` so two shapes never differ on a dead field.
        signed: true,
    })
}

/// The operand-stack kind a PRIMITIVE field of `type_tag` is read and written
/// as, or `None` for a reference field (`L`/`[`) or an unrecognised tag.
///
/// The sub-`int` tags all answer `I32`: JVMS keeps `boolean`, `byte`, `char`
/// and `short` on the operand stack as `int`s, and their narrower declared
/// widths matter only at the store ([`Arm64Backend::emit_narrow_to_field_tag`])
/// and at the load's sign/zero extension.
fn primitive_field_operand_kind(type_tag: u8) -> Option<OperandKind> {
    match type_tag {
        b'Z' | b'B' | b'C' | b'S' | b'I' => Some(OperandKind::I32),
        b'J' => Some(OperandKind::I64),
        b'F' => Some(OperandKind::F32),
        b'D' => Some(OperandKind::F64),
        _ => None,
    }
}

/// One argument to a mid-method helper call (round 9 wave 15).
///
/// The register form names a SCRATCH register (X9-X15) and nothing else, which
/// [`Arm64Backend::emit_helper_call`] enforces rather than assumes: the
/// arguments land in X0-X7, so a source that was itself an argument register
/// could be overwritten by an earlier argument's move, and the call would pass
/// a value that was correct when the emitter looked at it and wrong by the
/// time the `BLR` ran. Every value this backend can hand a helper already
/// lives in the scratch pool -- the operand stack is allocated out of it and
/// nothing else is poppable -- so the check costs nothing and closes the case
/// permanently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm64HelperArg {
    /// A value already in a scratch register.
    Reg(Arm64Register),
    /// A compile-time constant, materialized with `MOVZ`/`MOVK`.
    Imm(i64),
}

impl Arm64Backend {
    pub fn new() -> Self {
        Self {
            buffer: Arm64CodeBuffer::new(),
            frame: None,
            local_regs: Vec::new(),
            float_local_regs: Vec::new(),
            operand_stack: Vec::new(),
            held: Vec::new(),
            label_states: HashMap::new(),
            reachable: true,
            stack_kinds: crate::x64::stack_kinds::StackKindMap::default(),
            pc_labels: HashMap::new(),
            cur_bytecode_pc: 0,
            local_oop_masks: Vec::new(),
            local_oop_reached: Vec::new(),
            param_oop_mask: 0,
            param_jvm_slots: None,
            return_narrow: None,
            static_fields: HashMap::new(),
            instance_fields: HashMap::new(),
            needs_context: false,
            allocates: false,
            new_sites: HashMap::new(),
            multianewarray_sites: HashMap::new(),
            static_init_classes: Vec::new(),
            exception_table_empty: false,
            arith_throw_labels: Vec::new(),
            npe_throw_labels: Vec::new(),
            aioobe_stubs: Vec::new(),
            pending_local_oop_slots: Vec::new(),
            pending_operand_oop_slots: Vec::new(),
            epilogue_label: 0,
            num_params: 0,
            method_info: HashMap::new(),
            failed: false,
            pending_oop_maps: Vec::new(),
            // SAFETY: `JitRuntimeHelpers` is a plain struct of `usize`
            // addresses; all-zero is its documented "nothing wired" state, and
            // `safepoint_flag_addr == 0` is what gates the poll emitter.
            helpers: unsafe { std::mem::zeroed() },
            back_edge_targets: std::collections::HashSet::new(),
            sp_id_slot_off: 0,
            spill_area: Arm64SpillArea::default(),
            safepoint_count: 0,
            incomplete_oop_maps: 0,
            pending_map_incomplete: false,
            invoke_sites: HashMap::new(),
            typecheck_sites: HashMap::new(),
            calls: false,
            frame_base_published: false,
            safepoints_enabled: arm64_safepoints_enabled(),
        }
    }

    /// T1.1.3 — mark the top of the operand stack as holding an object
    /// reference. Production pushes set the mark from the entry's kind; this
    /// remains for tests that build a stack by hand.
    #[allow(dead_code)]
    fn mark_top_operand_as_oop(&mut self) {
        if let Some(top) = self.operand_stack.last_mut() {
            top.oop = true;
        }
    }

    /// Record this safepoint's oop map: the frame slots that hold object
    /// references right now, keyed so the encoder can give them a real PC.
    ///
    /// The PC an `OopMapEntry` needs is a BYTE OFFSET, and this backend has a
    /// pseudo-op stream whose entries are not 4 bytes each, so the map is
    /// recorded against the pseudo-op INDEX of the instruction that follows
    /// the safepoint, in an [`Arm64PendingOopMap`], and
    /// [`emit_machine_code_with_oop_maps`] translates it.
    ///
    /// Names: reference operands already in their depth slots, the reference
    /// operands the poll stored for its call (`pending_operand_oop_slots`),
    /// and the reference locals it staged (`pending_local_oop_slots`). A
    /// reference operand still in a REGISTER is not nameable; the poll stores
    /// every one before calling this, which is why it is the only caller.
    fn emit_oop_map_for_safepoint(&mut self, safepoint_id: u32) {
        if self.failed {
            return;
        }
        // The pseudo-op that will FOLLOW this safepoint: the "return address"
        // convention x64 records.
        let pseudo_index = match u32::try_from(self.buffer.instruction_count()) {
            Ok(n) => n,
            Err(_) => {
                self.failed = true;
                return;
            }
        };

        let mut offsets: Vec<i32> = self
            .operand_stack
            .iter()
            .filter(|o| o.oop)
            .filter_map(|o| match o.loc {
                OperandLoc::Slot(off) => Some(off),
                OperandLoc::Reg(_) => None,
            })
            .collect();
        offsets.extend(std::mem::take(&mut self.pending_operand_oop_slots));
        offsets.extend(std::mem::take(&mut self.pending_local_oop_slots));

        let mut slots: Vec<i16> = Vec::new();
        for off in offsets {
            match i16::try_from(off) {
                Ok(off16) => {
                    if !slots.contains(&off16) {
                        slots.push(off16);
                    }
                }
                // A slot further than `i16` from FP. There is no completeness
                // channel for this, so refuse rather than publish a map that
                // silently drops a live reference.
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        }
        // PUBLISH EVERY SAFEPOINT, even one with no live reference: an id whose
        // map is absent is indistinguishable from an uncovered frame.
        self.safepoint_count += 1;
        if std::mem::take(&mut self.pending_map_incomplete) {
            self.incomplete_oop_maps += 1;
        }
        self.pending_oop_maps.push(Arm64PendingOopMap {
            pseudo_index,
            frame_slot_offsets: slots,
            safepoint_id,
        });
    }

    // -- The operand model ----------------------------------------------------

    /// Whether some operand-stack entry lives in `reg`.
    fn reg_is_live(&self, reg: Arm64Register) -> bool {
        self.operand_stack
            .iter()
            .any(|o| o.loc == OperandLoc::Reg(reg))
    }

    /// A scratch register of the requested class that holds nothing the
    /// current bytecode still needs. The register is HELD until the next
    /// bytecode.
    ///
    /// When every register of the class is live or held, the DEEPEST
    /// register-located entry is moved to its depth slot first. The entry
    /// records the move, so its value is found again by depth -- never by
    /// asking which register it used to be in.
    fn alloc_reg(&mut self, fp: bool) -> Arm64Register {
        let pool: &'static [Arm64Register] = if fp {
            &FLOAT_SCRATCH_REGS
        } else {
            &SCRATCH_REGS
        };
        if let Some(&reg) = pool
            .iter()
            .find(|&&r| !self.held.contains(&r) && !self.reg_is_live(r))
        {
            self.held.push(reg);
            return reg;
        }
        let victim = self.operand_stack.iter().position(|o| match o.loc {
            OperandLoc::Reg(r) => pool.contains(&r) && !self.held.contains(&r),
            OperandLoc::Slot(_) => false,
        });
        let Some(depth) = victim else {
            // Every register of the class is held by this one bytecode. No
            // bytecode needs that many; refuse rather than alias.
            self.failed = true;
            return pool[0];
        };
        let OperandLoc::Reg(reg) = self.operand_stack[depth].loc else {
            self.failed = true;
            return pool[0];
        };
        if !self.spill_entry(depth) {
            return pool[0];
        }
        self.held.push(reg);
        reg
    }

    /// Move entry `depth` from its register into its depth slot. `false` (and
    /// `failed`) when the slot is outside the reserved operand area.
    fn spill_entry(&mut self, depth: usize) -> bool {
        let Some(entry) = self.operand_stack.get(depth).copied() else {
            self.failed = true;
            return false;
        };
        let OperandLoc::Reg(reg) = entry.loc else {
            return true;
        };
        let Some(offset) = self.spill_offset_for_depth(depth) else {
            self.failed = true;
            return false;
        };
        self.emit_store_kind(reg, entry.kind, offset);
        self.operand_stack[depth].loc = OperandLoc::Slot(offset);
        true
    }

    /// Put every entry in its depth slot: the one layout that every path into
    /// a branch target agrees on.
    fn spill_all(&mut self) {
        for depth in 0..self.operand_stack.len() {
            if !self.spill_entry(depth) {
                return;
            }
        }
    }

    /// Store `reg` (holding a `kind`) to `[FP + offset]` at the kind's width.
    fn emit_store_kind(&mut self, reg: Arm64Register, kind: OperandKind, offset: i32) {
        let inst = match kind {
            OperandKind::F32 | OperandKind::F64 => Arm64Instruction::FpStr {
                vt: reg,
                rn: Arm64Register::FP,
                offset,
                is_double: kind == OperandKind::F64,
            },
            _ => Arm64Instruction::Str {
                rt: reg,
                rn: Arm64Register::FP,
                offset,
            },
        };
        self.buffer.emit(inst);
    }

    /// Load a `kind` from `[FP + offset]` into `reg` at the kind's width.
    fn emit_load_kind(&mut self, reg: Arm64Register, kind: OperandKind, offset: i32) {
        let inst = match kind {
            OperandKind::F32 | OperandKind::F64 => Arm64Instruction::FpLdr {
                vt: reg,
                rn: Arm64Register::FP,
                offset,
                is_double: kind == OperandKind::F64,
            },
            _ => Arm64Instruction::Ldr {
                rt: reg,
                rn: Arm64Register::FP,
                offset,
            },
        };
        self.buffer.emit(inst);
    }

    /// Push a value the current bytecode computed into `reg`.
    fn push_reg(&mut self, kind: OperandKind, reg: Arm64Register) {
        self.operand_stack.push(Operand::in_reg(kind, reg));
    }

    /// Push `entry`'s kind and oop mark, now living in `reg`.
    fn push_like(&mut self, entry: Operand, reg: Arm64Register) {
        self.operand_stack.push(Operand {
            loc: OperandLoc::Reg(reg),
            ..entry
        });
    }

    /// Pop the top entry into a register, reloading it from its slot when it
    /// was spilled. The register is HELD for the rest of this bytecode.
    /// Underflow sets `failed` and answers `None`.
    fn pop_entry(&mut self) -> Option<(Arm64Register, Operand)> {
        let Some(entry) = self.operand_stack.pop() else {
            self.failed = true;
            return None;
        };
        match entry.loc {
            OperandLoc::Reg(reg) => {
                self.held.push(reg);
                Some((reg, entry))
            }
            OperandLoc::Slot(offset) => {
                let reg = self.alloc_reg(entry.kind.is_fp());
                self.emit_load_kind(reg, entry.kind, offset);
                Some((reg, entry))
            }
        }
    }

    /// Pop an entry that must be a `want`. Any other kind is a malformed
    /// method (the verifier would reject it) or a model bug; either refuses.
    fn pop_kind(&mut self, want: OperandKind) -> Arm64Register {
        let fallback = if want.is_fp() {
            Arm64Register::V0
        } else {
            Arm64Register::X0
        };
        match self.pop_entry() {
            Some((reg, entry)) if entry.kind == want => reg,
            Some(_) => {
                self.failed = true;
                fallback
            }
            None => fallback,
        }
    }

    /// Pop an integer-class (int, long or reference) entry. Returns X0 as a
    /// sentinel and sets `self.failed = true` on underflow or an FP entry.
    pub fn pop_operand(&mut self) -> Arm64Register {
        match self.pop_entry() {
            Some((reg, entry)) if !entry.kind.is_fp() => reg,
            Some(_) => {
                self.failed = true;
                Arm64Register::X0
            }
            None => Arm64Register::X0,
        }
    }

    /// Discard the top entry without materializing it.
    fn drop_top(&mut self) {
        if self.operand_stack.pop().is_none() {
            self.failed = true;
        }
    }

    /// Re-establish the sign-extended `int` form after a W-form producer.
    fn emit_sxtw(&mut self, reg: Arm64Register) {
        self.buffer
            .emit(Arm64Instruction::Sxtw { rd: reg, rn: reg });
    }

    /// The kind and oop mark of every entry, bottom first.
    fn stack_shape(&self) -> Vec<(OperandKind, bool)> {
        self.operand_stack.iter().map(|o| (o.kind, o.oop)).collect()
    }

    /// Record `shape` for target `pc`, or refuse the method when a different
    /// path already recorded a different one.
    fn check_or_record_shape(&mut self, pc: usize, shape: Vec<(OperandKind, bool)>) {
        match self.label_states.get(&pc) {
            Some(existing) if *existing != shape => self.failed = true,
            Some(_) => {}
            None => {
                self.label_states.insert(pc, shape);
            }
        }
    }

    /// Everything a branch to `target` must do before it is emitted: put the
    /// stack in its canonical all-slots layout, record that shape, and resolve
    /// the label. Only STORES are emitted, so flags and held registers survive.
    fn prepare_branch(&mut self, target: usize) -> u32 {
        self.spill_all();
        let shape = self.stack_shape();
        self.check_or_record_shape(target, shape);
        self.label_for_pc(target)
    }

    /// The walk is about to fall into branch target `pc`: arrive in the layout
    /// the branches to it use.
    fn arrive_at_target(&mut self, pc: usize) {
        self.spill_all();
        let shape = self.stack_shape();
        self.check_or_record_shape(pc, shape);
    }

    /// Rebuild the model at a pc control cannot fall into: from the shape a
    /// branch recorded, else from the shared stack-kind analysis. With neither
    /// the pc is unreachable, and it is lowered against an empty stack -- at
    /// worst an underflow refuses the method.
    fn restore_stack_at(&mut self, pc: usize) {
        let shape = match self.label_states.get(&pc) {
            Some(s) => Some(s.clone()),
            None => self.shape_from_analysis(pc),
        };
        self.operand_stack.clear();
        let Some(shape) = shape else {
            return;
        };
        for (depth, (kind, oop)) in shape.into_iter().enumerate() {
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                self.failed = true;
                return;
            };
            self.operand_stack.push(Operand {
                kind,
                loc: OperandLoc::Slot(offset),
                oop,
            });
        }
    }

    /// The operand-stack shape at `pc` according to the shared analysis, or
    /// `None` when it has no answer or an entry is untyped.
    fn shape_from_analysis(&self, pc: usize) -> Option<Vec<(OperandKind, bool)>> {
        use crate::x64::stack_kinds::StackKind;
        self.stack_kinds
            .get(pc)?
            .iter()
            .map(|k| match k {
                StackKind::Int => Some((OperandKind::I32, false)),
                StackKind::Long => Some((OperandKind::I64, false)),
                StackKind::Float => Some((OperandKind::F32, false)),
                StackKind::Double => Some((OperandKind::F64, false)),
                StackKind::Ref => Some((OperandKind::Ref, true)),
                StackKind::Unknown => None,
            })
            .collect()
    }

    /// Supply the runtime helper addresses this backend needs for a safepoint
    /// poll. Without it `safepoint_flag_addr` stays 0 and
    /// [`Self::emit_safepoint_poll`] emits nothing.
    pub fn set_helpers(&mut self, helpers: crate::JitRuntimeHelpers) {
        self.helpers = helpers;
    }

    /// Supply the resolved `invoke*` sites (round 9 wave 22). Without this
    /// every `invoke*` refuses, which is what this backend did from its first
    /// commit until that wave.
    pub fn set_invoke_site_info(&mut self, sites: HashMap<usize, Arm64InvokeSite>) {
        self.invoke_sites = sites;
    }

    /// Supply the resolved `checkcast`/`instanceof` sites (round 9 wave 23).
    pub fn set_typecheck_site_info(&mut self, sites: HashMap<usize, Arm64TypecheckSite>) {
        self.typecheck_sites = sites;
    }

    /// Override the safepoint-poll decision for this compilation.
    ///
    /// Exists because [`arm64_safepoints_enabled`] latches a `OnceLock`, so a
    /// test binary can only ever observe one arm of it.
    pub fn set_safepoints_enabled(&mut self, on: bool) {
        self.safepoints_enabled = on;
    }

    /// Seed the reference-parameter mask and the argument-to-slot layout from
    /// this method's descriptor.
    ///
    /// Must be called BEFORE compiling. The mask feeds
    /// `compute_local_oop_masks` and the entry poll. The slot layout is what
    /// the prologue homes each argument by: the VM passes ONE register per
    /// argument, and a `long`/`double` argument occupies TWO JVM local slots,
    /// so argument `i` is local `i` only while every earlier argument is
    /// category 1. Without a descriptor the identity layout is assumed, which
    /// is right for exactly those signatures.
    pub fn set_method_descriptor(&mut self, descriptor: &str, is_static: bool) {
        self.param_oop_mask = crate::compute_param_oop_mask(descriptor, is_static);
        self.param_jvm_slots = Some(crate::compute_param_jvm_slots(descriptor, is_static).0);
        self.return_narrow = crate::narrowed_int_return_tag(descriptor);
    }

    /// Supply the resolved `getstatic` sites, keyed by the pc of each `0xb2`
    /// (round 9 wave 10). Must be called BEFORE compiling. A site that is
    /// absent, or whose field is not a primitive, refuses the method; see
    /// [`Arm64StaticField`] for what each entry promises and
    /// [`static_field_sites`] for the walk that finds the pcs.
    pub fn set_static_field_info(&mut self, fields: HashMap<usize, Arm64StaticField>) {
        self.static_fields = fields;
    }

    /// Resolved `getfield`/`putfield` sites, keyed by the pc of the opcode
    /// (round 9 wave 15). A site absent from the map refuses the method, which
    /// is what an unwired caller gets for free: the map is empty until this is
    /// called.
    pub fn set_instance_field_info(&mut self, fields: HashMap<usize, Arm64InstanceField>) {
        self.instance_fields = fields;
    }

    /// Resolved `new`/`anewarray` sites, keyed by the pc of the opcode (round
    /// 9 wave 18). A site absent from the map refuses the method, which is
    /// what an unwired caller gets for free: the map is empty until this is
    /// called. `newarray` is not here -- its `atype` is an operand byte, so
    /// there is nothing for a caller to resolve.
    pub fn set_new_site_info(&mut self, sites: HashMap<usize, Arm64NewSite>) {
        self.new_sites = sites;
    }

    /// Supply the resolved `multianewarray` sites (h23c), keyed by the pc of
    /// the `0xc5`, each the packed `(holder_class_id | cp_idx << 32)`
    /// descriptor `cratonvm_jit::pack_multianewarray_site` builds.
    ///
    /// The descriptor carries no ARITY -- the helper takes that separately --
    /// so one map serves every dimension count, and the backend reads the
    /// count from the opcode's own operand byte.
    pub fn set_multianewarray_site_info(&mut self, sites: HashMap<usize, i64>) {
        self.multianewarray_sites = sites;
    }

    /// Tell the backend whether this method's exception table is empty
    /// (round 9 wave 11). Must be called BEFORE compiling; the caller passes
    /// `cached.exception_table.is_empty()`.
    ///
    /// The one fact the direct-throw lowerings need that the bytecode does not
    /// carry. `idiv`/`irem`/`ldiv`/`lrem` compile only when it is `true`
    /// (and `helpers.throw_arithmetic` is wired): the zero-divisor stub leaves
    /// this frame through its epilogue and the interpreter's JIT-return drain
    /// raises the exception with an UNKNOWN throw pc, which is exact only when
    /// no handler in this method could have caught it.
    pub fn set_exception_table_empty(&mut self, empty: bool) {
        self.exception_table_empty = empty;
    }

    /// `jit_set_throw_bci(bci)` — the x64 twin is the stamp every trap stub
    /// there emits (`x64/deopt_stubs.rs`) before returning the `i64::MIN`
    /// deopt sentinel through the epilogue. The interpreter's JIT-return
    /// drain routes a pending exception through THIS method's own exception
    /// table by bci; without the stamp it has only an unknown return pc,
    /// which is exact only when the table is empty (nothing to route to) --
    /// the condition every trap lowering here used to require outright.
    ///
    /// A no-op when [`Self::exception_table_empty`] holds: propagation is
    /// unconditionally correct with nothing to catch, and the stamp is a
    /// helper call this backend has no reason to pay on that path. Also a
    /// no-op when the helper is unwired, matching this backend's usual
    /// "unwired means byte-identical to before" contract -- callers that gate
    /// on [`Self::can_route_exception`] never reach a live trap edge in that
    /// case anyway.
    fn emit_stamp_throw_bci(&mut self, bci: usize) {
        if self.exception_table_empty || self.helpers.set_throw_bci == 0 {
            return;
        }
        let Ok(bci) = i64::try_from(bci) else {
            self.failed = true;
            return;
        };
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: bci,
        });
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.set_throw_bci as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
    }

    /// Whether a trapping lowering can leave through the epilogue with the
    /// `i64::MIN` deopt sentinel and have the interpreter's drain route it
    /// correctly: either nothing in this method could catch it
    /// ([`Self::exception_table_empty`]), or [`Self::emit_stamp_throw_bci`]
    /// is available to tell the drain exactly which bci trapped, so it can
    /// search this method's own exception table the way the interpreter
    /// always has.
    ///
    /// Replaces the plain `exception_table_empty` refusal every trapping
    /// lowering here carried before the stamp existed. A caller that gates on
    /// this MUST call [`Self::emit_stamp_throw_bci`] on every edge that
    /// returns the sentinel -- this predicate only says the stamp is
    /// possible, not that a given call site remembered to emit it.
    fn can_route_exception(&self) -> bool {
        self.exception_table_empty || self.helpers.set_throw_bci != 0
    }

    /// Can this compilation raise `ArithmeticException` from compiled code?
    ///
    /// Both halves are "not wired" refusals: without the helper there is no
    /// throw path, and without [`Self::can_route_exception`] the stub's throw
    /// pc could pick the wrong handler (or miss the right one). Either
    /// missing keeps the round-7 refusal, so an unwired caller is
    /// byte-identical to before.
    fn can_throw_arithmetic(&self) -> bool {
        self.helpers.throw_arithmetic != 0 && self.can_route_exception()
    }

    /// The label of this pass's `ArithmeticException` stub for the CURRENT
    /// bci, creating it on first use.
    ///
    /// Keyed by bci, not shared across the whole method: once
    /// [`Self::emit_stamp_throw_bci`] is live (a non-empty exception table),
    /// two `idiv`s at different bcis that shared one stub would have the
    /// second one's throw routed through the first one's bci, silently
    /// picking the wrong handler or missing the right one. With an empty
    /// table every bci maps to the one stub anyway (the stamp is a no-op, so
    /// nothing distinguishes them) -- `self.exception_table_empty` collapses
    /// the key so that case keeps sharing one stub, unchanged from before
    /// this existed.
    fn arith_throw_label(&mut self) -> u32 {
        let key = if self.exception_table_empty {
            usize::MAX
        } else {
            self.cur_bytecode_pc
        };
        if let Some(&(_, label)) = self.arith_throw_labels.iter().find(|(b, _)| *b == key) {
            return label;
        }
        let label = self.buffer.new_label();
        self.arith_throw_labels.push((key, label));
        label
    }

    /// Emit every `ArithmeticException` stub this pass branched to, one per
    /// distinct bci (see [`Self::arith_throw_label`]). The aarch64 twin of
    /// x64's reason-3 deopt stub (`x64/deopt_stubs.rs`):
    ///
    /// ```text
    ///   throw_<bci>:
    ///     <emit_stamp_throw_bci(bci), if the exception table is not empty>
    ///     MOVZ/MOVK X16, #jit_throw_arithmetic
    ///     BLR       X16          ; sets the pending-arithmetic + deopt
    ///                            ; signals, returns i64::MIN in X0
    ///     B         epilogue     ; X0 survives the epilogue
    /// ```
    ///
    /// `jit_throw_arithmetic` takes no arguments, allocates nothing and runs
    /// no Java, so the call is not a safepoint: no operand is spilled and no
    /// oop map is recorded (x64 records none either). The operand stack and
    /// the caller-saved registers are dead -- this frame is leaving -- and the
    /// callee-saved local homes are restored by the epilogue, which is the
    /// same one every `*return` branches to. SP is 16-aligned here, as at the
    /// safepoint poll's `BLR`: the frame is sized in 16-byte units and
    /// nothing below it is pushed.
    fn emit_arith_throw_stub(&mut self) {
        let stubs = std::mem::take(&mut self.arith_throw_labels);
        for (bci, label) in stubs {
            self.buffer.bind_label(label);
            if bci != usize::MAX {
                self.emit_stamp_throw_bci(bci);
            }
            // Cast: a helper address is a real mapped pointer, always < i64::MAX.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X16,
                imm: self.helpers.throw_arithmetic as i64,
            });
            self.buffer.emit(Arm64Instruction::Blr {
                rn: Arm64Register::X16,
            });
            self.buffer.emit(Arm64Instruction::B {
                label: self.epilogue_label,
            });
        }
    }

    /// Can this compilation raise `NullPointerException` from compiled code?
    /// (Round 9 wave 12.)
    ///
    /// The same two-part condition as [`Self::can_throw_arithmetic`], for the
    /// same two reasons: without `helpers.jit_npe_with_action` there is no
    /// throw path at all, and without [`Self::can_route_exception`] the
    /// drain's `JitThrowPc::Unknown` could pick the wrong handler (or skip a
    /// `finally`). Either missing keeps the historical refusal, so an
    /// unwired caller is byte-identical to before.
    fn can_throw_npe(&self) -> bool {
        self.helpers.jit_npe_with_action != 0 && self.can_route_exception()
    }

    /// The label of this pass's `NullPointerException` stub for JEP-358
    /// `action` at the CURRENT bci, creating it on first use.
    ///
    /// One stub per `(action, bci)`, not per action alone, once
    /// [`Self::emit_stamp_throw_bci`] is live: the helper's argument is only
    /// the action code, so two `arraylength`s on different receivers still
    /// want the same stub body, but with a non-empty exception table each
    /// needs its OWN throw-bci stamp ahead of it, or a null `arraylength` at
    /// bci 20 would route through whatever bci last shared this stub. With an
    /// empty table the stamp is a no-op and every bci collapses back onto one
    /// stub per action, unchanged from before this existed -- see
    /// [`Self::arith_throw_label`] for the identical reasoning.
    fn npe_throw_label(&mut self, action: u8) -> u32 {
        let bci = if self.exception_table_empty {
            usize::MAX
        } else {
            self.cur_bytecode_pc
        };
        if let Some(&(_, _, label)) = self
            .npe_throw_labels
            .iter()
            .find(|(a, b, _)| *a == action && *b == bci)
        {
            return label;
        }
        let label = self.buffer.new_label();
        self.npe_throw_labels.push((action, bci, label));
        label
    }

    /// Emit every `NullPointerException` stub this pass branched to, after the
    /// epilogue's `RET`. The NPE twin of [`Self::emit_arith_throw_stub`]:
    ///
    /// ```text
    ///   npe_<action>_<bci>:
    ///     <emit_stamp_throw_bci(bci), if the exception table is not empty>
    ///     MOVZ/MOVK X0,  #action      ; the JEP-358 code, unpacked site id 0
    ///     MOVZ/MOVK X16, #jit_npe_with_action
    ///     BLR       X16               ; sets pending-NPE(action) + deopt
    ///     MOVZ      X0,  #i64::MIN    ; the deopt sentinel THIS stub owes
    ///     B         epilogue
    /// ```
    ///
    /// The one shape difference from the arithmetic stub is the last `MOV`:
    /// `jit_throw_arithmetic` RETURNS `i64::MIN` in X0, while
    /// `jit_npe_with_action` returns `()` (its x64 caller is a per-action stub
    /// that loads the sentinel itself, `x64::emit_null_check_store_stubs`), so
    /// the sentinel is this stub's to materialize. Emitting the call and
    /// forgetting the sentinel would return whatever the helper left in X0 as
    /// the method's result and drop the exception on the floor.
    ///
    /// The argument is the PACKED word `jit_npe_with_action` decodes: low byte
    /// the action, upper 24 bits an inline null-check site id. This backend has
    /// no `NpeTrapSite` table, so the id is 0 -- the helper's documented
    /// "stub that sets only the action" case, which reads back as
    /// `trap_key == 0` with no special casing.
    ///
    /// Not a safepoint, for the same reason the arithmetic stub is not: the
    /// helper allocates nothing and runs no Java, the operand stack is dead
    /// (this frame is leaving), and the epilogue restores the callee-saved
    /// homes.
    fn emit_npe_throw_stubs(&mut self) {
        let stubs = std::mem::take(&mut self.npe_throw_labels);
        for (action, bci, label) in stubs {
            self.buffer.bind_label(label);
            if bci != usize::MAX {
                self.emit_stamp_throw_bci(bci);
            }
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X0,
                imm: i64::from(action),
            });
            // Cast: a helper address is a real mapped pointer, always < i64::MAX.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X16,
                imm: self.helpers.jit_npe_with_action as i64,
            });
            self.buffer.emit(Arm64Instruction::Blr {
                rn: Arm64Register::X16,
            });
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X0,
                imm: i64::MIN,
            });
            self.buffer.emit(Arm64Instruction::B {
                label: self.epilogue_label,
            });
        }
    }

    /// `CBZ Xobj, npe_<action>` -- the inline null check every receiver
    /// dereference on this backend owes (round 9 wave 12).
    ///
    /// x64 reaches the same place through a `TEST`/`JZ` to a shared per-action
    /// stub. The 64-bit `CBZ` is right for a reference on aarch64 whether or
    /// not compressed oops are on: this backend never materializes a narrow
    /// oop -- every reference it holds is a full pointer, and null is the
    /// all-zero word.
    ///
    /// Callers must only use it where the exception is exact, i.e. under
    /// [`Self::can_throw_npe`].
    fn emit_null_check(&mut self, obj: Arm64Register, action: u8) {
        let throw = self.npe_throw_label(action);
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: obj,
            label: throw,
        });
    }

    /// `idiv`/`irem` (`kind == I32`) and `ldiv`/`lrem` (`I64`), round 9 wave
    /// 11. Only called when [`Self::can_throw_arithmetic`] holds.
    ///
    /// ```text
    ///     CBZ   Wb|Xb, throw      ; JVMS §6.5: a zero divisor throws
    ///     SDIV  Wd|Xd, Wa|Xa, Wb|Xb
    ///     MSUB  Wd|Xd, Wd|Xd, Wb|Xb, Wa|Xa   ; rem only: a - (a/b)*b
    ///     SXTW  Xd, Wd            ; int only: the canonical int form
    /// ```
    ///
    /// AArch64 `SDIV` never traps: the overflow case (`MIN / -1`) yields `MIN`
    /// and the remainder `MSUB` then yields 0, both exactly what JVMS
    /// `idiv`/`irem`/`ldiv`/`lrem` require, so -- unlike x64's `IDIV`, which
    /// faults -- no `-1` guard is owed. The zero test is the whole guard, and
    /// it is what the pre-round-7 lowering lacked (`BRK #1` for division,
    /// nothing at all for remainder, where `SDIV` by zero silently gave 0).
    fn emit_int_div_rem(&mut self, kind: OperandKind, rem: bool) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let throw = self.arith_throw_label();
        let wide = kind == OperandKind::I64;
        self.buffer.emit(if wide {
            Arm64Instruction::Cbz {
                rt: rhs,
                label: throw,
            }
        } else {
            Arm64Instruction::CbzW {
                rt: rhs,
                label: throw,
            }
        });
        // `lhs`/`rhs` are HELD, so the result register is neither.
        let dst = self.alloc_reg(false);
        self.buffer.emit(if wide {
            Arm64Instruction::SDiv {
                rd: dst,
                rn: lhs,
                rm: rhs,
            }
        } else {
            Arm64Instruction::SDivW {
                rd: dst,
                rn: lhs,
                rm: rhs,
            }
        });
        if rem {
            self.buffer.emit(if wide {
                Arm64Instruction::Msub {
                    rd: dst,
                    rn: dst,
                    rm: rhs,
                    ra: lhs,
                }
            } else {
                Arm64Instruction::MsubW {
                    rd: dst,
                    rn: dst,
                    rm: rhs,
                    ra: lhs,
                }
            });
        }
        if !wide {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// Whether this compilation reserves a SAFEPOINT FRAME: the per-local
    /// home slots, the safepoint-id word, and the frame-base publication that
    /// addresses both.
    ///
    /// Two things want one: a method that POLLS, and -- since round 9 wave 18
    /// -- a method that ALLOCATES, because an allocation call stops this frame
    /// inside a callee that can move objects whether or not
    /// `CRATONVM_JIT_ARM64_SAFEPOINTS` is on.
    ///
    /// It is a predicate rather than three copies of the same disjunction
    /// because wave 18 wrote it twice and missed the third: the homes and the
    /// id word were widened to the allocating case, [`Self::emit_frame_record`]
    /// was not, and a frame base that is never published makes both useless --
    /// see that function.
    fn wants_safepoint_frame(&self) -> bool {
        self.safepoints_enabled || self.allocates || self.calls
    }

    /// Publish this frame's base so the GC root walk can find it.
    ///
    /// The safepoint-id slot is read as `[frame_base - sp_id_slot_off]`, and
    /// the runtime learns `frame_base` from `set_top_frame_base`, which it is
    /// told through `helpers.frame_record`. FP is the base this backend
    /// publishes, playing the role x64's RBP does.
    ///
    /// **Round 9 wave 21: gated on [`Self::wants_safepoint_frame`], not on
    /// polls.** Wave 18 gave an allocating method the safepoint homes and the
    /// id word on the argument that an allocation stops the frame inside a
    /// moving callee either way -- and left this one behind. The consequence
    /// was the exact defect that argument was written to prevent, one level
    /// up: the runtime reads the id at `[frame_base - sp_id_slot_off]` and
    /// learns `frame_base` from HERE, so with no frame record
    /// `PreciseFrameInfo::exact_rbp` stays `0`, every precise path
    /// (`scan_active_oop_map_at_rbp`, `verify_precise_covers_conservative`,
    /// `moving_young_osr_method_needs_fallback`'s `exact_rbp != 0`) declines
    /// the frame, and the maps the allocation call had just recorded could
    /// never be read. The method was still CORRECT -- the conservative walk
    /// backs every one of those paths -- but a precise map that nothing can
    /// select is not precision, and the whole point of wave 18 was that an
    /// allocating frame can be stopped in a callee that MOVES objects, which
    /// the conservative walk marks without being able to rewrite.
    ///
    /// Emitted after the arguments are homed and before the entry poll. The
    /// call clobbers X0-X17 and V0-V7, which is sound only because nothing is
    /// on the operand stack yet; a non-empty stack here refuses the method
    /// rather than lose a value across the call. The VM context is already in
    /// its frame word by then (the prologue homes it out of X0), so clobbering
    /// X0 here costs nothing.
    fn emit_frame_record(&mut self) {
        if self.failed || !self.wants_safepoint_frame() || self.helpers.frame_record == 0 {
            return;
        }
        if !self.operand_stack.is_empty() {
            self.failed = true;
            return;
        }
        self.buffer.emit(Arm64Instruction::Mov {
            rd: Arm64Register::X0,
            rm: Arm64Register::FP,
        });
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.frame_record as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        self.frame_base_published = true;
    }

    /// Emit a cooperative GC safepoint poll.
    ///
    /// ```text
    ///     MOVZ/MOVK X16, #safepoint_flag_addr
    ///     LDRB      W17, [X16]          ; ONE byte -- the flag is an AtomicBool
    ///     CBZ       X17, skip           ; clear -> no safepoint requested
    ///     <store every register-located operand, GPR and FP, to its slot>
    ///     <store register-homed reference locals to their homes>
    ///     <stamp the safepoint id>
    ///     MOVZ/MOVK X16, #safepoint_slow_path
    ///     BLR       X16
    ///     <oop map recorded at the return address>
    ///     <reload the locals and the operands>
    ///   skip:
    /// ```
    ///
    /// X16/X17 are IP0/IP1, which AAPCS64 reserves for exactly this and which
    /// hold no operand or local. Java locals live in X19-X28, which the call
    /// preserves. The operand stack lives in X9-X15 and V0-V7, which it does
    /// not -- hence the store and reload. Both classes: this used to spill
    /// only the GPR operands, so a live float or double operand in V0-V7 was
    /// destroyed by every taken poll.
    ///
    /// # Why the stores and reloads sit INSIDE the branch
    ///
    /// They leave the compile-time model exactly as it was: an operand that was
    /// in a register is in the same register again on both paths, so the model
    /// and the two runtime paths agree without a merge.
    ///
    /// # What the GC sees
    ///
    /// The map, recorded at the BLR's return address, names every reference
    /// operand (now in its depth slot) and every reference LOCAL: a frame-homed
    /// one where it lives, and a register-homed one at the home it was just
    /// stored to. Register homes are callee-saved, so the value would survive
    /// on its own -- but inside the CALLEE's save area, where only a
    /// conservative walk sees it, and a conservative walk cannot rewrite a
    /// relocated object. The reload afterwards is what carries a moved
    /// object's new address back into the register.
    fn emit_safepoint_poll(&mut self, entry: bool) {
        if self.failed || !self.safepoints_enabled {
            return;
        }
        // The "not wired" contract, identical to x64's.
        if self.helpers.safepoint_flag_addr == 0 || self.helpers.safepoint_slow_path == 0 {
            return;
        }
        // THE SAFEPOINT ID: the site's bci, or the synthetic `ENTRY_POLL_BC_PC`
        // for the method-entry poll (bci 0 is a legal site of its own).
        let safepoint_id = if entry {
            crate::x64::safepoint::ENTRY_POLL_BC_PC as u32
        } else {
            match u32::try_from(self.cur_bytecode_pc) {
                Ok(n) => n,
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        };
        let skip = self.buffer.new_label();
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.safepoint_flag_addr as i64,
        });
        self.buffer.emit(Arm64Instruction::Ldrb {
            rt: Arm64Register::X17,
            rn: Arm64Register::X16,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: Arm64Register::X17,
            label: skip,
        });

        // Store every register-located operand for the call. An operand that
        // is already in its slot needs nothing; the map writer names it.
        let Some(stored) = self.spill_operands_for_call() else {
            return;
        };
        // NAME THE REFERENCE LOCALS.
        let Some(reg_homed) = self.home_reference_locals_for_call(entry) else {
            return;
        };
        // Stamp the id BEFORE the call, so a collector that stops this thread
        // inside the slow path reads the site it is actually standing at.
        self.stamp_safepoint_id(safepoint_id);

        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.safepoint_slow_path as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        self.emit_oop_map_for_safepoint(safepoint_id);
        self.reload_after_safepoint(&reg_homed, stored);
        self.buffer.bind_label(skip);
    }

    /// Store every register-located operand to its depth slot, and record the
    /// frame offset of each reference one for the map.
    ///
    /// Shared by [`Self::emit_safepoint_poll`] and
    /// [`Self::emit_helper_call_at_safepoint`] (round 9 wave 18) — one copy of
    /// a sequence whose two halves (the store and the matching reload) have to
    /// agree, rather than two copies that can drift.
    ///
    /// The operand MODEL is left untouched: each entry still says `Reg`,
    /// because [`Self::reload_after_safepoint`] puts the value back in that
    /// same register. `None` (with `failed` set) when a live operand has no
    /// slot, which would leave it in a caller-saved register across a call.
    fn spill_operands_for_call(&mut self) -> Option<Vec<(Arm64Register, OperandKind, i32)>> {
        let mut stored: Vec<(Arm64Register, OperandKind, i32)> = Vec::new();
        for depth in 0..self.operand_stack.len() {
            let operand = self.operand_stack[depth];
            let OperandLoc::Reg(reg) = operand.loc else {
                continue;
            };
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                self.failed = true;
                return None;
            };
            self.emit_store_kind(reg, operand.kind, offset);
            if operand.oop {
                self.pending_operand_oop_slots.push(offset);
            }
            stored.push((reg, operand.kind, offset));
        }
        Some(stored)
    }

    /// Put every reference LOCAL where the collector can read and rewrite it,
    /// and record its frame offset for the map.
    ///
    /// A register-homed one is stored to its safepoint home and returned, so
    /// the caller can reload it after the call: the home is what a relocating
    /// collector rewrites, and without the reload the register keeps the old
    /// address. A frame-homed one is already in the right place and only has
    /// to be named.
    ///
    /// `None` (with `failed` set) when a live reference local has no home,
    /// which would leave it reachable only through a conservative scan — and a
    /// conservative scan marks but cannot rewrite.
    fn home_reference_locals_for_call(&mut self, entry: bool) -> Option<Vec<(usize, i32)>> {
        let mut reg_homed: Vec<(usize, i32)> = Vec::new();
        let claim = self.oop_locals_at_current_pc(entry);
        if claim.is_none() && !self.local_regs.is_empty() {
            // The dataflow could not answer for this site (an unreached pc, or
            // more than 64 locals). Sound only while a conservative scan still
            // runs, so this method may not claim full coverage.
            self.pending_map_incomplete = true;
        }
        if let Some(mut mask) = claim {
            while mask != 0 {
                // Cast: count/index to usize
                let i = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                if i >= self.local_regs.len() {
                    continue;
                }
                if let Some(reg) = self.local_regs.get(i).copied().flatten() {
                    let Some(off) = self.safepoint_home_for_reg_local(i) else {
                        self.failed = true;
                        return None;
                    };
                    self.buffer.emit(Arm64Instruction::Str {
                        rt: reg,
                        rn: Arm64Register::FP,
                        offset: off,
                    });
                    self.pending_local_oop_slots.push(off);
                    reg_homed.push((i, off));
                } else {
                    // Frame-homed: already where the GC can read and rewrite
                    // it. At the ENTRY poll that includes frame-homed reference
                    // PARAMETERS, which the argument homing stored before this
                    // poll runs.
                    let Some(off) = self.local_slot_offset(i) else {
                        self.failed = true;
                        return None;
                    };
                    self.pending_local_oop_slots.push(off);
                }
            }
        }
        Some(reg_homed)
    }

    /// Stamp `id` into the frame's safepoint-id word, so a collector that
    /// stops this thread inside the callee reads the site it is standing at.
    fn stamp_safepoint_id(&mut self, id: u32) {
        if self.sp_id_slot_off == 0 {
            return;
        }
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X17,
            imm: i64::from(id),
        });
        self.buffer.emit(Arm64Instruction::Str {
            rt: Arm64Register::X17,
            rn: Arm64Register::FP,
            offset: -self.sp_id_slot_off,
        });
    }

    /// Reload what [`Self::home_reference_locals_for_call`] and
    /// [`Self::spill_operands_for_call`] put in the frame.
    ///
    /// The locals first, and the reason is the whole point of homing them: a
    /// relocating collector rewrote the HOME, and this is what carries the
    /// moved object's new address back into the register.
    fn reload_after_safepoint(
        &mut self,
        reg_homed: &[(usize, i32)],
        stored: Vec<(Arm64Register, OperandKind, i32)>,
    ) {
        for (i, off) in reg_homed {
            if let Some(reg) = self.local_regs.get(*i).copied().flatten() {
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: reg,
                    rn: Arm64Register::FP,
                    offset: *off,
                });
            }
        }
        for (reg, kind, offset) in stored {
            self.emit_load_kind(reg, kind, offset);
        }
    }

    /// The oop-local mask in force at the safepoint being emitted, or `None`
    /// when no claim can be made.
    ///
    /// `None` is a REFUSAL, not "no oop locals": the dataflow is empty above 64
    /// locals and unreached at pcs only an exception edge can arrive at.
    fn oop_locals_at_current_pc(&self, entry: bool) -> Option<u64> {
        if entry {
            // The entry poll runs before the walk; the live oops there are
            // exactly the reference parameters.
            return Some(self.param_oop_mask);
        }
        if self.local_oop_masks.is_empty() {
            return None;
        }
        if !self
            .local_oop_reached
            .get(self.cur_bytecode_pc)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        self.local_oop_masks.get(self.cur_bytecode_pc).copied()
    }

    /// Frame offset of the safepoint home reserved for register-homed local
    /// `index`, or `None` if it has no register or no home was reserved.
    ///
    /// `k` numbers the register-homed locals in order; where that word sits is
    /// [`Arm64SpillArea::safepoint_home_word`]'s business, not this function's.
    /// A `k` past the reserved homes refuses rather than walking off the end of
    /// its region into the safepoint-id word.
    fn safepoint_home_for_reg_local(&self, index: usize) -> Option<i32> {
        self.local_regs.get(index).copied().flatten()?;
        let k = (0..index)
            .filter(|&i| self.local_regs.get(i).copied().flatten().is_some())
            .count();
        self.spill_word_offset(self.spill_area.safepoint_home_word(k))
    }

    /// Frame offset of the word the prologue homed the VM context pointer
    /// into, or `None` when this compilation does not take a context.
    fn context_slot_offset(&self) -> Option<i32> {
        self.spill_word_offset(self.spill_area.context_word())
    }

    /// Frame offset of the slot reserved for operand-stack depth `depth`, or
    /// `None` when `depth` is outside the operand area.
    ///
    /// `max_stack` counts JVM slots and every entry takes at least one, so
    /// every legal depth has a slot. A depth at or past `max_stack` would land
    /// on a safepoint home or the id word, so
    /// [`Arm64SpillArea::operand_word`] refuses it — bounded against the
    /// operand region, not against the grand total, which is the distinction
    /// that makes the refusal mean anything.
    fn spill_offset_for_depth(&self, depth: usize) -> Option<i32> {
        self.spill_word_offset(self.spill_area.operand_word(depth))
    }

    /// Run the shared operand-stack kind analysis over `bytecode`.
    ///
    /// `static_fields` types each resolved `getstatic` (round 9 wave 10), so a
    /// pc after one is still described; an unresolved one leaves its
    /// successors `Unknown`, which is harmless because that site refuses the
    /// method anyway.
    fn analyze_stack_kinds(
        bytecode: &[u8],
        static_fields: &HashMap<usize, Arm64StaticField>,
        instance_fields: &HashMap<usize, Arm64InstanceField>,
        invoke_sites: &HashMap<usize, Arm64InvokeSite>,
    ) -> crate::x64::stack_kinds::StackKindMap {
        use crate::x64::stack_kinds::{analyze, StackKindInputs};
        let refs = rustc_hash::FxHashSet::default();
        let inputs = StackKindInputs {
            // Round 9 wave 16: without these the analysis answers `None` from
            // the first `getfield` onwards (its `0xb4` arm needs the field's
            // type to say what it pushed), so every pc after one would be
            // undescribed -- and `restore_stack_at` would refuse to rebuild
            // the model at any branch target past it.
            field_types: instance_fields
                .iter()
                .map(|(&pc, f)| (pc, f.type_tag))
                .collect(),
            static_types: static_fields
                .iter()
                .map(|(&pc, f)| (pc, f.type_tag))
                .collect(),
            // Round 9 wave 22, and the same argument as the field types
            // above: the `0xb6..=0xba` arm answers `None` for a site it has no
            // shape for, which would leave every pc after the first call
            // undescribed. `calls` wants the PARAMETER count with no
            // receiver -- the opcode says whether there is one -- while
            // `num_jit_args` includes it, so the receiver comes back off here.
            calls: invoke_sites
                .iter()
                .map(|(&pc, site)| {
                    let receiver = usize::from(!matches!(bytecode.get(pc), Some(0xb8) | Some(0xba)));
                    (pc, (site.num_jit_args.saturating_sub(receiver), site.return_type))
                })
                .collect(),
            ldc_refs: &refs,
            ldc_fp: &refs,
            ldc_resolved: &refs,
            handler_pcs: &[],
        };
        analyze(bytecode, bytecode.len(), &inputs)
    }

    /// Whether each of the top `want` entries is category 2, top first, or
    /// `None` when the stack holds fewer entries.
    ///
    /// The stack shuffles are specified in JVM SLOTS and categories, and this
    /// model has one entry per VALUE, typed. So each shuffle arm resolves its
    /// JVMS form from these categories, and a form the JVMS does not define
    /// (a `dup` of a `long`, say) refuses the method.
    fn top_categories(&self, want: usize) -> Option<Vec<bool>> {
        let n = self.operand_stack.len();
        if n < want {
            return None;
        }
        Some(
            self.operand_stack[n - want..]
                .iter()
                .rev()
                .map(|o| o.kind.is_category2())
                .collect(),
        )
    }

    /// A fresh register holding a copy of the `kind` value in `reg`.
    fn copy_value(&mut self, reg: Arm64Register, kind: OperandKind) -> Arm64Register {
        let dst = self.alloc_reg(kind.is_fp());
        let inst = match kind {
            OperandKind::F32 => Arm64Instruction::FmovFpSingle { vd: dst, vn: reg },
            OperandKind::F64 => Arm64Instruction::FmovFp { vd: dst, vn: reg },
            _ => Arm64Instruction::Mov { rd: dst, rm: reg },
        };
        self.buffer.emit(inst);
        dst
    }

    /// The shared duplicate shuffle, `[under.., group..] -> [group'.., under..,
    /// group..]`, counted in ENTRIES. Every `dup*` form is one of these once
    /// its categories are resolved: `dup` is (1, 0), `dup_x1` (1, 1), `dup2` of
    /// two category-1 values (2, 0), and so on.
    fn emit_dup_group_over(&mut self, group_entries: usize, under_entries: usize) {
        let mut group = Vec::with_capacity(group_entries);
        for _ in 0..group_entries {
            match self.pop_entry() {
                Some(e) => group.push(e),
                None => return,
            }
        }
        let mut under = Vec::with_capacity(under_entries);
        for _ in 0..under_entries {
            match self.pop_entry() {
                Some(e) => under.push(e),
                None => return,
            }
        }
        let copies: Vec<Arm64Register> = group
            .iter()
            .map(|&(reg, e)| self.copy_value(reg, e.kind))
            .collect();
        for (&copy, &(_, e)) in copies.iter().zip(group.iter()).rev() {
            self.push_like(e, copy);
        }
        for &(reg, e) in under.iter().rev() {
            self.push_like(e, reg);
        }
        for &(reg, e) in group.iter().rev() {
            self.push_like(e, reg);
        }
    }

    /// Get or create a label for a bytecode PC.
    ///
    /// Also the single chokepoint where a **loop back-edge** is detected: every
    /// branch target on this backend -- `goto`, `if*`, and every switch case and
    /// default -- is resolved through here, so a target at or before the
    /// instruction being lowered is exactly the set of back-edges.
    ///
    /// A compiled loop needs a safepoint poll: without one a thread inside it
    /// never observes a stop-the-world request and any GC that needs to stop
    /// it hangs the VM. With polls on (`CRATONVM_JIT_ARM64_SAFEPOINTS`) the
    /// target is RECORDED and pass 2 emits a poll at the loop header. With
    /// polls off there is nothing to put there, so the method is refused and
    /// interpreted -- deliberately for provably terminating loops too, because
    /// the property that matters is bounded time to the next safepoint.
    fn label_for_pc(&mut self, pc: usize) -> u32 {
        if pc <= self.cur_bytecode_pc {
            if self.safepoints_enabled {
                self.back_edge_targets.insert(pc);
            } else {
                self.failed = true;
            }
        }
        self.label_for_pc_unchecked(pc)
    }

    /// Label allocation without the back-edge check.
    ///
    /// Used only by the walk loop's pre-seed step, which binds a label at a PC
    /// the discovery pass already identified as a branch target. That step is
    /// not itself a branch (and at `pc == 0` the check would misfire).
    fn label_for_pc_unchecked(&mut self, pc: usize) -> u32 {
        if let Some(&label) = self.pc_labels.get(&pc) {
            label
        } else {
            let label = self.buffer.new_label();
            self.pc_labels.insert(pc, label);
            label
        }
    }

    // -- Prologue / Epilogue ------------------------------------------------

    /// Emit the AAPCS64 prologue.
    ///
    /// ```text
    ///     STP  X29, X30, [SP, #-16]!   ; the frame record
    ///     ADD  X29, SP, #0             ; FP -> the record: [FP] = caller FP, [FP+8] = LR
    ///     <stack bang: SUB X16, SP, #off; STR XZR, [X16] per page crossed>
    ///     SUB  SP, SP, #(frame_size - 16)
    ///     STP/STR callee-saved GPRs at [FP - 8 ...]
    ///     <stamp the safepoint-id slot unset>
    /// ```
    ///
    /// # The frame record is the standard one now
    ///
    /// The previous prologue set FP to the caller's SP, leaving the saved pair
    /// at `[FP-16]`/`[FP-8]`. AAPCS64 (and Darwin, and every unwinder, and the
    /// frame-pointer walk in `vm/src/jit/helpers.rs`) expects FP to point AT the
    /// record: `[FP]` = caller's FP, `[FP+8]` = LR. A walk through one of these
    /// frames read the caller's LR as its FP. Every FP-relative offset is
    /// rebased accordingly in `Arm64FrameLayout::compute`.
    ///
    /// `ADD X29, SP, #0` and not `MOV X29, SP`: `Mov` lowers to `ORR`, where
    /// register 31 is XZR, so it would set FP to zero.
    ///
    /// # The stack bang
    ///
    /// Before SP moves, every page the frame will cross is touched, so stack
    /// exhaustion faults ON the guard page (recoverable) instead of a large
    /// `SUB SP` stepping clean past it into unrelated memory. x64 does the same
    /// (`emit_stack_bang_before_frame_alloc`). This replaces the old refusal
    /// of every frame of 4096 bytes or more.
    fn emit_prologue(&mut self) {
        let Some(frame) = self.frame.as_ref() else {
            self.failed = true;
            return;
        };
        let frame_size = frame.frame_size;
        let callee_save_offset = frame.callee_save_offset;
        let saved_len = frame.saved_regs.len();

        self.buffer.emit(Arm64Instruction::StpPre {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: -16,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::FP,
            rn: Arm64Register::SP,
            imm: 0,
        });

        let below = frame_size - 16;
        let Some(probes) = stack_bang_probe_offsets(below) else {
            self.failed = true;
            return;
        };
        for off in probes {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::X16,
                rn: Arm64Register::SP,
                imm: off,
            });
            // Register 31 as a store's Rt is XZR: the probe writes zero.
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::XZR,
                rn: Arm64Register::X16,
                offset: 0,
            });
        }
        if below > 0 {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: below,
            });
        }

        // Save callee-saved registers used for locals (in pairs), below FP.
        let mut i = 0;
        while i + 1 < saved_len {
            // Cast: i < 10 (CALLEE_SAVED.len()).
            let offset = callee_save_offset + (i as i32) * 8;
            let frame = self.frame.as_ref().expect("frame present");
            let (rt1, rt2) = (frame.saved_regs[i], frame.saved_regs[i + 1]);
            self.buffer.emit(Arm64Instruction::Stp {
                rt1,
                rt2,
                rn: Arm64Register::FP,
                offset,
            });
            i += 2;
        }
        if i < saved_len {
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            let rt = self.frame.as_ref().expect("frame present").saved_regs[i];
            self.buffer.emit(Arm64Instruction::Str {
                rt,
                rn: Arm64Register::FP,
                offset,
            });
        }

        // STAMP THE SAFEPOINT-ID SLOT with "this frame has not reached a
        // safepoint yet" (`SP_ID_UNSET_BC_PC`, which matches no map). Left
        // uninitialised it could read as a valid id. X17 is not an argument
        // register, so this is safe before the arguments are homed.
        if self.sp_id_slot_off != 0 {
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X17,
                imm: crate::x64::safepoint::SP_ID_UNSET_BC_PC as i64,
            });
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::X17,
                rn: Arm64Register::FP,
                offset: -self.sp_id_slot_off,
            });
        }
        // HOME THE VM CONTEXT POINTER (round 9 wave 16), which arrives in X0
        // ahead of every Java argument. It has to reach its frame word before
        // anything can clobber X0 -- the entry safepoint poll's `BLR` would,
        // and so would a `getfield`'s own helper call. Nothing above touches
        // X0: the stack bang and the sp-id stamp use X16/X17, and the
        // callee-saved saves read X19-X28.
        if self.needs_context {
            let Some(off) = self.context_slot_offset() else {
                // `needs_context` and the frame's context word are set from
                // the same field, so this is unreachable; refuse rather than
                // silently compile a body that reads an unwritten word.
                self.failed = true;
                return;
            };
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::X0,
                rn: Arm64Register::FP,
                offset: off,
            });
        }

        // The method-entry poll is NOT emitted here: X0-X7 still hold the
        // arguments until `emit_argument_homing`, and the poll's call may
        // destroy them. See `the_entry_poll_runs_after_the_argument_copy`.
    }

    /// Deposit each incoming argument in its JVM local's home.
    ///
    /// The VM passes one 64-bit register per ARGUMENT (`this` first), and an
    /// `int` arrives sign-extended, a `float` as its zero-extended bit pattern
    /// and a `double` as its bits. Argument `i` goes to JVM local
    /// `arg_slots[i]`, which differs from `i` after any `long`/`double`
    /// argument. A register home gets a `MOV`; a frame home gets a `STR` of
    /// the whole word, which the local's later loads read at their own width.
    ///
    /// The previous prologue moved X_i into local i's register and did nothing
    /// for a frame-homed local -- which every `float`/`double` local is, since
    /// the allocator never gives one a GPR -- so every FP parameter, and every
    /// parameter after a `long`/`double`, read garbage.
    fn emit_argument_homing(&mut self, arg_slots: &[usize]) {
        // THE CONTEXT OCCUPIES X0 (round 9 wave 16), so every Java argument
        // moves up one register and only seven fit. A body that reads its
        // first argument from X0 while the VM passed a context there reads the
        // context pointer as that argument, which is why this shift and
        // `Arm64CompileResult::needs_context` are driven by the same field.
        let shift = usize::from(self.needs_context);
        if arg_slots.len() + shift > Arm64EntryConvention::INT_ARG_REGS.len() {
            // Arguments past the eighth arrive on the stack, which this
            // prologue does not read.
            self.failed = true;
            return;
        }
        for (i, &slot) in arg_slots.iter().enumerate() {
            let arg = Arm64EntryConvention::INT_ARG_REGS[i + shift];
            match self.local_regs.get(slot).copied() {
                Some(Some(home)) => {
                    if home != arg {
                        self.buffer
                            .emit(Arm64Instruction::Mov { rd: home, rm: arg });
                    }
                }
                Some(None) => {
                    let Some(offset) = self.local_slot_offset(slot) else {
                        self.failed = true;
                        return;
                    };
                    self.buffer.emit(Arm64Instruction::Str {
                        rt: arg,
                        rn: Arm64Register::FP,
                        offset,
                    });
                }
                // A parameter slot past `max_locals`: a malformed method.
                None => {
                    self.failed = true;
                    return;
                }
            }
        }
    }

    /// Emit the epilogue: restore the callee-saved GPRs, `ADD SP, X29, #0`
    /// (again not `MOV`, for the same register-31 reason), `LDP X29, X30,
    /// [SP], #16`, `RET`.
    fn emit_epilogue(&mut self) {
        let Some(frame) = self.frame.as_ref() else {
            self.failed = true;
            return;
        };
        let callee_save_offset = frame.callee_save_offset;
        let saved = frame.saved_regs.clone();

        self.buffer.bind_label(self.epilogue_label);

        let mut i = 0;
        while i + 1 < saved.len() {
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Ldp {
                rt1: saved[i],
                rt2: saved[i + 1],
                rn: Arm64Register::FP,
                offset,
            });
            i += 2;
        }
        if i < saved.len() {
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Ldr {
                rt: saved[i],
                rn: Arm64Register::FP,
                offset,
            });
        }

        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::FP,
            imm: 0,
        });
        self.buffer.emit(Arm64Instruction::LdpPost {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: 16,
        });
        self.buffer.emit(Arm64Instruction::Ret);
    }

    // -- Bytecode compilation -----------------------------------------------

    /// Compile a JVM bytecode method to ARM64 instructions.
    ///
    /// `method_info` maps constant pool indices (from invokestatic operands) to
    /// the number of arguments the target method expects.
    pub fn compile_method(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
    ) -> Arm64CompileResult {
        self.compile_method_with_info(num_locals, num_params, max_stack, bytecode, HashMap::new())
    }

    /// Like [`compile_method`] but accepts an explicit method-info map for invoke
    /// resolution.
    ///
    /// Runs [`Arm64Backend::compile_pass`] **twice**. Branch targets are
    /// discovered as each branch is decoded, which is too late for a BACKWARD
    /// branch: its target was walked past before the label existed. Pass 1
    /// discovers every target (and every target's operand-stack shape, and
    /// every loop header); pass 2 re-walks with them in hand.
    pub fn compile_method_with_info(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
    ) -> Arm64CompileResult {
        // Discovered by pass 1 and read by pass 2, so `compile_pass` must not
        // clear them; cleared here, per compile.
        self.back_edge_targets.clear();
        self.label_states.clear();

        drop(self.compile_pass(
            num_locals,
            num_params,
            max_stack,
            bytecode,
            method_info.clone(),
            &[],
        ));
        let mut branch_targets: Vec<usize> = self.pc_labels.keys().copied().collect();
        branch_targets.sort_unstable();
        // `failed` is sticky and intentionally NOT cleared between the passes.

        self.compile_pass(
            num_locals,
            num_params,
            max_stack,
            bytecode,
            method_info,
            &branch_targets,
        )
    }

    /// One walk of the bytecode. See [`compile_method_with_info`] for why this
    /// runs twice and what `branch_targets` carries between the passes.
    fn compile_pass(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
        branch_targets: &[usize],
    ) -> Arm64CompileResult {
        // Reset state.
        self.buffer = Arm64CodeBuffer::new();
        self.operand_stack.clear();
        self.held.clear();
        self.reachable = true;
        self.pc_labels.clear();
        self.cur_bytecode_pc = 0;
        self.local_regs.clear();
        self.float_local_regs.clear();
        self.num_params = num_params;
        self.method_info = method_info;
        self.stack_kinds =
            Self::analyze_stack_kinds(
                bytecode,
                &self.static_fields,
                &self.instance_fields,
                &self.invoke_sites,
            );
        // THE CONTEXT DECISION, made from the bytecode alone and made here.
        //
        // It has to be identical in both passes and it has to be known before
        // the frame is sized, because the context occupies a frame word and
        // shifts every argument register by one. Deriving it from the bytecode
        // -- "does this method contain a `getfield`?" -- gives both for free.
        // Deriving it from what the walk actually lowered would not: pass 1
        // and pass 2 could disagree, and the frame would already be built.
        //
        // A `getfield` that then REFUSES costs an unused frame word in a
        // method that is discarded anyway, which is the harmless direction.
        // A call that can SAFEPOINT: the three allocations (round 9 wave 18)
        // and the two REFERENCE array element accesses (wave 19), whose
        // helpers read and write a slot whose encoding only they know.
        // A CALL is a safepoint too (round 9 wave 22). `allocates` is read
        // for one thing -- whether this frame can be stopped inside a callee
        // that moves objects -- and an `invoke*` can be stopped inside
        // anything at all. The name is now narrower than the question it
        // answers; `wants_safepoint_frame` is the question.
        // ...and a type check, which resolves its target on demand and
        // allocates a `java/lang/Class` mirror doing it (round 9 wave 23).
        //
        // h23: ...and a MONITOR op, which is the most safepoint-capable call
        // on this backend -- `jit_monitor_enter`'s contended path PARKS this
        // thread, so the frame can sit inside it for an entire collection.
        // Without this term `wants_safepoint_frame` answers `false` for a
        // body whose only helper call is the lock, no safepoint-id word is
        // reserved, and `emit_helper_call_at_safepoint` refuses the method --
        // which is how `emit_monitor_op` first failed. A BYTE scan is enough
        // and errs the right way: a `0xc2`/`0xc3` that is some other
        // instruction's operand only reserves frame words nothing reads,
        // where MISSING one refuses a method that should have compiled.
        let has_monitor_op = bytecode.iter().any(|&b| b == 0xc2 || b == 0xc3);
        // h23c: `multianewarray_n` takes the VM pointer and can collect, so a
        // `0xc5` needs both the context word and a safepoint frame. Same byte
        // scan, erring the same (safe) way as the monitor one above.
        let has_multianewarray = bytecode.contains(&0xc5);
        self.calls = !invoke_sites(bytecode).is_empty()
            || !typecheck_sites(bytecode).is_empty()
            || has_monitor_op
            || has_multianewarray;
        self.allocates = !allocation_sites(bytecode).is_empty()
            || any_opcode(bytecode, |op| matches!(op, 0x32 | 0x53));
        // ...and the VM context, which every one of those helpers takes, as
        // does `getfield`, `putstatic`, a REFERENCE `putfield` and both
        // monitor ops. The reference `putfield` is the only one that cannot
        // be read off the opcode: the resolved site table answers it, and it
        // is fixed before either pass runs, so both passes decide the same
        // way.
        let stores_a_reference_field = instance_field_sites(bytecode).iter().any(|&(pc, op, _)| {
            op == 0xb5
                && self
                    .instance_fields
                    .get(&pc)
                    .is_some_and(|f| matches!(f.type_tag, b'L' | b'['))
        });
        self.needs_context = self.allocates
            // Every `invoke*` passes the VM pointer to `jit_invoke_dispatch`.
            || self.calls
            || stores_a_reference_field
            || has_monitor_op
            || has_multianewarray
            || instance_field_sites(bytecode)
                .iter()
                .any(|&(_, op, _)| op == 0xb4)
            || static_field_sites(bytecode)
                .iter()
                .any(|&(_, op, _)| op == 0xb3);
        self.static_init_classes.clear();
        // Label ids of the PREVIOUS pass's buffer; this pass's buffer was
        // just replaced, so they would name nothing (or something else).
        // `npe_throw_labels` carried this same gap before it gained a bci key
        // (round 9 wave 11 cleared only the arithmetic stub's label) -- worth
        // closing now rather than leaving more labels able to go stale.
        self.arith_throw_labels.clear();
        self.npe_throw_labels.clear();
        self.pending_local_oop_slots.clear();
        self.pending_operand_oop_slots.clear();
        self.safepoint_count = 0;
        self.incomplete_oop_maps = 0;
        self.pending_map_incomplete = false;
        // The same "must be oop" local dataflow x64 uses, seeded with this
        // method's reference parameters.
        let (lo_masks, lo_reached) = crate::x64::compute_local_oop_masks(
            bytecode,
            bytecode.len(),
            num_locals,
            self.param_oop_mask,
        );
        self.local_oop_masks = lo_masks;
        self.local_oop_reached = lo_reached;

        // The JVM local slot of each incoming argument.
        let arg_slots: Vec<usize> = match &self.param_jvm_slots {
            Some(slots) => slots.clone(),
            None => (0..num_params).collect(),
        };

        // Graph-coloring register allocation, told where the parameters really
        // are: its liveness seeds them live-on-entry, and seeding the identity
        // layout left a parameter after a `long`/`double` dead on entry, free to
        // share a register with a live one.
        let alloc = super::regalloc::allocate_registers_arm64_with_param_slots(
            bytecode,
            bytecode.len(),
            num_locals,
            num_params,
            &arg_slots,
            &[],
        );

        for &a in &alloc.assignments {
            self.local_regs.push(a.map(Arm64Register));
        }
        while self.local_regs.len() < num_locals {
            self.local_regs.push(None);
        }

        // Float/double locals get NO dedicated FP register on this backend.
        // `regalloc::ARM64_LOCAL_FPS` is D8-D15, which AAPCS64 makes callee-saved,
        // and this prologue saves only GPRs -- homing a float local there
        // destroyed the caller's copy (aarch64 parity audit, 2026-08-01). They
        // live in frame slots. If FP homing is wanted back, the prerequisite is
        // an FP save area in `Arm64FrameLayout::compute` driven by
        // `alloc.used_xmm_regs`. Asserted by
        // `float_locals_never_use_callee_saved_fp_regs`.
        for _ in 0..num_locals {
            self.float_local_regs.push(None);
        }

        let saved_regs: Vec<Arm64Register> = alloc
            .used_callee_saved
            .iter()
            .map(|&n| Arm64Register(n))
            .collect();

        // Frame words: the frame-homed locals, the operand area, one safepoint
        // home per register-homed local and the safepoint-id word (the last two
        // only when this compilation polls). The partition is stated ONCE, here,
        // and every word index below `spill_offset` is derived from it — see
        // [`Arm64SpillArea`] for why it is a type and not four additions.
        let safepoint_frame = self.wants_safepoint_frame();
        let area = Arm64SpillArea {
            locals: self.local_spill_count(),
            operands: max_stack,
            // The safepoint region is reserved for a method that POLLS or a
            // method that ALLOCATES: both stop this frame inside a callee that
            // can move objects, and both therefore need somewhere to put the
            // register-homed reference locals and an id the collector can
            // select their map by.
            //
            // One word per register-homed LOCAL, which is what
            // `safepoint_home_for_reg_local` indexes by -- NOT `saved_regs.len()`,
            // which is how many distinct callee-saved REGISTERS the allocator
            // used. The two differ whenever two locals with disjoint live
            // ranges share one register, and then the second local's `k` runs
            // off the end of the region and `safepoint_home_word` answers
            // `None`: the poll (or, since wave 18, the allocation call)
            // refuses the method rather than home it. Found by the first test
            // to put two reference locals in one register across a call.
            safepoint_homes: if safepoint_frame {
                self.local_regs.iter().filter(|r| r.is_some()).count()
            } else {
                0
            },
            sp_id: usize::from(safepoint_frame),
            context: usize::from(self.needs_context),
        };
        self.install_frame(num_locals, area, &saved_regs);
        // The sp-id word sits past the locals, the operand area and the homes —
        // which is what `sp_id_word()` says, rather than "the last word", which
        // is what `num_spills - 1` said and which is only the same answer while
        // the id word happens to be laid out last.
        self.sp_id_slot_off = match self.spill_word_offset(self.spill_area.sp_id_word()) {
            // The runtime reads `[frame_base - off]`, so publish the magnitude.
            Some(off) => -off,
            None => 0,
        };

        self.epilogue_label = self.buffer.new_label();

        self.emit_prologue();
        self.emit_argument_homing(&arg_slots);
        // Publish the frame base BEFORE the first poll stamps an id into it.
        self.emit_frame_record();
        // METHOD-ENTRY SAFEPOINT POLL, after the arguments are homed: its call
        // may destroy X0-X7.
        self.emit_safepoint_poll(true);

        let mut pc = 0;
        let mut success = true;
        while pc < bytecode.len() {
            // Pre-seed this PC's label if the discovery pass saw a branch to it.
            if branch_targets.binary_search(&pc).is_ok() {
                let _ = self.label_for_pc_unchecked(pc);
            }
            self.held.clear();

            if let Some(&label) = self.pc_labels.get(&pc) {
                if self.reachable {
                    self.arrive_at_target(pc);
                } else {
                    self.restore_stack_at(pc);
                }
                if !self.buffer.labels.contains_key(&label) {
                    self.buffer.bind_label(label);
                }
            } else if !self.reachable {
                self.restore_stack_at(pc);
            }
            self.reachable = true;

            let opcode = bytecode[pc];
            let start_pc = pc;

            // THE SHARED-MEMORY GATE. Consulted before the dispatch below, so
            // no lowering arm can be reached for an opcode that touches memory
            // another thread observes unless that arm orders its own accesses.
            // The refusal used to be an ACCIDENT of which arms happen to be
            // written -- invisible to anyone about to write one, and so the
            // first `getfield` arm would have lowered a `volatile` read to a
            // plain `LDR`, which reviews as correct against an x86-first
            // mental model and is wrong on every ARM core.
            //
            // `opcode_has_ordered_lowering` is the whole permission since
            // round 9 wave 14: a per-opcode list, each entry a reviewed claim
            // about ONE lowering, and each listed arm still refuses any site
            // it was handed nothing for. There is deliberately no blanket
            // constant in front of it any more -- see
            // `ARM64_LOWERS_ACQUIRE_RELEASE` for what that one was and why a
            // single `bool` was both too weak and unreachable.
            if opcode_touches_shared_memory(opcode) && !opcode_has_ordered_lowering(opcode) {
                self.buffer.emit(Arm64Instruction::Comment(format!(
                    "opcode 0x{opcode:02x} at pc={start_pc} touches shared memory, and this \
                     backend emits no LDAR/STLR/DMB — bailing to interpreter"
                )));
                success = false;
                break;
            }
            // Published before the loop-header poll (whose id and oop-local mask
            // are this pc's) and before lowering (so `label_for_pc` can tell a
            // back-edge from a forward branch).
            self.cur_bytecode_pc = start_pc;

            // LOOP-HEADER SAFEPOINT POLL, after the label so a back edge lands on
            // it -- one poll per header regardless of how many branches target it.
            if self.back_edge_targets.contains(&pc) {
                self.emit_safepoint_poll(false);
            }
            pc += 1;

            match opcode {
                // nop
                0x00 => self.buffer.emit(Arm64Instruction::Nop),
                // aconst_null
                0x01 => {
                    let dst = self.alloc_reg(false);
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
                    self.push_reg(OperandKind::Ref, dst);
                }
                // iconst_m1 .. iconst_5
                0x02..=0x08 => self.emit_iconst(i32::from(opcode) - 3),
                // lconst_0, lconst_1
                0x09 | 0x0a => self.emit_lconst(i64::from(opcode - 0x09)),
                // fconst_0 .. fconst_2
                0x0b..=0x0d => self.emit_fconst(f32::from(opcode - 0x0b)),
                // dconst_0, dconst_1
                0x0e | 0x0f => self.emit_dconst(f64::from(opcode - 0x0e)),
                // bipush
                0x10 => {
                    let Some(v) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    // Cast: bipush's operand is a signed byte.
                    self.emit_iconst(i32::from(v as i8));
                }
                // sipush
                0x11 => {
                    let Some(v) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    self.emit_iconst(i32::from(v));
                }
                // ldc / ldc_w / ldc2_w: this backend is never handed the
                // constant pool, so it cannot recover the constant. Refuse.
                0x12..=0x14 => {
                    self.buffer.emit(Arm64Instruction::Comment(
                        "ldc family: constant pool not available — bailing to interpreter".into(),
                    ));
                    success = false;
                    break;
                }
                // iload lload fload dload aload
                0x15..=0x19 => {
                    let Some(idx) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    self.emit_load_local(usize::from(idx), LOCAL_KINDS[usize::from(opcode - 0x15)]);
                }
                // iload_0 .. aload_3
                0x1a..=0x2d => {
                    let n = opcode - 0x1a;
                    self.emit_load_local(usize::from(n % 4), LOCAL_KINDS[usize::from(n / 4)]);
                }
                // istore lstore fstore dstore astore
                0x36..=0x3a => {
                    let Some(idx) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    self.emit_store_local(
                        usize::from(idx),
                        LOCAL_KINDS[usize::from(opcode - 0x36)],
                    );
                }
                // istore_0 .. astore_3
                0x3b..=0x4e => {
                    let n = opcode - 0x3b;
                    self.emit_store_local(usize::from(n % 4), LOCAL_KINDS[usize::from(n / 4)]);
                }
                // aaload (round 9 wave 19). The ONE array access that goes
                // through a helper, because a reference element is not always
                // a pointer: compressed oops make it a 4-byte scaled offset
                // and an armed ZGC cycle makes it a coloured word. Its guards
                // are still this backend's own.
                0x32 => {
                    if !self.emit_aaload() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array load 0x32 at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // aastore (round 9 wave 19). Two calls at one bytecode: the
                // element-type check, whose verdict a compiled caller can
                // see, and the store itself with its SATB pre-barrier and
                // card mark.
                0x53 => {
                    if !self.emit_aastore() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array store 0x53 at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // iaload .. saload, minus `aaload` above (round 9 wave 13).
                // Past the gate for the same reason `arraylength` is: the
                // lowering orders every access it makes.
                0x2e..=0x35 => {
                    if !self.emit_array_load(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array load 0x{opcode:02x} at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // iastore .. sastore, minus `aastore` above (round 9 wave 13).
                0x4f..=0x56 => {
                    if !self.emit_array_store(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array store 0x{opcode:02x} at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // pop: one category-1 value.
                0x57 => match self.top_categories(1) {
                    Some(c) if !c[0] => self.drop_top(),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // pop2: one category-2 value, or two category-1 values.
                0x58 => match self.top_categories(1) {
                    Some(c) if c[0] => self.drop_top(),
                    Some(_) => match self.top_categories(2) {
                        Some(c) if !c[1] => {
                            self.drop_top();
                            self.drop_top();
                        }
                        _ => {
                            success = false;
                            break;
                        }
                    },
                    None => {
                        success = false;
                        break;
                    }
                },
                // dup: category-1 top.
                0x59 => match self.top_categories(1) {
                    Some(c) if !c[0] => self.emit_dup_group_over(1, 0),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // dup_x1: both category-1.
                0x5a => match self.top_categories(2) {
                    Some(c) if !c[0] && !c[1] => self.emit_dup_group_over(1, 1),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // dup_x2: FORM 1 is three category-1 values; FORM 2 a category-1
                // top over one category-2 value.
                0x5b => {
                    let under = match self.top_categories(2) {
                        Some(c) if !c[0] && c[1] => Some(1),
                        Some(c) if !c[0] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some(2),
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(under) = under else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(1, under);
                }
                // dup2: FORM 1 two category-1 values; FORM 2 one category-2.
                0x5c => {
                    let group = match self.top_categories(1) {
                        Some(c) if c[0] => Some(1),
                        Some(_) => match self.top_categories(2) {
                            Some(c) if !c[1] => Some(2),
                            _ => None,
                        },
                        None => None,
                    };
                    let Some(group) = group else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, 0);
                }
                // dup2_x1: FORM 1 two category-1 over one; FORM 2 one category-2
                // over one. The entry underneath is category-1 in both.
                0x5d => {
                    let group = match self.top_categories(2) {
                        Some(c) if c[0] && !c[1] => Some(1),
                        Some(c) if !c[0] && !c[1] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some(2),
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(group) = group else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, 1);
                }
                // dup2_x2, in this backend's one-entry-per-value model:
                //   FORM 4  v1,v2 cat-2   [v2, v1]         -> [v1, v2, v1]
                //   FORM 2  v1 cat-2      [v3, v2, v1]     -> [v1, v3, v2, v1]
                //   FORM 3  v3 cat-2      [v3, v2, v1]     -> [v2, v1, v3, v2, v1]
                //   FORM 1  all cat-1     [v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]
                0x5e => {
                    let shape = match self.top_categories(2) {
                        Some(c) if c[0] && c[1] => Some((1usize, 1usize)),
                        Some(c) if c[0] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some((1, 2)),
                            _ => None,
                        },
                        Some(c) if !c[1] => match self.top_categories(3) {
                            Some(c3) if c3[2] => Some((2, 1)),
                            Some(_) => match self.top_categories(4) {
                                Some(c4) if !c4[3] => Some((2, 2)),
                                _ => None,
                            },
                            None => None,
                        },
                        _ => None,
                    };
                    let Some((group, under)) = shape else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, under);
                }
                // swap: both category-1.
                0x5f => match self.top_categories(2) {
                    Some(c) if !c[0] && !c[1] => {
                        let (Some(v1), Some(v2)) = (self.pop_entry(), self.pop_entry()) else {
                            success = false;
                            break;
                        };
                        self.push_like(v1.1, v1.0);
                        self.push_like(v2.1, v2.0);
                    }
                    _ => {
                        success = false;
                        break;
                    }
                },
                // iadd ladd fadd dadd
                0x60 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::AddW { rd, rn, rm }
                }),
                0x61 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Add { rd, rn, rm }
                }),
                0x62 | 0x63 => self.emit_binary_fp(
                    if opcode == 0x62 {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FaddSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FaddDouble { vd, vn, vm },
                ),
                // isub lsub fsub dsub
                0x64 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::SubW { rd, rn, rm }
                }),
                0x65 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Sub { rd, rn, rm }
                }),
                0x66 | 0x67 => self.emit_binary_fp(
                    if opcode == 0x66 {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FsubSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FsubDouble { vd, vn, vm },
                ),
                // imul lmul fmul dmul
                0x68 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::MulW { rd, rn, rm }
                }),
                0x69 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Mul { rd, rn, rm }
                }),
                0x6a | 0x6b => self.emit_binary_fp(
                    if opcode == 0x6a {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FmulSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FmulDouble { vd, vn, vm },
                ),
                // idiv / ldiv / irem / lrem. Round 9 wave 11: lowered with a
                // zero-divisor guard that branches to the shared
                // `ArithmeticException` stub (`emit_int_div_rem`,
                // `emit_arith_throw_stub`) -- but ONLY when that throw path is
                // wired and exact (`can_throw_arithmetic`: the helper address is
                // set and the caller vouched the exception table is empty).
                // Otherwise refused, as since round 7: the old guard was
                // `BRK #1` (SIGTRAP, which nothing converts), and AArch64 `SDIV`
                // by zero yields 0, so `x % 0` silently returned `x`.
                0x6c | 0x6d | 0x70 | 0x71 => {
                    if !self.can_throw_arithmetic() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "opcode 0x{opcode:02x} at pc={start_pc}: no exact \
                             ArithmeticException path (helper unwired or exception \
                             table not known empty) — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                    let kind = if opcode == 0x6c || opcode == 0x70 {
                        OperandKind::I32
                    } else {
                        OperandKind::I64
                    };
                    self.emit_int_div_rem(kind, opcode >= 0x70);
                }
                // fdiv ddiv
                0x6e | 0x6f => self.emit_binary_fp(
                    if opcode == 0x6e {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FdivSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FdivDouble { vd, vn, vm },
                ),
                // frem / drem -- refused: there is no exact lowering (the
                // truncating FCVTZS round-trip saturates for |a/b| >= 2^63, and
                // Java's remainder is exact across the whole range).
                0x72 | 0x73 => {
                    self.buffer.emit(Arm64Instruction::Comment(
                        "frem/drem: no exact lowering — bailing to interpreter".into(),
                    ));
                    success = false;
                    break;
                }
                // ineg lneg fneg dneg
                0x74 => {
                    let src = self.pop_kind(OperandKind::I32);
                    let dst = self.alloc_reg(false);
                    self.buffer
                        .emit(Arm64Instruction::NegW { rd: dst, rn: src });
                    self.emit_sxtw(dst);
                    self.push_reg(OperandKind::I32, dst);
                }
                0x75 => {
                    let src = self.pop_kind(OperandKind::I64);
                    let dst = self.alloc_reg(false);
                    self.buffer.emit(Arm64Instruction::Neg { rd: dst, rn: src });
                    self.push_reg(OperandKind::I64, dst);
                }
                0x76 => self.emit_float_neg(OperandKind::F32),
                0x77 => self.emit_float_neg(OperandKind::F64),
                // ishl lshl ishr lshr iushr lushr. The W-form variable shifts take
                // the amount MOD 32 and the X forms MOD 64, which is exactly the
                // JVMS `& 0x1f` / `& 0x3f`.
                0x78 => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::LslW {
                    rd,
                    rn,
                    rm,
                }),
                0x79 => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Lsl {
                    rd,
                    rn,
                    rm,
                }),
                0x7a => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::AsrW {
                    rd,
                    rn,
                    rm,
                }),
                0x7b => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Asr {
                    rd,
                    rn,
                    rm,
                }),
                0x7c => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::LsrW {
                    rd,
                    rn,
                    rm,
                }),
                0x7d => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Lsr {
                    rd,
                    rn,
                    rm,
                }),
                // iand land ior lor ixor lxor
                0x7e => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::AndW { rd, rn, rm }
                }),
                0x7f => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::And { rd, rn, rm }
                }),
                0x80 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::OrrW { rd, rn, rm }
                }),
                0x81 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Orr { rd, rn, rm }
                }),
                0x82 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::EorW { rd, rn, rm }
                }),
                0x83 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Eor { rd, rn, rm }
                }),
                // iinc
                0x84 => {
                    let (Some(idx), Some(delta)) = (bc_u8(bytecode, pc), bc_u8(bytecode, pc + 1))
                    else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    // Cast: iinc's constant is a signed byte.
                    self.emit_iinc(usize::from(idx), i32::from(delta as i8));
                }
                // i2l l2i i2f i2d l2f l2d f2i f2l f2d d2i d2l d2f i2b i2c i2s
                0x85..=0x93 => self.emit_conversion(opcode),
                // lcmp
                0x94 => self.emit_lcmp(),
                // fcmpl fcmpg dcmpl dcmpg
                0x95 => self.emit_fcmp(OperandKind::F32, false),
                0x96 => self.emit_fcmp(OperandKind::F32, true),
                0x97 => self.emit_fcmp(OperandKind::F64, false),
                0x98 => self.emit_fcmp(OperandKind::F64, true),
                // ifeq ifne iflt ifge ifgt ifle
                0x99..=0x9e => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = IF_CONDS[usize::from(opcode - 0x99)];
                    self.emit_if_zero(cond, branch_target(start_pc, i32::from(off)));
                }
                // if_icmpeq .. if_icmple
                0x9f..=0xa4 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = IF_CONDS[usize::from(opcode - 0x9f)];
                    self.emit_if_icmp(cond, branch_target(start_pc, i32::from(off)));
                }
                // if_acmpeq if_acmpne
                0xa5 | 0xa6 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = if opcode == 0xa5 {
                        Arm64Condition::Eq
                    } else {
                        Arm64Condition::Ne
                    };
                    self.emit_if_acmp(cond, branch_target(start_pc, i32::from(off)));
                }
                // goto
                0xa7 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let label = self.prepare_branch(branch_target(start_pc, i32::from(off)));
                    self.buffer.emit(Arm64Instruction::B { label });
                    self.reachable = false;
                }
                // tableswitch. The `checked_tableswitch_count` audit rejects
                // adversarial overflow / oversize tables, as on x64.
                0xaa => {
                    let key = self.pop_kind(OperandKind::I32);
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let (Some(default_off), Some(low), Some(high)) = (
                        bc_i32(bytecode, pc),
                        bc_i32(bytecode, pc + 4),
                        bc_i32(bytecode, pc + 8),
                    ) else {
                        success = false;
                        break;
                    };
                    pc += 12;
                    let Some(count) = super::x64::checked_tableswitch_count(low, high) else {
                        success = false;
                        break;
                    };
                    let default = self.prepare_branch(branch_target(start_pc, default_off));
                    let mut targets = Vec::with_capacity(count);
                    for _ in 0..count {
                        let Some(off) = bc_i32(bytecode, pc) else {
                            break;
                        };
                        pc += 4;
                        targets.push(self.prepare_branch(branch_target(start_pc, off)));
                    }
                    if targets.len() != count {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::TableSwitch {
                        key,
                        low,
                        default,
                        targets,
                    });
                    self.reachable = false;
                }
                // lookupswitch, with `checked_lookupswitch_npairs` validation.
                0xab => {
                    let key = self.pop_kind(OperandKind::I32);
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let (Some(default_off), Some(npairs_raw)) =
                        (bc_i32(bytecode, pc), bc_i32(bytecode, pc + 4))
                    else {
                        success = false;
                        break;
                    };
                    pc += 8;
                    let Some(npairs) = super::x64::checked_lookupswitch_npairs(npairs_raw) else {
                        success = false;
                        break;
                    };
                    let default = self.prepare_branch(branch_target(start_pc, default_off));
                    let mut pairs = Vec::with_capacity(npairs);
                    for _ in 0..npairs {
                        let (Some(value), Some(off)) =
                            (bc_i32(bytecode, pc), bc_i32(bytecode, pc + 4))
                        else {
                            break;
                        };
                        pc += 8;
                        pairs.push((value, self.prepare_branch(branch_target(start_pc, off))));
                    }
                    if pairs.len() != npairs {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::LookupSwitch {
                        key,
                        pairs,
                        default,
                    });
                    self.reachable = false;
                }
                // ireturn lreturn freturn dreturn areturn
                0xac..=0xb0 => {
                    const RETURN_KINDS: [OperandKind; 5] = LOCAL_KINDS;
                    self.emit_return_value(RETURN_KINDS[usize::from(opcode - 0xac)]);
                }
                // return
                0xb1 => {
                    self.buffer.emit(Arm64Instruction::B {
                        label: self.epilogue_label,
                    });
                    self.reachable = false;
                }
                // getstatic (round 9 wave 10). The one shared-memory opcode
                // `opcode_has_ordered_lowering` lets past the gate; it lowers
                // only a site the caller resolved (`set_static_field_info`)
                // to a primitive static, and refuses everything else here.
                0xb2 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.static_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getstatic at pc={start_pc}: no resolved static field — \
                             bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_getstatic(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getstatic at pc={start_pc}: type '{}' has no aarch64 \
                             lowering — bailing to interpreter",
                            char::from(field.type_tag)
                        )));
                        success = false;
                        break;
                    }
                }
                // athrow (round 9 wave 20). Past the gate because it
                // publishes nothing: the exception object was published by
                // whatever created it, and this hands the reference to a
                // helper and leaves.
                0xbf => {
                    if !self.emit_athrow() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "athrow at pc={start_pc}: an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // new / newarray / anewarray (round 9 wave 18). Past the
                // gate because their lowering owes no fence and says why: the
                // object is unreachable by construction until a later store
                // publishes it, and that store carries its own ordering.
                // Refused when the site was not resolved, the helper is
                // unwired, or this method has an exception table (the OOM the
                // helper stashes is drained with an unknown pc).
                0xbb | 0xbd => {
                    let Some(operand) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    // Cast: a constant-pool index, which is unsigned.
                    if !self.emit_allocation(opcode, operand as u16) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "allocation at pc={start_pc}: no resolved class, an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                0xbc => {
                    let Some(atype) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    if !self.emit_allocation(opcode, u16::from(atype)) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "allocation at pc={start_pc}: no resolved class, an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // putstatic (round 9 wave 17). Past the gate because its
                // lowering orders its own access: a volatile store gets a
                // `DMB ISH` on both sides, the store itself happening inside a
                // helper that takes the statics write lock. Refused when the
                // site was not resolved (which is also the proof that the
                // declaring class is initialized), when the field is a
                // reference, or when the helper is unwired.
                0xb3 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.static_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putstatic at pc={start_pc}: no resolved static field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_putstatic(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putstatic at pc={start_pc}: an unwired helper, an uninitialized class, or a reference static — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // getfield (round 9 wave 16). Past the gate because its
                // lowering orders its own access: the read happens inside
                // `jit_getfield`, which takes the object's own lock-free path,
                // and a volatile instance READ owes no fence this backend can
                // place around a helper -- see `emit_getfield`. Refused when
                // the site was not resolved, when the field is a reference, or
                // when the helper, the context word or the exact NPE path is
                // missing.
                0xb4 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.instance_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getfield at pc={start_pc}: no resolved instance field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_getfield(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getfield at pc={start_pc}: no exact NullPointerException path, an unwired helper, or an unrecognised type tag — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // putfield (round 9 wave 15). Past the gate because its
                // lowering orders its own access -- a volatile store gets a
                // `DMB ISH` on both sides, because the store itself happens
                // inside a helper where it is relaxed. Refused here when the
                // site was not resolved, when the field is a reference, or
                // when the NPE path a null receiver needs is not exact.
                0xb5 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.instance_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putfield at pc={start_pc}: no resolved instance field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_putfield(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putfield at pc={start_pc}: no exact NullPointerException path, an unwired helper, or a reference field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // arraylength (round 9 wave 12). Past the gate for the same
                // reason `getstatic` is -- its lowering orders every access it
                // makes -- and refused here when the NPE throw path it needs
                // for a null receiver is not wired or not exact.
                0xbe => {
                    if !self.emit_arraylength() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "arraylength at pc={start_pc}: no exact NullPointerException path — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // multianewarray (h23c, 2026-09-22).
                0xc5 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    let Some(&ndims) = bytecode.get(pc + 2) else {
                        success = false;
                        break;
                    };
                    pc += 3;
                    if !self.emit_multianewarray(usize::from(ndims)) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "multianewarray at pc={start_pc}: no resolved site, an unwired helper, an unsupported arity, or no route for a pending exception — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // monitorenter / monitorexit (h23, 2026-09-22).
                0xc2 | 0xc3 => {
                    if !self.emit_monitor_op(opcode == 0xc2) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "monitor at pc={start_pc}: an unwired helper, or no route for a pending exception — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // checkcast / instanceof (round 9 wave 23).
                0xc0 | 0xc1 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    if !self.emit_typecheck(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "typecheck at pc={start_pc}: unresolved target class, an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // THE FIVE INVOKE FORMS (round 9 wave 22), all through
                // `jit_invoke_dispatch`. The operand consumed by the
                // instruction differs in WIDTH, not in meaning: `0xb9`
                // carries a count byte and a zero after its index and `0xba`
                // two zeros, and reading three bytes for either walks into the
                // next instruction.
                0xb6..=0xba => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += if matches!(opcode, 0xb9 | 0xba) { 4 } else { 2 };
                    if !self.emit_invoke(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "invoke at pc={start_pc}: unresolved site, unwired dispatch helper or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // ifnull ifnonnull
                0xc6 | 0xc7 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    self.emit_if_null(opcode == 0xc7, branch_target(start_pc, i32::from(off)));
                }
                _ => {
                    self.buffer.emit(Arm64Instruction::Comment(format!(
                        "unsupported opcode 0x{:02x} at pc={}",
                        opcode, start_pc
                    )));
                    success = false;
                    break;
                }
            }
        }

        self.emit_epilogue();
        // Out of line, after the `RET`: straight-line code never falls into
        // these.
        self.emit_arith_throw_stub();
        self.emit_npe_throw_stubs();
        self.emit_aioobe_stubs();

        let frame = match self.frame.take() {
            Some(f) => f,
            None => {
                return Arm64CompileResult {
                    instructions: Vec::new(),
                    frame: Arm64FrameLayout {
                        frame_size: 0,
                        callee_save_offset: 0,
                        spill_offset: 0,
                        num_spills: 0,
                        saved_regs: Vec::new(),
                        num_reg_locals: 0,
                    },
                    labels: HashMap::new(),
                    success: false,
                    pending_oop_maps: Vec::new(),
                    sp_id_slot_off: 0,
                    safepoint_count: 0,
                    incomplete_oop_maps: 0,
                    static_init_classes: Vec::new(),
                    needs_context: false,
                    polls_enabled: false,
                    frame_base_published: false,
                }
            }
        };
        let mut static_init_classes = std::mem::take(&mut self.static_init_classes);
        static_init_classes.sort_unstable();
        static_init_classes.dedup();
        Arm64CompileResult {
            static_init_classes,
            needs_context: self.needs_context,
            polls_enabled: self.safepoints_enabled,
            frame_base_published: self.frame_base_published,
            instructions: self.buffer.instructions.clone(),
            frame,
            labels: self.buffer.labels.clone(),
            success: success && !self.failed,
            sp_id_slot_off: self.sp_id_slot_off,
            safepoint_count: self.safepoint_count,
            incomplete_oop_maps: self.incomplete_oop_maps,
            pending_oop_maps: std::mem::take(&mut self.pending_oop_maps),
        }
    }

    // -- Arithmetic ----------------------------------------------------------

    /// A two-operand integer operation on `kind` operands. For `I32` the
    /// instruction is a W form, whose result is the JVMS 32-bit wrapped value,
    /// and the result is sign-extended back to the canonical form.
    fn emit_binary_int(
        &mut self,
        kind: OperandKind,
        make: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        self.buffer.emit(make(dst, lhs, rhs));
        if kind == OperandKind::I32 {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// A shift of a `kind` value by an `int` amount.
    fn emit_shift(
        &mut self,
        kind: OperandKind,
        make: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let amount = self.pop_kind(OperandKind::I32);
        let value = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        self.buffer.emit(make(dst, value, amount));
        if kind == OperandKind::I32 {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// A two-operand FP operation, in the S form for `float` and the D form
    /// for `double`. `float` used to be computed as `double` end to end, which
    /// gets rounding, overflow and the bits handed back to the VM all wrong.
    fn emit_binary_fp(
        &mut self,
        kind: OperandKind,
        single: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
        double: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(true);
        let inst = if kind == OperandKind::F32 {
            single(dst, lhs, rhs)
        } else {
            double(dst, lhs, rhs)
        };
        self.buffer.emit(inst);
        self.push_reg(kind, dst);
    }

    /// `fneg` / `dneg`: FNEG, which flips the sign bit. The old `0.0 - x`
    /// turned `-(+0.0)` into `+0.0` rather than `-0.0`.
    fn emit_float_neg(&mut self, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let dst = self.alloc_reg(true);
        let inst = if kind == OperandKind::F32 {
            Arm64Instruction::FnegSingle { vd: dst, vn: src }
        } else {
            Arm64Instruction::FnegDouble { vd: dst, vn: src }
        };
        self.buffer.emit(inst);
        self.push_reg(kind, dst);
    }

    /// `iinc`: a W-form add, re-sign-extended, on the local's home.
    fn emit_iinc(&mut self, index: usize, delta: i32) {
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        let add = |rd: Arm64Register| {
            if delta >= 0 {
                Arm64Instruction::AddImmW {
                    rd,
                    rn: rd,
                    imm: delta,
                }
            } else {
                Arm64Instruction::SubImmW {
                    rd,
                    rn: rd,
                    imm: -delta,
                }
            }
        };
        match home {
            Some(reg) => {
                self.buffer.emit(add(reg));
                self.emit_sxtw(reg);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                let tmp = self.alloc_reg(false);
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: tmp,
                    rn: Arm64Register::FP,
                    offset,
                });
                self.buffer.emit(add(tmp));
                self.emit_sxtw(tmp);
                self.buffer.emit(Arm64Instruction::Str {
                    rt: tmp,
                    rn: Arm64Register::FP,
                    offset,
                });
            }
        }
    }

    /// The numeric conversions `i2l` (0x85) through `i2s` (0x93).
    ///
    /// Width discipline, given that an `int` is held sign-extended:
    /// * `i2l` is a relabelling -- the register already holds the `long`.
    /// * `l2i` is `SXTW`, keeping the low 32 bits as a signed value. It used to
    ///   `AND` with `0xFFFFFFFF`, i.e. zero-extend, so `(int) -1L` read back as
    ///   4294967295 anywhere the upper half was observed.
    /// * `f2i`/`d2i` are `FCVTZS W` then `SXTW`: the W form saturates at the
    ///   32-bit bounds, as the JVMS requires. The X form they used saturated at
    ///   the 64-bit bounds, so `(int) 1e10` was not `Integer.MAX_VALUE`.
    /// * `i2f`/`i2d` read the W register; `l2f`/`l2d` read the X register.
    fn emit_conversion(&mut self, opcode: u8) {
        use OperandKind::{F32, F64, I32, I64};
        match opcode {
            // i2l
            0x85 => {
                let Some(top) = self.operand_stack.last_mut() else {
                    self.failed = true;
                    return;
                };
                if top.kind != I32 {
                    self.failed = true;
                    return;
                }
                top.kind = I64;
            }
            0x86 => self.emit_convert(I32, F32, false, |d, s| Arm64Instruction::ScvtfSingle {
                vd: d,
                rn: s,
            }),
            0x87 => self.emit_convert(I32, F64, false, |d, s| Arm64Instruction::ScvtfDoubleW {
                vd: d,
                rn: s,
            }),
            0x88 => self.emit_convert(I64, I32, false, |d, s| Arm64Instruction::Sxtw {
                rd: d,
                rn: s,
            }),
            0x89 => self.emit_convert(I64, F32, false, |d, s| Arm64Instruction::ScvtfSingleX {
                vd: d,
                rn: s,
            }),
            0x8a => self.emit_convert(I64, F64, false, |d, s| Arm64Instruction::ScvtfDouble {
                vd: d,
                rn: s,
            }),
            0x8b => self.emit_convert(F32, I32, true, |d, s| Arm64Instruction::FcvtzsSingle {
                rd: d,
                vn: s,
            }),
            0x8c => self.emit_convert(F32, I64, false, |d, s| Arm64Instruction::FcvtzsSingleX {
                rd: d,
                vn: s,
            }),
            0x8d => self.emit_convert(F32, F64, false, |d, s| {
                Arm64Instruction::FcvtSingleToDouble { vd: d, vn: s }
            }),
            0x8e => self.emit_convert(F64, I32, true, |d, s| Arm64Instruction::FcvtzsIntW {
                rd: d,
                vn: s,
            }),
            0x8f => self.emit_convert(F64, I64, false, |d, s| Arm64Instruction::FcvtzsInt {
                rd: d,
                vn: s,
            }),
            0x90 => self.emit_convert(F64, F32, false, |d, s| {
                Arm64Instruction::FcvtDoubleToSingle { vd: d, vn: s }
            }),
            // i2b, i2c (zero-extend 16 bits), i2s
            0x91 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::Sxtb {
                rd: d,
                rn: s,
            }),
            0x92 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::AndImm {
                rd: d,
                rn: s,
                imm: 0xFFFF,
            }),
            0x93 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::Sxth {
                rd: d,
                rn: s,
            }),
            _ => self.failed = true,
        }
    }

    /// Pop a `from`, emit `make(dst, src)` into a fresh register of the `to`
    /// class, sign-extend when `sxtw`, and push a `to`.
    fn emit_convert(
        &mut self,
        from: OperandKind,
        to: OperandKind,
        sxtw: bool,
        make: fn(Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let src = self.pop_kind(from);
        let dst = self.alloc_reg(to.is_fp());
        self.buffer.emit(make(dst, src));
        if sxtw {
            self.emit_sxtw(dst);
        }
        self.push_reg(to, dst);
    }

    // -- Compare / Branch ---------------------------------------------------

    /// `lcmp`: `CMP; CSET ne; CNEG lt` -- 1, 0 or -1 with no branch.
    fn emit_lcmp(&mut self) {
        let rhs = self.pop_kind(OperandKind::I64);
        let lhs = self.pop_kind(OperandKind::I64);
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::Cmp { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::Cset {
            rd: dst,
            cond: Arm64Condition::Ne,
        });
        self.buffer.emit(Arm64Instruction::Cneg {
            rd: dst,
            rn: dst,
            cond: Arm64Condition::Lt,
        });
        self.push_reg(OperandKind::I32, dst);
    }

    /// `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`: `FCMP; CSET ne; CNEG <c>`.
    ///
    /// After `FCMP`: equal is `Z=1 C=1`, less `N=1`, greater `C=1`, and
    /// unordered `C=1 V=1`. `CSET ne` gives 1 for everything but equal; the
    /// negation then decides where NaN lands:
    ///
    /// * `*cmpg` (NaN -> +1) negates on `MI` (`N=1`), which only "less" sets.
    /// * `*cmpl` (NaN -> -1) negates on `LT` (`N!=V`), which "less" and
    ///   "unordered" both satisfy.
    ///
    /// The branchy version it replaces took `B.LT` for `*cmpg`, which is TRUE
    /// on unordered, so NaN produced -1 where the JVMS requires +1.
    fn emit_fcmp(&mut self, kind: OperandKind, nan_is_greater: bool) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        let cmp = if kind == OperandKind::F32 {
            Arm64Instruction::FcmpSingle { vn: lhs, vm: rhs }
        } else {
            Arm64Instruction::FcmpDouble { vn: lhs, vm: rhs }
        };
        self.buffer.emit(cmp);
        self.buffer.emit(Arm64Instruction::Cset {
            rd: dst,
            cond: Arm64Condition::Ne,
        });
        self.buffer.emit(Arm64Instruction::Cneg {
            rd: dst,
            rn: dst,
            cond: if nan_is_greater {
                Arm64Condition::Mi
            } else {
                Arm64Condition::Lt
            },
        });
        self.push_reg(OperandKind::I32, dst);
    }

    /// `if_icmp<cond>`: a W-form compare.
    pub fn emit_if_icmp(&mut self, cond: Arm64Condition, target_pc: usize) {
        let rhs = self.pop_kind(OperandKind::I32);
        let lhs = self.pop_kind(OperandKind::I32);
        let label = self.prepare_branch(target_pc);
        self.buffer
            .emit(Arm64Instruction::CmpW { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::BCond { cond, label });
    }

    /// `if_acmp<cond>`: references are full 64-bit words.
    fn emit_if_acmp(&mut self, cond: Arm64Condition, target_pc: usize) {
        let rhs = self.pop_kind(OperandKind::Ref);
        let lhs = self.pop_kind(OperandKind::Ref);
        let label = self.prepare_branch(target_pc);
        self.buffer.emit(Arm64Instruction::Cmp { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::BCond { cond, label });
    }

    /// `if<cond>` against zero, on the W register.
    fn emit_if_zero(&mut self, cond: Arm64Condition, target_pc: usize) {
        let val = self.pop_kind(OperandKind::I32);
        let label = self.prepare_branch(target_pc);
        match cond {
            Arm64Condition::Eq => self.buffer.emit(Arm64Instruction::CbzW { rt: val, label }),
            Arm64Condition::Ne => self.buffer.emit(Arm64Instruction::CbnzW { rt: val, label }),
            _ => {
                self.buffer
                    .emit(Arm64Instruction::CmpImmW { rn: val, imm: 0 });
                self.buffer.emit(Arm64Instruction::BCond { cond, label });
            }
        }
    }

    /// `ifnull` / `ifnonnull`.
    fn emit_if_null(&mut self, nonnull: bool, target_pc: usize) {
        let val = self.pop_kind(OperandKind::Ref);
        let label = self.prepare_branch(target_pc);
        if nonnull {
            self.buffer.emit(Arm64Instruction::Cbnz { rt: val, label });
        } else {
            self.buffer.emit(Arm64Instruction::Cbz { rt: val, label });
        }
    }

    // -- Load / Store locals ------------------------------------------------

    /// Size the frame from a spill-area partition and record both.
    ///
    /// The only place `self.frame` and `self.spill_area` are set, so the total
    /// the frame is sized from is derived from the same partition the
    /// accessors index against and cannot drift from it. A test that builds a
    /// frame goes through here too: a frame without its partition is a frame
    /// production never builds, and letting a test construct one is how a
    /// fixture ends up proving something about a shape that cannot occur.
    fn install_frame(&mut self, num_locals: usize, area: Arm64SpillArea, saved: &[Arm64Register]) {
        self.spill_area = area;
        self.frame = Some(Arm64FrameLayout::compute(num_locals, area.total(), saved));
    }

    /// FP-relative offset of spill-area word `word`, or `None` when there is no
    /// frame yet or the word index is `None`.
    ///
    /// The ONE place a word index becomes a byte offset. Every caller gets its
    /// index from an [`Arm64SpillArea`] accessor, which has already bounded it
    /// against its own region, so this function's only job is the arithmetic.
    fn spill_word_offset(&self, word: Option<usize>) -> Option<i32> {
        let frame = self.frame.as_ref()?;
        let scaled = i32::try_from(word?).ok()?.checked_mul(8)?;
        frame.spill_offset.checked_add(scaled)
    }

    /// FP-relative offset of frame-homed local `index`'s word.
    fn local_slot_offset(&self, index: usize) -> Option<i32> {
        let word = self.spill_area.local_word(self.spill_index_for(index));
        self.spill_word_offset(word)
    }

    /// Push a copy of local `index`, read as a `kind`.
    fn emit_load_local(&mut self, index: usize, kind: OperandKind) {
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        let dst = self.alloc_reg(kind.is_fp());
        match home {
            Some(reg) => {
                let inst = match kind {
                    OperandKind::F32 => Arm64Instruction::FmovToFpSingle { vd: dst, rn: reg },
                    OperandKind::F64 => Arm64Instruction::FmovToFp { vd: dst, rn: reg },
                    _ => Arm64Instruction::Mov { rd: dst, rm: reg },
                };
                self.buffer.emit(inst);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                self.emit_load_kind(dst, kind, offset);
            }
        }
        self.push_reg(kind, dst);
    }

    /// Pop a `kind` and store it to local `index`.
    fn emit_store_local(&mut self, index: usize, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        match home {
            Some(reg) => {
                let inst = match kind {
                    OperandKind::F32 => Arm64Instruction::FmovFromFpSingle { rd: reg, vn: src },
                    OperandKind::F64 => Arm64Instruction::FmovFromFp { rd: reg, vn: src },
                    _ => Arm64Instruction::Mov { rd: reg, rm: src },
                };
                self.buffer.emit(inst);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                self.emit_store_kind(src, kind, offset);
            }
        }
    }

    /// How many spill slots the FRAME-HOMED LOCALS occupy -- equivalently, the
    /// first spill index the operand area uses. Both the operand area and the
    /// safepoint homes take their base from here, or they overlap the locals
    /// (see `operand_spill_slots_do_not_alias_frame_homed_locals`).
    fn local_spill_count(&self) -> usize {
        self.spill_index_for(self.local_regs.len())
    }

    /// The spill index of a local that has no register: how many locals before
    /// it also have none.
    fn spill_index_for(&self, local_index: usize) -> usize {
        let mut spill_idx = 0;
        for i in 0..local_index {
            let has_gpr = self.local_regs.get(i).map_or(false, |r| r.is_some());
            let has_fp = self.float_local_regs.get(i).map_or(false, |r| r.is_some());
            if !has_gpr && !has_fp {
                spill_idx += 1;
            }
        }
        spill_idx
    }

    // -- Constants ----------------------------------------------------------

    /// An `int` constant, sign-extended -- already the canonical form.
    pub fn emit_iconst(&mut self, value: i32) {
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: i64::from(value),
        });
        self.push_reg(OperandKind::I32, dst);
    }

    pub fn emit_lconst(&mut self, value: i64) {
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: value,
        });
        self.push_reg(OperandKind::I64, dst);
    }

    /// A `float` constant, bit-exact: its IEEE-754 single pattern into a GPR,
    /// then `FMOV Sd, Wn` (a bit move, not a conversion). It used to load the
    /// DOUBLE pattern, so every `float` was a `double` in disguise.
    pub fn emit_fconst(&mut self, value: f32) {
        let tmp = self.alloc_reg(false);
        let dst = self.alloc_reg(true);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: tmp,
            imm: i64::from(value.to_bits()),
        });
        self.buffer
            .emit(Arm64Instruction::FmovToFpSingle { vd: dst, rn: tmp });
        self.push_reg(OperandKind::F32, dst);
    }

    /// A `double` constant, bit-exact via `FMOV Dd, Xn`.
    pub fn emit_dconst(&mut self, value: f64) {
        let tmp = self.alloc_reg(false);
        let dst = self.alloc_reg(true);
        // Cast: reinterpret the f64 as its raw 64-bit pattern.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: tmp,
            imm: value.to_bits() as i64,
        });
        self.buffer
            .emit(Arm64Instruction::FmovToFp { vd: dst, rn: tmp });
        self.push_reg(OperandKind::F64, dst);
    }

    // -- Static fields (round 9 wave 10) ------------------------------------

    /// `getstatic` of a resolved PRIMITIVE static: the x64 inline
    /// `getstatic`'s shape (`x64/objects.rs` `try_emit_inline_getstatic`)
    /// with the aarch64 memory model applied.
    ///
    /// ```text
    ///   MOV   Xa, #base_cell         ; &statics_index[class].base
    ///   LDR   Xa, [Xa]               ; the class's statics block
    ///   ADD   Xa, Xa, #(field_index*SLOT_SIZE + payload)
    ///   LDR   Wa|Xa, [Xa]            ; LDAR for a volatile field
    ///   SXTW  Xa, Wa                 ; int category: this backend's int form
    ///   FMOV  Sv|Dv, Wa|Xa           ; float / double: into an FP register
    /// ```
    ///
    /// The cell is the 16-byte `Value` layout: the int category (`Z B C S I`)
    /// and `F` read their 32-bit payload at `FIELD_CELL_PAYLOAD32_OFFSET`, `J`
    /// and `D` the 64-bit one at `FIELD_CELL_PAYLOAD64_OFFSET` -- the same
    /// loads, widths and result conventions as x64 (`MOVSXD` for the int
    /// category is `LDR W` + `SXTW` here). A 64-bit `LDAR` of the 8-aligned
    /// payload is single-copy atomic, which JLS §17.7 requires of a `volatile
    /// long`/`double`.
    ///
    /// JMM: a `volatile` read is `LDAR` and nothing more. ARMv8 orders a
    /// prior `STLR` before a later `LDAR` (RCsc), so the StoreLoad edge is the
    /// writer's to pay, exactly as x64 puts its `MFENCE` after the volatile
    /// STORE and none after the load. A plain read needs no ordering.
    ///
    /// # Reference statics (h23, 2026-09-22)
    ///
    /// `L` and `[` read the SAME 64-bit payload word as `J`/`D` and push it as
    /// an `OperandKind::Ref`, which is exactly what x64's
    /// `try_emit_inline_getstatic` does (`b'J' | b'D' | b'L' | b'['` share one
    /// arm there, followed by `mark_top_as_oop`). The oop story the earlier
    /// refusal was waiting for is the operand model's, and it already exists:
    /// a `Ref` entry carries `oop: true` (`Operand::in_reg`), the shuffles
    /// carry the mark, and a safepoint spills the entry to its depth slot,
    /// which is what `local_oop_mask_at_current_pc` publishes. No load barrier
    /// is owed: every collector in this VM reads a static's payload word raw
    /// from compiled code, which is the contract the inline `getfield` arms
    /// and x64's own reference `getstatic` already run under. `Object(None)`
    /// leaves the payload word zero, i.e. JVM null, so no decode is needed.
    ///
    /// A `volatile` reference static gets the same `LDAR` as any other
    /// volatile read here, which is stronger than the plain `MOV` x64 emits
    /// under TSO and correct for the same reason.
    ///
    /// Returns `false`, having emitted nothing, for an unknown type tag, a
    /// zero `base_cell`, or an offset that does not fit; the caller refuses
    /// the method.
    fn emit_getstatic(&mut self, field: Arm64StaticField) -> bool {
        use cratonvm_types::{FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET, SLOT_SIZE};
        let (kind, width, payload) = match field.type_tag {
            b'L' | b'[' => (
                OperandKind::Ref,
                Arm64MemWidth::X64,
                FIELD_CELL_PAYLOAD64_OFFSET,
            ),
            b'Z' | b'B' | b'C' | b'S' | b'I' => (
                OperandKind::I32,
                Arm64MemWidth::W32,
                FIELD_CELL_PAYLOAD32_OFFSET,
            ),
            b'F' => (
                OperandKind::F32,
                Arm64MemWidth::W32,
                FIELD_CELL_PAYLOAD32_OFFSET,
            ),
            b'J' => (
                OperandKind::I64,
                Arm64MemWidth::X64,
                FIELD_CELL_PAYLOAD64_OFFSET,
            ),
            b'D' => (
                OperandKind::F64,
                Arm64MemWidth::X64,
                FIELD_CELL_PAYLOAD64_OFFSET,
            ),
            _ => return false,
        };
        if field.base_cell == 0 {
            return false;
        }
        let Some(cell_off) = field
            .field_index
            .checked_mul(SLOT_SIZE)
            .and_then(|off| off.checked_add(payload))
            .and_then(|off| i32::try_from(off).ok())
        else {
            return false;
        };
        let addr = self.alloc_reg(false);
        // Cast: the pointer cell's address as the 64-bit pattern `MovImm`
        // materializes.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: addr,
            imm: field.base_cell as i64,
        });
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: addr,
            rn: addr,
            offset: 0,
        });
        // `cell_off` is never 0 (the payload offsets are 4 and 8), but the
        // pseudo-op costs nothing to skip if a layout ever makes it so.
        if cell_off != 0 {
            self.buffer.emit(Arm64Instruction::AddImm {
                rd: addr,
                rn: addr,
                imm: cell_off,
            });
        }
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: addr,
            rn: addr,
            width,
            acquire: field.is_volatile,
        });
        match kind {
            OperandKind::I32 => {
                self.emit_sxtw(addr);
                self.push_reg(OperandKind::I32, addr);
            }
            OperandKind::F32 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: addr });
                self.push_reg(OperandKind::F32, v);
            }
            OperandKind::F64 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: addr });
                self.push_reg(OperandKind::F64, v);
            }
            OperandKind::Ref => {
                self.push_reg(OperandKind::Ref, addr);
                // Belt and braces with `Operand::in_reg`'s own `kind == Ref`,
                // and the same line the reference-returning `invoke` arm
                // writes: the mark is what a safepoint's oop map is built from.
                self.mark_top_operand_as_oop();
            }
            // `I64` (the only other kind the table above produces).
            _ => self.push_reg(kind, addr),
        }
        self.static_init_classes.push(field.class_id);
        true
    }

    // -- Mid-method helper calls --------------------------------------------

    /// Call a runtime helper from the MIDDLE of a method, preserving the
    /// operand stack across it (round 9 wave 15).
    ///
    /// ```text
    ///     <store every register-located operand to its depth slot>
    ///     MOV       X0..X7, <arguments>      ; or MOVZ/MOVK for a constant
    ///     MOVZ/MOVK X16, #helper
    ///     BLR       X16
    ///     MOV       Xinto, X0                ; when the helper returns a value
    ///     <reload the operands into the registers the model says they are in>
    /// ```
    ///
    /// # Why this is not just the safepoint poll's sequence
    ///
    /// Until this landed, every call this backend emitted was one of two
    /// shapes that dodge the hard part. `emit_frame_record` and the entry poll
    /// run with an EMPTY operand stack -- `emit_frame_record` refuses the
    /// method outright when it is not, which is the admission that it cannot
    /// preserve one. The throw stubs (`emit_arith_throw_stub`,
    /// `emit_npe_throw_stubs`, `emit_aioobe_stubs`) marshal, call and branch
    /// to the epilogue: nothing has to survive, because the frame is leaving.
    /// The loop-header poll is the one that does preserve a stack, and it gets
    /// the join for free by being unconditional, argument-less and
    /// result-less.
    ///
    /// # The CALLER allocates the result register, and allocates it FIRST
    ///
    /// `into` is a register the caller already took from [`Self::alloc_reg`],
    /// and this function refuses one that still holds an operand. Both halves
    /// matter, for different reasons.
    ///
    /// Allocating it FIRST is what keeps it out of the reload set: `alloc_reg`
    /// spills whatever operand occupies the register it hands out, so an
    /// allocation made before the spill loop has already moved that entry to
    /// its slot. Allocating after can hand back a register that is about to be
    /// reloaded, and the reload then overwrites the helper's result with the
    /// operand -- silently, because both are legitimate values of the same
    /// width. The refusal here is what makes that a checked rule rather than a
    /// comment.
    ///
    /// Allocating it in the CALLER is what makes a call inside a branch
    /// possible. `getfield`'s sentinel disambiguation calls a second helper on
    /// the rare `X0 == i64::MIN` path; if this function allocated, that
    /// allocation would happen on one path only and could spill an operand on
    /// one path only, so the two paths would reach the join with different
    /// operand models. With the caller allocating before the branch, both
    /// paths are identical by construction.
    ///
    /// # What this does NOT do, and what a caller therefore owes
    ///
    /// * **No oop map.** The reference operands are in their depth slots and
    ///   the reference LOCALS are in callee-saved X19-X28, where a relocating
    ///   collector can neither find nor rewrite them. That is sound only for a
    ///   helper that cannot safepoint, which every caller must establish for
    ///   ITS helper -- `jit_putfield_*` and `jit_getfield` allocate nothing and
    ///   run no Java, and x64 records no map at those call sites either. A
    ///   helper that CAN safepoint (`tlab_post_init`, the `invoke*`
    ///   dispatchers) needs the poll's treatment instead: home the
    ///   register-homed reference locals, record a map at the return address,
    ///   and reload them afterwards.
    /// * **No sentinel check.** A helper that signals an exception by
    ///   returning `i64::MIN` needs its caller to test for it.
    ///
    /// Answers `false`, having set `self.failed`, when it refuses.
    fn emit_helper_call(
        &mut self,
        helper: usize,
        args: &[Arm64HelperArg],
        into: Option<Arm64Register>,
    ) -> bool {
        if helper == 0 {
            // An unwired helper would make this a call through a null pointer.
            self.failed = true;
            return false;
        }
        if args.len() > Arm64EntryConvention::INT_ARG_REGS.len() {
            // A ninth argument goes on the stack, which this sequence does not
            // build. No helper called from here takes one.
            self.failed = true;
            return false;
        }
        for arg in args {
            if let Arm64HelperArg::Reg(reg) = *arg {
                if !SCRATCH_REGS.contains(&reg) {
                    // See `Arm64HelperArg`: a source outside the scratch pool
                    // could alias an argument register.
                    self.failed = true;
                    return false;
                }
            }
        }
        if let Some(dst) = into {
            if !SCRATCH_REGS.contains(&dst) || self.reg_is_live(dst) {
                // See the doc comment: a destination that still holds an
                // operand is one the reload below would overwrite.
                self.failed = true;
                return false;
            }
        }

        // The operand stack, GPR and FP alike: X9-X15 and V0-V7 are all
        // caller-saved, so the call destroys every one of them.
        let mut stored: Vec<(Arm64Register, OperandKind, i32)> = Vec::new();
        for depth in 0..self.operand_stack.len() {
            let operand = self.operand_stack[depth];
            let OperandLoc::Reg(reg) = operand.loc else {
                continue;
            };
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                // No slot for this depth: a live value would sit in a
                // caller-saved register across a CALL. Refuse the method.
                self.failed = true;
                return false;
            };
            self.emit_store_kind(reg, operand.kind, offset);
            stored.push((reg, operand.kind, offset));
        }

        // The arguments. X0-X7 are disjoint from the scratch pool the sources
        // come out of (checked above), so no order of these moves can destroy
        // a source a later one still reads.
        for (i, arg) in args.iter().enumerate() {
            let dst = Arm64EntryConvention::INT_ARG_REGS[i];
            match *arg {
                Arm64HelperArg::Reg(src) => {
                    self.buffer.emit(Arm64Instruction::Mov { rd: dst, rm: src });
                }
                Arm64HelperArg::Imm(imm) => {
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm });
                }
            }
        }

        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: helper as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });

        // Out of X0 BEFORE the reloads. They target scratch registers and X0
        // is not one, so the order is not load-bearing today; it is written
        // this way so that it cannot become load-bearing unnoticed.
        if let Some(dst) = into {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: dst,
                rm: Arm64Register::X0,
            });
        }

        for (reg, kind, offset) in stored {
            self.emit_load_kind(reg, kind, offset);
        }
        true
    }

    /// `putstatic` (`0xb3`) of a PRIMITIVE static, round 9 wave 17.
    ///
    /// ```text
    ///   FMOV  Xval, Dval                  ; F/D only: the helper takes bits
    ///   <narrow the value to the field's declared type>
    ///   LDR   Xctx, [FP, #context]
    ///   DMB   ISH                         ; volatile only
    ///   <emit_helper_call(jit_putstatic_*, [ctx, class_id, field_index, val]) -> Xr>
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xr, Xs
    ///   B.NE  join                        ; 0 is the helper's success value
    ///   MOV   X0, #i64::MIN               ; `<clinit>` failed -- leave
    ///   B     epilogue
    /// join:
    ///   DMB   ISH                         ; volatile only
    /// ```
    ///
    /// # Why this is a CALL when `getstatic` two arms up is two loads
    ///
    /// A raw store into the statics block races `set_static_shared`'s
    /// `grow_to`, which copies the block under the write lock and republishes
    /// it: a compiled store into the OLD block between the copy and the
    /// republish is simply lost. The helper takes that lock, and taking a lock
    /// is a call. The read has no such hazard, which is why `getstatic` can be
    /// the cell read plus one load and this cannot be its mirror image.
    ///
    /// # Why no oop map, when this helper CAN run `<clinit>`
    ///
    /// `ensure_class_initialized_shared` runs Java, and running Java can
    /// safepoint and can move objects -- which by [`Self::emit_helper_call`]'s
    /// stated obligation would mean homing the reference locals, recording a
    /// map at the return address and reloading them. It does not, because
    /// **the class is already initialized at compile time**: the caller
    /// resolves a `base_cell` through `DirectHelperTable::resolve_static_base`,
    /// which answers only for an initialized class, and this arm refuses a
    /// site without one. Initialization is monotonic, so the class cannot
    /// become uninitialized before the body runs -- and the declaring class is
    /// additionally recorded in `CompiledMethod::static_init_classes`, which
    /// the interpreter's compiled-entry path ensure-initializes before first
    /// execution. The `<clinit>` branch inside the helper is therefore
    /// unreachable from here, and the sentinel check below is the belt to that
    /// pair of braces.
    ///
    /// # The sentinel is an EXCEPTION, not a re-run
    ///
    /// `jit_putstatic_*` returns `0` on success and `i64::MIN` only after
    /// `set_jit_pending_exception` -- so leaving through the epilogue with it
    /// raises that exception rather than re-running the method from its entry,
    /// which is what makes this safe without a deopt point. The comparison is
    /// against `i64::MIN` specifically, not `!= 0`: a spurious bail would
    /// leave with the sentinel and NO pending exception, which the interpreter
    /// would read as a deopt and re-run -- double-executing this very store.
    ///
    /// # `DMB ISH` on BOTH sides of a volatile store
    ///
    /// The same asymmetry as `putfield`, and for the same reason: x64 emits
    /// only the trailing `MFENCE` because TSO already makes its plain `MOV` a
    /// release, while here the store happens inside the helper and a `BLR`
    /// orders nothing.
    ///
    /// A REFERENCE static takes the same shape through
    /// `jit_putstatic_object`, which adds the SATB pre-barrier and the card
    /// mark (round 9 wave 19).
    ///
    /// Returns `false`, and the caller refuses the method, for an unresolved
    /// site, an unwired helper, a non-empty exception table, or no context
    /// word.
    fn emit_putstatic(&mut self, field: Arm64StaticField) -> bool {
        // A REFERENCE static takes the same shape with a different helper:
        // `jit_putstatic_object` runs the SATB pre-barrier and the card mark
        // the primitive stores do not owe, and takes the value as a pointer
        // rather than as a narrowed word (round 9 wave 19).
        let is_ref = matches!(field.type_tag, b'L' | b'[');
        let kind = if is_ref {
            OperandKind::Ref
        } else {
            let Some(kind) = primitive_field_operand_kind(field.type_tag) else {
                return false;
            };
            kind
        };
        let helper = match field.type_tag {
            b'L' | b'[' => self.helpers.putstatic_object,
            b'J' => self.helpers.putstatic_long,
            b'F' => self.helpers.putstatic_float,
            b'D' => self.helpers.putstatic_double,
            _ => self.helpers.putstatic_int,
        };
        // `base_cell` is not READ here -- the helper does the store. It is the
        // caller's proof that the declaring class is already initialized; see
        // the doc comment.
        if helper == 0 || field.base_cell == 0 {
            return false;
        }
        // The sentinel path below leaves through the epilogue for the drain to
        // raise. Wave 17 shipped without a guard here at all: the `<clinit>`
        // branch it reasons is unreachable is not the only way the helper
        // returns the sentinel, because `contain(.., OnPanic::Throw,
        // i64::MIN, ..)` returns it for a PANIC as well. `can_route_exception`
        // is what makes the drain's throw exact regardless -- see its doc.
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        let Ok(index) = i64::try_from(field.field_index) else {
            return false;
        };

        let val = self.pop_kind(kind);
        if self.failed {
            return false;
        }
        // The helpers take `(vm_ptr, class_id, field_index, val: i64)`, so a
        // float or double travels in a GPR as its bit pattern. A reference is
        // already one.
        let val = match kind {
            OperandKind::F32 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFpSingle { rd: g, vn: val });
                g
            }
            OperandKind::F64 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFp { rd: g, vn: val });
                g
            }
            _ => val,
        };
        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let ret = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        // JVMS §6.5: a `boolean` static stores `value & 1` and a
        // `byte`/`char`/`short` static only its own width. `jit_putstatic_int`
        // stores its operand verbatim and has no descriptor to narrow by, so
        // it happens here -- exactly as x64's `0xb3` arm does it. A reference
        // has no narrowing: `emit_narrow_to_field_tag` answers nothing for
        // `L`/`[`, and this says so rather than relying on it.
        if !is_ref {
            self.emit_narrow_to_field_tag(val, field.type_tag);
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        if !self.emit_helper_call(
            helper,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(i64::from(field.class_id)),
                Arm64HelperArg::Imm(index),
                Arm64HelperArg::Reg(val),
            ],
            Some(ret),
        ) {
            return false;
        }
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: ret,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        // The ensure-initialized obligation, as x64 records it for every
        // static site including every `putstatic`.
        self.static_init_classes.push(field.class_id);
        true
    }

    // -- Arrays -------------------------------------------------------------

    /// `arraylength` (`0xbe`), round 9 wave 12. The second ordered lowering,
    /// and the first one with a RECEIVER.
    ///
    /// ```text
    ///   CBZ   Xa, npe_ARRAY_LENGTH        ; JVMS: a null array throws NPE
    ///   ADD   Xd, Xa, #ARRAY_LENGTH_OFFSET
    ///   LDR   Wd, [Xd]                    ; the header's `shape` word
    ///   SXTW  Xd, Wd                      ; this backend's canonical int form
    /// ```
    ///
    /// x64 emits the same two steps (`x64/arrays.rs::emit_arraylength_regs`,
    /// `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` behind a null check). The
    /// 32-bit load ZERO-extends and the `SXTW` then sign-extends, which agree
    /// because an array length is a non-negative `int`: the allocator refuses
    /// a negative one (`NegativeArraySizeException`), so bit 31 is clear.
    ///
    /// # Why a PLAIN `LDR` is the ordered lowering on a weakly-ordered machine
    ///
    /// This is the part that is not obvious from x64. An array's length is
    /// written once, by the allocator, before the reference is published, and
    /// never again -- it is a `final` field in all but name. The hazard a weak
    /// memory model adds is reading the length through a freshly published
    /// reference and seeing the pre-initialization value. AArch64 cannot do
    /// that here: the length load's ADDRESS is computed FROM the reference,
    /// so it is address-dependent on the load that produced the reference, and
    /// ARMv8 never reorders a load before the load its address depends on
    /// (§B2.3.2, "dependency-ordered before"). That is exactly the guarantee
    /// the JMM's freeze action needs, and it is why no `LDAR` and no
    /// `DMB ISHLD` is owed -- not because x64 emits none.
    ///
    /// Returns `false`, having emitted nothing, when there is no exact NPE
    /// path ([`Self::can_throw_npe`]); the caller refuses the method.
    fn emit_arraylength(&mut self) -> bool {
        if !self.can_throw_npe() {
            return false;
        }
        let arr = self.pop_kind(OperandKind::Ref);
        self.emit_null_check(arr, npe_action::ARRAY_LENGTH);
        // `arr` is HELD, so `dst` is a different register and the `ADD` cannot
        // clobber the receiver the stub above still names.
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: dst,
            rn: arr,
            // Cast: a small header constant, const-asserted <= 127 in
            // `cratonvm_types`.
            imm: cratonvm_types::ARRAY_LENGTH_OFFSET as i32,
        });
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: dst,
            rn: dst,
            width: Arm64MemWidth::W32,
            acquire: false,
        });
        self.emit_sxtw(dst);
        self.push_reg(OperandKind::I32, dst);
        true
    }

    // -- Arrays (round 9 wave 13) -------------------------------------------

    /// Can this compilation raise `ArrayIndexOutOfBoundsException` from
    /// compiled code? Unlike [`Self::can_throw_arithmetic`] and
    /// [`Self::can_throw_npe`], only the helper needs to be wired:
    /// `jit_throw_aioobe` takes the throwing bci as its own fourth argument
    /// (`emit_aioobe_stubs`) and stamps `JIT_SIGNALS::athrow_bci` with it
    /// directly (`vm/src/jit/helpers.rs`), the same cell
    /// [`Self::emit_stamp_throw_bci`] writes for the lowerings that need it --
    /// so the drain always has an exact throw site here, table empty or not.
    fn can_throw_aioobe(&self) -> bool {
        self.helpers.throw_aioobe != 0
    }

    /// Both throw paths an array element access needs: a null receiver is an
    /// NPE and an out-of-range index an AIOOBE, and a lowering that can raise
    /// only one of them is not a lowering.
    fn can_access_array(&self) -> bool {
        self.can_throw_npe() && self.can_throw_aioobe()
    }

    /// Null-check `arr`, bounds-check `idx` against its length, and compute
    /// the element address `arr + ARRAY_DATA_OFFSET + (idx << shift)`.
    ///
    /// ```text
    ///   CBZ   Xarr, npe_<action>
    ///   ADD   Xlen, Xarr, #ARRAY_LENGTH_OFFSET
    ///   LDR   Wlen, [Xlen]
    ///   CMP   Widx, Wlen
    ///   B.HS  aioobe_<site>          ; UNSIGNED: catches idx < 0 too
    ///   ADD   Xaddr, Xarr, Xidx, LSL #shift
    ///   ADD   Xaddr, Xaddr, #ARRAY_DATA_OFFSET
    /// ```
    ///
    /// The unsigned compare is x64's trick (`CMP ECX, [len]` + `JAE`) and for
    /// the same reason: a negative index reads as a huge unsigned value, so
    /// one branch covers both halves of the JVMS check. `idx` arrives in this
    /// backend's canonical int form (`SXTW`), so its W view is the `int` and
    /// its X view is the correct 64-bit scale factor once the check has
    /// proved it non-negative.
    ///
    /// Neither the length load nor the element access is ordered, and that is
    /// the lowering, not an omission: both addresses are derived from the
    /// array reference, so ARMv8 orders them after the load that produced it
    /// (address dependency), and a plain array element is ordinary memory the
    /// JMM owes nothing about -- exactly what x64 emits plain `MOV`s for.
    ///
    /// Returns the address register, or `None` (having set `failed`) when the
    /// registers it would name are not the scratch ones the stub assumes.
    fn emit_array_guards_and_address(
        &mut self,
        arr: Arm64Register,
        idx: Arm64Register,
        action: u8,
        shift: u8,
    ) -> Option<Arm64Register> {
        self.emit_null_check(arr, action);
        // `arr` and `idx` are HELD, so `len` is neither of them.
        let len = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: len,
            rn: arr,
            // Cast: a small header constant, const-asserted <= 127.
            imm: cratonvm_types::ARRAY_LENGTH_OFFSET as i32,
        });
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: len,
            rn: len,
            width: Arm64MemWidth::W32,
            acquire: false,
        });
        self.buffer
            .emit(Arm64Instruction::CmpW { rn: idx, rm: len });
        // The stub reads these three registers, so they must be ones nothing
        // between the branch and the stub can be holding: the argument
        // registers the stub writes (X0-X3, X16) are disjoint from the scratch
        // pool by construction, and this is the assertion of that.
        if !SCRATCH_REGS.contains(&arr)
            || !SCRATCH_REGS.contains(&idx)
            || !SCRATCH_REGS.contains(&len)
        {
            self.failed = true;
            return None;
        }
        let label = self.buffer.new_label();
        self.aioobe_stubs.push(Arm64AioobeStub {
            label,
            index: idx,
            length: len,
            array: arr,
            bci: self.cur_bytecode_pc,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Cs,
            label,
        });
        let addr = self.alloc_reg(false);
        if shift == 0 {
            self.buffer.emit(Arm64Instruction::Add {
                rd: addr,
                rn: arr,
                rm: idx,
            });
        } else {
            self.buffer.emit(Arm64Instruction::AddLsl {
                rd: addr,
                rn: arr,
                rm: idx,
                shift,
            });
        }
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: addr,
            rn: addr,
            // Cast: a small header constant, const-asserted <= 127.
            imm: cratonvm_types::ARRAY_DATA_OFFSET as i32,
        });
        Some(addr)
    }

    /// The guards WITHOUT the address: the null check and the unsigned bounds
    /// compare, and nothing else (round 9 wave 19).
    ///
    /// `aaload` and `aastore` want exactly this. They do not address the
    /// element themselves -- only the helper knows its encoding -- and the
    /// address register [`Self::emit_array_guards_and_address`] allocates is
    /// not free: every register it hands out is HELD for the rest of the
    /// bytecode, because the AIOOBE stub reads the guard's registers by name
    /// when it is emitted at the end of the pass. One unnecessary hold is one
    /// fewer register for a bytecode that already needs the array, the index,
    /// the value, the context and a result across two calls.
    fn emit_array_guards_only(
        &mut self,
        arr: Arm64Register,
        idx: Arm64Register,
        action: u8,
    ) -> bool {
        self.emit_array_guards_and_address(arr, idx, action, 0)
            .is_some()
    }

    /// Emit every `ArrayIndexOutOfBoundsException` stub this pass branched to,
    /// after the epilogue's `RET`.
    ///
    /// ```text
    ///   aioobe_<site>:
    ///     MOV       X0, Xidx        ; index   (i64, sign-extended int)
    ///     MOV       X1, Xlen        ; length  (i64, zero-extended u32)
    ///     MOV       X2, Xarr        ; the array, for the helper's diagnostics
    ///     MOVZ/MOVK X3, #bci
    ///     MOVZ/MOVK X16, #jit_throw_aioobe
    ///     BLR       X16             ; returns the i64::MIN deopt sentinel
    ///     B         epilogue
    /// ```
    ///
    /// ONE stub per SITE, unlike the NPE stubs' one per action: the helper
    /// takes the index, the length, the array and the bci, so two sites agree
    /// on none of its four arguments. The three register arguments are read
    /// from where the guard left them, which is sound precisely because the
    /// only way into the stub is that guard's `B.HS`.
    ///
    /// Not a safepoint: the helper allocates nothing and runs no Java, and
    /// this frame is leaving through the epilogue -- the same argument as
    /// [`Self::emit_arith_throw_stub`].
    fn emit_aioobe_stubs(&mut self) {
        let stubs = std::mem::take(&mut self.aioobe_stubs);
        for stub in stubs {
            self.buffer.bind_label(stub.label);
            for (rd, rm) in [
                (Arm64Register::X0, stub.index),
                (Arm64Register::X1, stub.length),
                (Arm64Register::X2, stub.array),
            ] {
                self.buffer.emit(Arm64Instruction::Mov { rd, rm });
            }
            let Ok(bci) = i64::try_from(stub.bci) else {
                self.failed = true;
                return;
            };
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X3,
                imm: bci,
            });
            // Cast: a helper address is a real mapped pointer, always < i64::MAX.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X16,
                imm: self.helpers.throw_aioobe as i64,
            });
            self.buffer.emit(Arm64Instruction::Blr {
                rn: Arm64Register::X16,
            });
            self.buffer.emit(Arm64Instruction::B {
                label: self.epilogue_label,
            });
        }
    }

    /// JVMS §6.5 `bastore`: into a `boolean[]` the `int` is narrowed by
    /// `value & 1`; into a `byte[]` it is truncated to its low byte. One
    /// opcode serves both and the verifier accepts either, so the element type
    /// is a RUNTIME question -- the aarch64 twin of x64's
    /// `emit_bastore_boolean_mask_regs`, reading the same header byte:
    ///
    /// ```text
    ///   LDRB  Wt, [Xarr, #KIND_TAGS_BYTE_OFFSET]
    ///   CMP   Wt, #tag("[Z")
    ///   B.NE  skip
    ///   AND   Xval, Xval, #1
    ///   skip:
    /// ```
    ///
    /// The `AND` is the 64-bit form on purpose: `val` is a canonical
    /// sign-extended `int`, and `& 1` leaves 0 or 1, which is canonical too.
    /// `STRB` then stores the low byte either way, so the `byte[]` path is
    /// byte-identical to no mask at all.
    fn emit_bastore_boolean_mask(&mut self, arr: Arm64Register, val: Arm64Register) {
        let Some(tag) = cratonvm_types::primitive_array_kind_tags_byte("[Z") else {
            // Cannot answer `None` for `"[Z"`; if it ever does, emit nothing
            // and keep the low-byte truncation rather than mask a `byte[]`.
            return;
        };
        let probe = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::Ldrb {
            rt: probe,
            rn: arr,
            // Cast: a small header constant, well inside the byte load's imm12.
            offset: cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32,
        });
        self.buffer.emit(Arm64Instruction::CmpImmW {
            rn: probe,
            imm: i32::from(tag),
        });
        let skip = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: skip,
        });
        self.buffer.emit(Arm64Instruction::AndImm {
            rd: val,
            rn: val,
            imm: 1,
        });
        self.buffer.bind_label(skip);
    }

    /// `iaload`/`laload`/`faload`/`daload`/`baload`/`caload`/`saload`
    /// (round 9 wave 13). `aaload` is not here: see
    /// [`array_element_load_shape`].
    ///
    /// Guards, then one load of the element's own width, then the narrowing
    /// JVMS specifies for the sub-word kinds. `MemLoad` ZERO-extends every
    /// width, so `baload` and `saload` owe an explicit `SXTB`/`SXTH`; `caload`
    /// owes nothing, a `char` being unsigned.
    fn emit_array_load(&mut self, opcode: u8) -> bool {
        let Some(shape) = array_element_load_shape(opcode) else {
            return false;
        };
        if !self.can_access_array() {
            return false;
        }
        let idx = self.pop_kind(OperandKind::I32);
        let arr = self.pop_kind(OperandKind::Ref);
        let Some(addr) = self.emit_array_guards_and_address(arr, idx, shape.action, shape.shift)
        else {
            return false;
        };
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: addr,
            rn: addr,
            width: shape.width,
            acquire: false,
        });
        match shape.kind {
            OperandKind::I32 => {
                match shape.width {
                    Arm64MemWidth::B8 => self
                        .buffer
                        .emit(Arm64Instruction::Sxtb { rd: addr, rn: addr }),
                    Arm64MemWidth::H16 if shape.signed => self
                        .buffer
                        .emit(Arm64Instruction::Sxth { rd: addr, rn: addr }),
                    // `caload`'s zero-extended halfword, and `iaload`'s word,
                    // are already the value; `SXTW` below canonicalizes both.
                    _ => {}
                }
                self.emit_sxtw(addr);
                self.push_reg(OperandKind::I32, addr);
            }
            OperandKind::F32 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: addr });
                self.push_reg(OperandKind::F32, v);
            }
            OperandKind::F64 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: addr });
                self.push_reg(OperandKind::F64, v);
            }
            // `I64` (`laload`), the only other kind the table produces.
            kind => self.push_reg(kind, addr),
        }
        true
    }

    /// `iastore`/`lastore`/`fastore`/`dastore`/`bastore`/`castore`/`sastore`
    /// (round 9 wave 13). `aastore` is not here: see
    /// [`array_element_store_shape`].
    ///
    /// Guards, the `bastore` boolean mask, then one store of the element's own
    /// width -- `STRB`/`STRH` truncate, which is exactly the narrowing JVMS
    /// specifies for `bastore`/`castore`/`sastore`.
    fn emit_array_store(&mut self, opcode: u8) -> bool {
        let Some(shape) = array_element_store_shape(opcode) else {
            return false;
        };
        if !self.can_access_array() {
            return false;
        }
        let val = self.pop_kind(shape.kind);
        let idx = self.pop_kind(OperandKind::I32);
        let arr = self.pop_kind(OperandKind::Ref);
        let Some(addr) = self.emit_array_guards_and_address(arr, idx, shape.action, shape.shift)
        else {
            return false;
        };
        // An FP value moves into a GPR bit-exactly first: `MemStore` names a
        // general register, and the bits are what the element holds.
        let src = match shape.kind {
            OperandKind::F32 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFpSingle { rd: g, vn: val });
                g
            }
            OperandKind::F64 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFp { rd: g, vn: val });
                g
            }
            _ => val,
        };
        if opcode == 0x54 {
            self.emit_bastore_boolean_mask(arr, src);
        }
        self.buffer.emit(Arm64Instruction::MemStore {
            rt: src,
            rn: addr,
            width: shape.width,
            release: false,
        });
        true
    }

    /// Call a runtime helper that CAN SAFEPOINT, with an oop map at the return
    /// address (round 9 wave 18).
    ///
    /// ```text
    ///     <store every register-located operand to its depth slot>
    ///     <store every register-homed reference LOCAL to its safepoint home>
    ///     <stamp the safepoint id>
    ///     MOV       X0..X7, <arguments>
    ///     MOVZ/MOVK X16, #helper
    ///     BLR       X16
    ///     <oop map recorded at the return address>
    ///     MOV       Xinto, X0
    ///     <reload the locals, then the operands>
    /// ```
    ///
    /// # What this adds to [`Self::emit_helper_call`], and why it is a second
    /// function rather than a flag
    ///
    /// `emit_helper_call` records no map, and says so as an obligation each
    /// caller discharges for ITS helper: `jit_putfield_*`, `jit_getfield` and
    /// `jit_putstatic_*` each have a specific argument for why they cannot
    /// safepoint. `jit_new_object` and friends have no such argument — they
    /// allocate, which can trigger a collection, which can MOVE every object
    /// this frame holds. Three things follow, and none of them is a flag on
    /// the existing sequence:
    ///
    /// * the reference operands must be in their depth slots AND NAMED, so the
    ///   collector rewrites them rather than merely marking them;
    /// * the reference LOCALS must be out of X19-X28 and in their safepoint
    ///   homes, for the same reason — a callee-saved register survives the
    ///   call, but inside the CALLEE's save area, where only a conservative
    ///   walk sees it, and a conservative walk cannot rewrite a relocated
    ///   object;
    /// * the reload afterwards is not housekeeping. It is what carries the
    ///   moved object's new address back into the register the model says it
    ///   is in.
    ///
    /// That is exactly the loop-header poll's sequence, and it is literally
    /// the same code: both call [`Self::spill_operands_for_call`],
    /// [`Self::home_reference_locals_for_call`], [`Self::stamp_safepoint_id`]
    /// and [`Self::reload_after_safepoint`].
    ///
    /// # What the caller still owes
    ///
    /// * **`into` is allocated by the caller, before any branch**, exactly as
    ///   for `emit_helper_call` — and this refuses one that still holds a live
    ///   operand, for the same reason.
    /// * **No reference may be live only in an ARGUMENT register.** X0-X7 are
    ///   outside the map, so a reference handed to the helper and needed after
    ///   it would come back stale if the collector moved it. Every caller here
    ///   passes only the context pointer (not a Java object), immediates, and
    ///   values it does not use again.
    /// * **The result is not covered either.** It lands in X0 and moves to
    ///   `into`, both outside the map — which is sound only because the map is
    ///   recorded at the return address, i.e. for the window in which the
    ///   helper runs, and by then the result is a brand-new object no other
    ///   thread can reach.
    ///
    /// Answers `false`, having set `self.failed`, when it refuses. A
    /// compilation whose frame reserved no safepoint homes refuses here rather
    /// than emitting a call with a map that cannot name the locals.
    fn emit_helper_call_at_safepoint(
        &mut self,
        helper: usize,
        args: &[Arm64HelperArg],
        into: Option<Arm64Register>,
        safepoint_id: u32,
    ) -> bool {
        self.emit_helper_call_at_safepoint_consuming(helper, args, into, safepoint_id, 0)
    }

    /// [`Self::emit_helper_call_at_safepoint`] for a call that CONSUMES the
    /// top `consumed` operand entries (round 9 wave 22).
    ///
    /// `invoke*` is the only caller, and it needs all three halves of this:
    ///
    /// * the arguments must be IN THE FRAME when the call happens, because the
    ///   helper reads them out of it -- which the spill does for every entry
    ///   the model holds, at each entry's own depth slot;
    /// * each reference among them must be NAMED, which the map does for the
    ///   same reason and by the same route, so a collector that moves one
    ///   during the callee hands the helper its new address;
    /// * and none of them may be RELOADED afterwards, because they are gone.
    ///   Reloading `n` dead values would not be merely wasteful: `invoke*` can
    ///   consume seven entries, which is the whole scratch pool, and the
    ///   reload would restore them over the registers the result needs.
    ///
    /// So the entries are dropped from the model after the map is recorded and
    /// before the reload, and the reload list is truncated to the survivors.
    /// `stored` is built in ascending depth order, so the survivors are
    /// exactly its prefix -- `keep` is counted before the spill for that
    /// reason, from the depths that will still exist.
    fn emit_helper_call_at_safepoint_consuming(
        &mut self,
        helper: usize,
        args: &[Arm64HelperArg],
        into: Option<Arm64Register>,
        safepoint_id: u32,
        consumed: usize,
    ) -> bool {
        if helper == 0 {
            self.failed = true;
            return false;
        }
        if args.len() > Arm64EntryConvention::INT_ARG_REGS.len() {
            self.failed = true;
            return false;
        }
        for arg in args {
            if let Arm64HelperArg::Reg(reg) = *arg {
                if !SCRATCH_REGS.contains(&reg) {
                    self.failed = true;
                    return false;
                }
            }
        }
        if let Some(dst) = into {
            if !SCRATCH_REGS.contains(&dst) || self.reg_is_live(dst) {
                self.failed = true;
                return false;
            }
        }
        if self.sp_id_slot_off == 0 {
            // No id word means `active_safepoint_id` answers `None` at run
            // time and no map can be selected, so the map this call is about
            // to record would be unreachable. The frame reserves the word for
            // any method that allocates; reaching here means it did not.
            self.failed = true;
            return false;
        }

        let Some(surviving) = self.operand_stack.len().checked_sub(consumed) else {
            self.failed = true;
            return false;
        };
        // Counted BEFORE the spill, over the depths that outlive the call.
        let keep = (0..surviving)
            .filter(|&d| matches!(self.operand_stack[d].loc, OperandLoc::Reg(_)))
            .count();
        let Some(mut stored) = self.spill_operands_for_call() else {
            return false;
        };
        let Some(reg_homed) = self.home_reference_locals_for_call(false) else {
            return false;
        };
        self.stamp_safepoint_id(safepoint_id);

        for (i, arg) in args.iter().enumerate() {
            let dst = Arm64EntryConvention::INT_ARG_REGS[i];
            match *arg {
                Arm64HelperArg::Reg(src) => {
                    self.buffer.emit(Arm64Instruction::Mov { rd: dst, rm: src });
                }
                Arm64HelperArg::Imm(imm) => {
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm });
                }
            }
        }
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: helper as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        self.emit_oop_map_for_safepoint(safepoint_id);
        if self.failed {
            return false;
        }
        if let Some(dst) = into {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: dst,
                rm: Arm64Register::X0,
            });
        }
        // The consumed entries die HERE: after the map named them (they were
        // live for the whole call) and before the reload would resurrect them.
        if consumed > 0 {
            self.operand_stack.truncate(surviving);
            stored.truncate(keep);
        }
        self.reload_after_safepoint(&reg_homed, stored);
        true
    }

    // -- Instance fields ----------------------------------------------------

    /// JVMS §6.5 `putfield` narrowing, for a field whose declared type is
    /// narrower than the `int` on the operand stack.
    ///
    /// Exactly x64's `emit_narrow_to_field_tag`, instruction for instruction:
    /// `B` and `S` sign-extend their low 8 / 16 bits, `C` keeps its low 16
    /// unsigned, and `Z` keeps one bit. `I`, `J`, `F` and `D` owe nothing.
    ///
    /// javac already narrows (`i2b`, `i2s`, `i2c`, a 0/1 `boolean`), so for
    /// javac output every one of these is a no-op ON THE VALUE -- but the
    /// helper writes a whole `Value::Int` into a legacy cell, and a
    /// handwritten or rewritten class file that skips the conversion would
    /// otherwise store a `byte` of 300.
    fn emit_narrow_to_field_tag(&mut self, reg: Arm64Register, type_tag: u8) {
        let inst = match type_tag {
            b'B' => Arm64Instruction::Sxtb { rd: reg, rn: reg },
            b'S' => Arm64Instruction::Sxth { rd: reg, rn: reg },
            b'C' => Arm64Instruction::AndImm {
                rd: reg,
                rn: reg,
                imm: 0xFFFF,
            },
            b'Z' => Arm64Instruction::AndImm {
                rd: reg,
                rn: reg,
                imm: 1,
            },
            _ => return,
        };
        self.buffer.emit(inst);
    }

    /// `putfield` of a REFERENCE field, round 9 wave 19.
    ///
    /// ```text
    ///   CBZ   Xobj, npe_NONE
    ///   LDR   Xctx, [FP, #context]
    ///   DMB   ISH                         ; volatile only
    ///   <emit_helper_call(jit_putfield_object, [ctx, obj, field_index, val])>
    ///   DMB   ISH                         ; volatile only
    /// ```
    ///
    /// # What the helper buys, and why nothing here is inline
    ///
    /// A reference store owes two barriers this backend emits nowhere: the
    /// SATB pre-write barrier (log the OLD value, or a concurrent marker loses
    /// the only path to a still-live object — a use-after-free on the next
    /// evacuation) and the card write-barrier that keeps the remembered set
    /// right. It also owes whatever ENCODING the collector is using: a
    /// compressed-oops slot is a 4-byte `(addr - base) >> 3`, and an armed ZGC
    /// slot is a coloured word that is not an address. `jit_putfield_object`
    /// does all of it; x64 has inline fast paths for the cases it can prove
    /// barrier-free and falls back to this same helper otherwise.
    ///
    /// # Not a safepoint
    ///
    /// The barriers enqueue and mark; they run no Java and allocate nothing on
    /// the Java heap, so nothing here can move an object. x64 records no oop
    /// map at this call either — unlike its `putstatic_object` sibling, which
    /// gets one because `<clinit>` can run inside it. This is the
    /// per-helper judgement [`Self::emit_helper_call`] asks every caller to
    /// make rather than assume.
    ///
    /// The null check and the fences are the primitive path's, unchanged: the
    /// helper's own receiver guard returns WITHOUT raising, and a `volatile`
    /// reference field is as volatile as a `volatile` `int`.
    fn emit_putfield_reference(&mut self, field: Arm64InstanceField) -> bool {
        if self.helpers.putfield_object == 0 || !self.can_throw_npe() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        let Ok(index) = i64::try_from(field.field_index) else {
            return false;
        };
        let val = self.pop_kind(OperandKind::Ref);
        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.emit_null_check(obj, npe_action::NONE);
        let ctx = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        if !self.emit_helper_call(
            self.helpers.putfield_object,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(index),
                Arm64HelperArg::Reg(val),
            ],
            None,
        ) {
            return false;
        }
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        true
    }

    /// `putfield` (`0xb5`) of a PRIMITIVE field, round 9 wave 15 -- the first
    /// lowering on this backend to call a helper from the middle of a method.
    ///
    /// ```text
    ///   CBZ   Xobj, npe_NONE              ; JVMS: a null receiver throws NPE
    ///   FMOV  Xval, Dval                  ; F/D only: the helpers take bits
    ///   <narrow the value to the field's declared type>
    ///   DMB   ISH                         ; volatile only -- see below
    ///   <emit_helper_call(jit_putfield_*, [obj, field_index, val])>
    ///   DMB   ISH                         ; volatile only
    /// ```
    ///
    /// # Why the helper at all, when the store itself is one instruction
    ///
    /// An instance field has TWO layouts per OBJECT -- the packed compact one
    /// and the legacy 16-byte tagged `Value` cell -- chosen at allocation
    /// time, plus a layout epoch the class manager can bump. x64 emits an
    /// inline store with a runtime branch and falls back to this same helper
    /// on every path it declines, one of which is "a compact receiver at a
    /// site with no baked compact offset", where no address is computable at
    /// all. A lowering with no fallback would have to be total for every
    /// layout, and for that receiver it cannot be. The inline fast path is a
    /// later wave; the helper is what makes one possible, because the
    /// fallback has to exist before the branch that needs it.
    ///
    /// # The null check is ours, not the helper's
    ///
    /// `jit_putfield_int`'s receiver guard (`!plausible_heap_pointer(obj_ptr)
    /// -> return`) exists to avoid dereferencing garbage, and it returns
    /// WITHOUT raising: a `putfield` on null through it drops the store and
    /// carries on. x64 emits its own null check ahead of the call for exactly
    /// this reason (`emit_precise_null_check_field_store`, with
    /// `npe_action::NONE` -- JEP 358 has no field-access action code), and so
    /// does this. Hence the [`Self::can_throw_npe`] gate: without an exact NPE
    /// path the method is refused rather than compiled with a `putfield` that
    /// silently does nothing.
    ///
    /// # `DMB ISH` on BOTH sides of a volatile store, which x64 does not owe
    ///
    /// This is the part that does not carry over from the x64 tier. There the
    /// store is a plain `MOV` and TSO makes it a release already, so a
    /// volatile `putfield` owes only the trailing StoreLoad edge (`MFENCE`,
    /// `x64::BackendRequest::volatile_field_pcs`). Here the store happens
    /// INSIDE the helper, where it is a relaxed store
    /// (`write_compact_field(.., Ordering::Relaxed)`), and a `BLR` is not a
    /// barrier on AArch64: without a leading `DMB ISH` the helper's store may
    /// become visible before a Java store that precedes it in program order,
    /// which is precisely the release edge the JMM requires. The trailing one
    /// is the StoreLoad edge, the same one x64 pays.
    ///
    /// The instructions the leading fence ends up preceding -- the operand
    /// spills `emit_helper_call` emits, the argument moves -- are stores to
    /// THIS thread's own frame, which the fence orders harmlessly; what
    /// matters is that every Java-visible access before this bytecode is on
    /// the far side of it.
    ///
    /// A REFERENCE field is not here: it goes to
    /// [`Self::emit_putfield_reference`], which calls the helper that runs the
    /// SATB pre-barrier and the card mark.
    ///
    /// Returns `false`, and the caller refuses the method, for an unwired
    /// helper or no exact NPE path.
    fn emit_putfield(&mut self, field: Arm64InstanceField) -> bool {
        if matches!(field.type_tag, b'L' | b'[') {
            return self.emit_putfield_reference(field);
        }
        let Some(kind) = primitive_field_operand_kind(field.type_tag) else {
            return false;
        };
        let helper = match field.type_tag {
            b'J' => self.helpers.putfield_long,
            b'F' => self.helpers.putfield_float,
            b'D' => self.helpers.putfield_double,
            _ => self.helpers.putfield_int,
        };
        if helper == 0 {
            return false;
        }
        if !self.can_throw_npe() {
            return false;
        }
        let Ok(index) = i64::try_from(field.field_index) else {
            return false;
        };
        // JVMS operand order: the receiver is UNDER the value.
        let val = self.pop_kind(kind);
        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.emit_null_check(obj, npe_action::NONE);
        // All four helpers take `(obj_ptr: i64, field_index: i64, val: i64)`
        // and reinterpret the bits (`f32::from_bits(val as u32)`), so a float
        // or double argument travels in a GPR, not in V0-V7.
        let val = match kind {
            OperandKind::F32 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFpSingle { rd: g, vn: val });
                g
            }
            OperandKind::F64 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFp { rd: g, vn: val });
                g
            }
            _ => val,
        };
        if self.failed {
            return false;
        }
        self.emit_narrow_to_field_tag(val, field.type_tag);
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        // No oop map: `jit_putfield_*` allocates nothing, runs no Java and
        // cannot safepoint, so the reference operands this call spills and the
        // reference locals it leaves in X19-X28 are not relocatable behind its
        // back. See `emit_helper_call`, which states that obligation.
        if !self.emit_helper_call(
            helper,
            &[
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(index),
                Arm64HelperArg::Reg(val),
            ],
            None,
        ) {
            return false;
        }
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        true
    }

    /// `getfield` (`0xb4`), round 9 wave 16 -- the first lowering that needs
    /// the VM context, and the first with a sentinel to disambiguate.
    /// Reference fields since h23 (2026-09-22); see below.
    ///
    /// ```text
    ///   CBZ   Xobj, npe_NONE              ; JVMS: a null receiver throws NPE
    ///   LDR   Xctx, [FP, #context]        ; the VM pointer the prologue homed
    ///   <emit_helper_call(jit_getfield, [ctx, obj, field_index]) -> Xv>
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xv, Xs
    ///   B.NE  join                        ; the common case: a real value
    ///   <J/D only: call dispatch_threw, and keep the value when it says 0>
    ///   <emit_stamp_throw_bci(bci)>       ; so the drain finds the handler
    ///   MOV   X0, #i64::MIN               ; a pending exception -- leave
    ///   B     epilogue
    /// join:
    ///   SXTW/FMOV                         ; into this backend's canonical form
    /// ```
    ///
    /// # Reference fields (h23, 2026-09-22)
    ///
    /// `L` and `[` were refused here until h23, which was two separate
    /// refusals wearing one coat. The HELPER has handled reference fields all
    /// along -- `jit_getfield` takes `GETFIELD_EXPECT_REFERENCE` in its third
    /// argument and returns the oop -- and the operand model has had
    /// `OperandKind::Ref` since wave 10. What was missing was only this arm
    /// passing the flag and marking the result. Both are now done through the
    /// canonical `getfield_index_arg` encoder rather than the hand-written
    /// `field_index as i64` this site used to build, which is precisely the
    /// rot that encoder's doc comment warns about.
    ///
    /// A reference needs no `dispatch_threw` probe: only `J`/`D` can hold
    /// `i64::MIN` legitimately. It also needs no load barrier, for the same
    /// reason the reference `getstatic` arm needs none -- the helper returns
    /// what every collector in this VM lets compiled code hold.
    ///
    /// # Why the read is a helper call and not two loads
    ///
    /// The same reason `putfield`'s store is: an instance field has two
    /// layouts per OBJECT (packed compact, or the legacy 16-byte tagged
    /// `Value` cell) plus a layout epoch, and x64's inline read declines to a
    /// helper on four paths, one of which -- a compact receiver at a site with
    /// no baked compact offset -- has no computable address at all. A lowering
    /// with no fallback would have to be total for every layout, and there it
    /// cannot be.
    ///
    /// # Why the null check is still ours
    ///
    /// `jit_getfield` DOES raise on a null receiver (unlike `jit_putfield_*`,
    /// which silently drops the store) -- it flags the pending NPE and returns
    /// the sentinel. Checking first is not redundant, though: it keeps the
    /// common null out of the helper, and it means the sentinel path below is
    /// reached only by a receiver that is non-null and implausible, or by a
    /// panic inside the helper.
    ///
    /// # The sentinel, and why `J` and `D` need a second call
    ///
    /// `jit_getfield` returns `i64::MIN` for "an exception is pending" -- and
    /// it returns it ONLY after setting one (`set_jit_pending_npe_action_at`
    /// for an implausible receiver, `set_jit_pending_npe` for one that is not
    /// a live object; an out-of-range slot returns 0, not the sentinel). So
    /// leaving through the epilogue with the sentinel here raises an exception
    /// rather than re-running the method, which is why this lowering does not
    /// need the deopt point that `uncommon_trap`-style bails would.
    ///
    /// For an `int`, `boolean`, `byte`, `char`, `short` or `float` field,
    /// `X0 == i64::MIN` is therefore unambiguous: the helper returns
    /// `i as i64` for an `Int` (sign-extended, never `i64::MIN`) and
    /// `f.to_bits() as i64` for a `Float` (zero-extended 32 bits, likewise).
    ///
    /// **`J` and `D` are ambiguous, and getting this wrong is a miscompile
    /// with no crash.** A `long` field holding `Long.MIN_VALUE` returns
    /// exactly `i64::MIN`, and so does a `double` field holding `-0.0`, whose
    /// bit pattern IS `i64::MIN`. Reading either as "an exception is pending"
    /// throws a `NullPointerException` out of a correct program. So on the
    /// (rare) equal branch those two call `dispatch_threw`, which PEEKS every
    /// out-of-band signal and answers 1 only for a genuine one -- the same
    /// helper and the same reason as x64's `J`/`D` call sites. The value is
    /// re-materialized rather than preserved across that call, because on this
    /// branch it is `i64::MIN` by definition.
    ///
    /// Every `alloc_reg` happens BEFORE the branch, so both paths reach `join`
    /// with the same operand model. That is the rule `emit_helper_call`'s doc
    /// states, and this is the lowering it was written for.
    ///
    /// Returns `false`, and the caller refuses the method, for a reference
    /// field (no narrow-oop decode and no operand-stack oop story), an unwired
    /// helper, no exact NPE path, or no context word.
    fn emit_getfield(&mut self, field: Arm64InstanceField) -> bool {
        let is_reference = matches!(field.type_tag, b'L' | b'[');
        let kind = if is_reference {
            OperandKind::Ref
        } else {
            let Some(kind) = primitive_field_operand_kind(field.type_tag) else {
                return false;
            };
            kind
        };
        if self.helpers.getfield == 0 || !self.can_throw_npe() {
            return false;
        }
        // `needs_context` is set from the presence of a `0xb4` in the
        // bytecode, so reaching here without a context word would mean the
        // frame and the walk disagree.
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Only `J`/`D` can legitimately BE `i64::MIN` and so need the
        // `dispatch_threw` probe to tell a real value from a pending
        // exception. A reference cannot: `i64::MIN` is not a mapped address
        // on any target this VM runs, and the helper returns 0 for null.
        let ambiguous = matches!(field.type_tag, b'J' | b'D');
        if ambiguous && self.helpers.dispatch_threw == 0 {
            return false;
        }
        // h23: through the ONE encoder, not `field_index as i64` by hand.
        // `GETFIELD_EXPECT_REFERENCE` is what stops the helper handing back
        // the payload of whatever `Value` variant the slot holds -- a
        // reference field punned to a primitive becomes a wild pointer, and
        // this arm is about to push the result as an oop. The receiver proof
        // is left `false`: this backend's null check is not the
        // three-clause plausibility screen x64's `receiver_proven_oop`
        // means, so claiming it would be a lie the helper acts on.
        // `npe_trap_key` is 0 -- JEP 358 messages are an x64-only feature so
        // far -- which is the documented "site not described" value.
        let Ok(slot) = u32::try_from(field.field_index) else {
            return false;
        };
        let Ok(index) =
            i64::try_from(cratonvm_jit_api::getfield_index_arg(slot, is_reference, false, 0))
        else {
            return false;
        };

        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.emit_null_check(obj, npe_action::NONE);

        // EVERY allocation, before the branch. See the doc comment.
        let value = self.alloc_reg(false);
        let ctx = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        let probe = if ambiguous {
            Some(self.alloc_reg(false))
        } else {
            None
        };
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        // No oop map: `jit_getfield` allocates nothing and runs no Java, so it
        // cannot safepoint. See `emit_helper_call`, which states that
        // obligation rather than assuming it.
        if !self.emit_helper_call(
            self.helpers.getfield,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(index),
            ],
            Some(value),
        ) {
            return false;
        }

        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: value,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        if let Some(probe) = probe {
            let leave = self.buffer.new_label();
            if !self.emit_helper_call(self.helpers.dispatch_threw, &[], Some(probe)) {
                return false;
            }
            self.buffer.emit(Arm64Instruction::Cbnz {
                rt: probe,
                label: leave,
            });
            // Not an exception: the `i64::MIN` is this field's real value.
            // Re-materialized rather than preserved, because the call above
            // destroyed `value` -- and on this branch it can only have been
            // `i64::MIN`.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: value,
                imm: i64::MIN,
            });
            self.buffer.emit(Arm64Instruction::B { label: join });
            self.buffer.bind_label(leave);
        }
        // A pending exception. The interpreter's JIT-return drain raises it
        // and searches THIS method's exception table from the throw-site bci,
        // so the bci has to be stamped first.
        //
        // h23 (2026-09-22): this edge was missed when `can_throw_npe` was
        // widened from `exception_table_empty` to `can_route_exception`. The
        // inline null check above leaves through the NPE stub, which is
        // bci-keyed and stamps; THIS edge -- the helper's own `i64::MIN`,
        // raised for a receiver the inline check passed -- left with whatever
        // bci a previous stamp had written, which in a method with a
        // `try`/`catch` is the wrong-handler bug the stamp exists to prevent.
        // `emit_putfield`/`emit_putfield_reference` need no such call: they
        // ignore the helper's result and their only trap is that same stub.
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        match kind {
            OperandKind::Ref => {
                self.push_reg(OperandKind::Ref, value);
                self.mark_top_operand_as_oop();
            }
            OperandKind::I32 => {
                // The helper already returns a sign-extended `int`, so this is
                // a no-op on every value it can produce; it is emitted so the
                // canonical form is established HERE rather than inferred from
                // the helper's `i as i64`.
                self.emit_sxtw(value);
                self.push_reg(OperandKind::I32, value);
            }
            OperandKind::F32 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: value });
                self.push_reg(OperandKind::F32, v);
            }
            OperandKind::F64 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: value });
                self.push_reg(OperandKind::F64, v);
            }
            // `I64` (the only other kind the table produces).
            _ => self.push_reg(kind, value),
        }
        true
    }

    // -- Reference array elements -------------------------------------------

    /// Ensure the operand `from_top` entries below the top is in a register,
    /// and answer that register — WITHOUT popping it.
    ///
    /// The difference from [`Self::pop_entry`] is what makes a value survive a
    /// call: [`Self::spill_operands_for_call`] stores and reloads exactly the
    /// entries the model still holds, so an operand left on the stack comes
    /// back in the same register on the far side, while one that was popped is
    /// simply destroyed. `aastore` needs both its array and its value across
    /// TWO calls (the type check and the store), which is what this exists
    /// for.
    ///
    /// `None` (with `failed` set) on underflow.
    fn materialize_entry(&mut self, from_top: usize) -> Option<(Arm64Register, Operand)> {
        let Some(depth) = self.operand_stack.len().checked_sub(from_top + 1) else {
            self.failed = true;
            return None;
        };
        let entry = self.operand_stack[depth];
        match entry.loc {
            OperandLoc::Reg(reg) => {
                // HELD, like `pop_entry`'s. Being live on the stack protects
                // this register from `alloc_reg`'s free-register pass but NOT
                // from its victim pass, which spills the deepest
                // register-located entry -- and a caller that has already
                // captured the register would then be holding a name the
                // model no longer agrees with. Holding makes a later
                // allocation refuse instead.
                if !self.held.contains(&reg) {
                    self.held.push(reg);
                }
                Some((reg, entry))
            }
            OperandLoc::Slot(offset) => {
                let reg = self.alloc_reg(entry.kind.is_fp());
                if self.failed {
                    return None;
                }
                self.emit_load_kind(reg, entry.kind, offset);
                self.operand_stack[depth].loc = OperandLoc::Reg(reg);
                Some((reg, entry))
            }
        }
    }

    /// Leave the frame with the `i64::MIN` deopt sentinel when `reg` holds it
    /// — the shape every helper here that signals through the sentinel shares.
    ///
    /// ```text
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xreg, Xs
    ///   B.NE  join
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    /// ```
    ///
    /// `sentinel` is a register the CALLER allocated, for the same reason
    /// `emit_helper_call`'s destination is: this sits between a call and a
    /// join, and an allocation here would happen on one path only.
    fn emit_bail_on_sentinel(&mut self, reg: Arm64Register, sentinel: Arm64Register) {
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: reg,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);
    }

    /// Leave the frame when `reg` is NON-ZERO — the shape for a helper whose
    /// result is binary.
    ///
    /// ```text
    ///   CBZ   Xreg, join
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    /// ```
    ///
    /// Used only by `aastore`'s element-type check, whose contract is `0` for
    /// a legal store and `i64::MIN` for a refused one and nothing else. That
    /// is what makes `CBZ` as precise as the `CMP` against the sentinel that
    /// [`Self::emit_bail_on_sentinel`] emits, and one register cheaper -- and
    /// the difference matters at this bytecode, which already holds the array,
    /// the index, the value, the length the AIOOBE stub reads, the context and
    /// the verdict at the same time.
    ///
    /// It is NOT interchangeable with the other form. `aaload`'s result is a
    /// REFERENCE, where every non-zero value but the sentinel is a legal one.
    fn emit_bail_if_refused(&mut self, reg: Arm64Register) {
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: reg,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);
    }

    /// `aaload` (`0x32`), round 9 wave 19 — the reference element LOAD.
    ///
    /// ```text
    ///   <the wave-13 guards: CBZ arr, npe; unsigned bounds compare, B.HS aioobe>
    ///   LDR   Xctx, [FP, #context]
    ///   <emit_helper_call_at_safepoint(jit_aaload, [ctx, arr, idx]) -> Xv>
    ///   <bail if Xv is the sentinel>
    ///   <push Xv as a REFERENCE, marked as an oop>
    /// ```
    ///
    /// # Why a helper, when every other array load here is one instruction
    ///
    /// Because a reference element is not always a pointer. Under compressed
    /// oops it is a 4-byte `(addr - base) >> 3` with 0 for null; under an
    /// ARMED ZGC cycle it is a COLOURED word — tag bit 63, colour bits
    /// 42..=46, a 42-bit offset — which is not a machine address at all, and
    /// reading it without the load barrier is a read-barrier hole (the
    /// hazard x64's `0x53` arm documents at length for the store side). x64
    /// lowers this inline because its emitters know both encodings; this
    /// backend knows neither, and `jit_aaload` knows both.
    ///
    /// The guards are still OURS, because the helper's own are not exact: it
    /// reports a null array or a bad index through the pending-signal channel,
    /// which surfaces only at method return. Emitting them inline keeps the
    /// `NullPointerException` and the `ArrayIndexOutOfBoundsException` at the
    /// bytecode that caused them.
    ///
    /// Returns `false`, and the caller refuses the method, without the array
    /// exception paths, an unwired helper, a context word or an empty
    /// exception table.
    fn emit_aaload(&mut self) -> bool {
        // `can_access_array` already folds in `can_route_exception` through
        // `can_throw_npe`/`can_throw_aioobe`; no separate check is owed here.
        if !self.can_access_array() {
            return false;
        }
        if self.helpers.aaload == 0 {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        let idx = self.pop_kind(OperandKind::I32);
        let arr = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        // The same guards every other array access emits. The address it
        // computes is discarded: the helper addresses the element itself,
        // because only it knows the element's encoding.
        if !self.emit_array_guards_only(arr, idx, npe_action::ALOAD_OBJECT) {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let out = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            self.helpers.aaload,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(arr),
                Arm64HelperArg::Reg(idx),
            ],
            Some(out),
            safepoint_id,
        ) {
            return false;
        }
        // `i64::MIN` cannot be a reference -- a heap pointer is under 2^48 --
        // so the sentinel is unambiguous here, unlike a `long` field's.
        self.emit_bail_on_sentinel(out, sentinel);
        self.push_reg(OperandKind::Ref, out);
        self.mark_top_operand_as_oop();
        true
    }

    /// `aastore` (`0x53`), round 9 wave 19 — the reference element STORE, and
    /// the first lowering here to make TWO calls at one bytecode.
    ///
    /// ```text
    ///   <the wave-13 guards: CBZ arr, npe; unsigned bounds compare, B.HS aioobe>
    ///   LDR   Xctx, [FP, #context]
    ///   <call_at_safepoint(jit_aastore_type_check, [ctx, arr, val]) -> Xr>
    ///   <bail if Xr is the sentinel: an ArrayStoreException is pending>
    ///   LDR   Xctx, [FP, #context]        ; the first call destroyed it
    ///   <call_at_safepoint(jit_aastore, [ctx, arr, idx, val])>
    /// ```
    ///
    /// # Why two calls, and why the array and the value are not popped first
    ///
    /// The store itself owes the SATB pre-write barrier, the card mark and
    /// whatever encoding the collector is using — all of which `jit_aastore`
    /// does and none of which this backend emits anywhere. But `jit_aastore`
    /// returns `()`, so a compiled caller cannot see that it refused the
    /// store: its `ArrayStoreException` travels by the pending-signal channel
    /// and would surface only at method return, after this frame had run on.
    /// `jit_aastore_type_check` exists for exactly that — it answers `0` or
    /// the sentinel — and x64's inline lowering calls it first for the same
    /// reason.
    ///
    /// Two calls means the array and the value have to survive the first one.
    /// They are therefore LEFT ON THE OPERAND STACK and reached with
    /// [`Self::materialize_entry`]: the spill-and-reload around a call
    /// preserves exactly the entries the model still holds, so both come back
    /// in the same registers — and, being reference operands, both are named
    /// in the map, so a collector that moves them during the type check hands
    /// the store their new addresses. A popped value would have neither
    /// property.
    ///
    /// Returns `false`, and the caller refuses the method, without the array
    /// exception paths, either helper, a context word or an empty exception
    /// table.
    fn emit_aastore(&mut self) -> bool {
        // `can_access_array` already folds in `can_route_exception` through
        // `can_throw_npe`/`can_throw_aioobe`; no separate check is owed here.
        if !self.can_access_array() {
            return false;
        }
        if self.helpers.aastore == 0 || self.helpers.aastore_type_check == 0 {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        // Top down: value, index, array -- and NOT popped. See the doc.
        let Some((val, val_entry)) = self.materialize_entry(0) else {
            return false;
        };
        let Some((idx, idx_entry)) = self.materialize_entry(1) else {
            return false;
        };
        let Some((arr, arr_entry)) = self.materialize_entry(2) else {
            return false;
        };
        if val_entry.kind != OperandKind::Ref
            || idx_entry.kind != OperandKind::I32
            || arr_entry.kind != OperandKind::Ref
        {
            self.failed = true;
            return false;
        }
        if !self.emit_array_guards_only(arr, idx, npe_action::ASTORE_OBJECT) {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`. Note
        // there is no sentinel register here, unlike `aaload`'s: the type
        // check's result is BINARY by contract (`0` legal, `i64::MIN`
        // refused), so `CBNZ` decides it, and this bytecode needs the
        // register -- it holds the array, the index, the value, the length
        // the AIOOBE stub reads, the context and the verdict at once.
        let ctx = self.alloc_reg(false);
        let verdict = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            self.helpers.aastore_type_check,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(arr),
                Arm64HelperArg::Reg(val),
            ],
            Some(verdict),
            safepoint_id,
        ) {
            return false;
        }
        self.emit_bail_if_refused(verdict);
        // The type check destroyed X9-X15 and the reload put back only what
        // the operand MODEL holds; the context is not an operand.
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            self.helpers.aastore,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(arr),
                Arm64HelperArg::Reg(idx),
                Arm64HelperArg::Reg(val),
            ],
            None,
            safepoint_id,
        ) {
            return false;
        }
        // Only now: the three entries this bytecode consumed.
        self.drop_top();
        self.drop_top();
        self.drop_top();
        !self.failed
    }

    // -- Throw --------------------------------------------------------------

    /// `athrow` (`0xbf`), round 9 wave 20.
    ///
    /// ```text
    ///   MOV       X0, Xexc
    ///   MOVZ/MOVK X1, #bci
    ///   MOVZ/MOVK X16, #jit_throw_exception
    ///   BLR       X16                 ; returns the i64::MIN deopt sentinel
    ///   B         epilogue            ; X0 already holds it
    /// ```
    ///
    /// The simplest call shape this backend has, and the one it has had since
    /// wave 11: a stub on the way OUT of the frame. Nothing has to survive it,
    /// so there is no spill, no reload and no oop map -- the operand stack is
    /// dead the moment the exception is raised, and the epilogue restores the
    /// callee-saved homes. What is new in wave 20 is only that the arm exists.
    ///
    /// `jit_throw_exception` stashes the exception as pending and answers the
    /// sentinel, which the interpreter's JIT-return drain turns into the
    /// throw; a NULL `exc_ptr` gets the JVMS `athrow`-on-null
    /// `NullPointerException` from the helper itself, so no inline check is
    /// owed. The `bci` argument is what lets the drain place the throw.
    ///
    /// Unlike every other trapping lowering here, this one needs no
    /// [`Self::can_route_exception`] check: `jit_throw_exception`'s own `bci`
    /// argument reaches `set_jit_pending_exception_with_bci`
    /// (`vm/src/jit/helpers.rs`), the same TLS cell [`Self::emit_stamp_throw_bci`]
    /// writes for everything else, so the drain always has an exact throw
    /// site to route through this method's own exception table -- table empty
    /// or not.
    ///
    /// No fence: `athrow` publishes nothing. The exception object was already
    /// published by whatever created it.
    fn emit_athrow(&mut self) -> bool {
        if self.helpers.throw_exception == 0 {
            return false;
        }
        let Ok(bci) = i64::try_from(self.cur_bytecode_pc) else {
            return false;
        };
        let exc = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Mov {
            rd: Arm64Register::X0,
            rm: exc,
        });
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X1,
            imm: bci,
        });
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.throw_exception as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        // The helper RETURNS the sentinel in X0, so unlike the NPE stub this
        // does not have to materialize one.
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.reachable = false;
        true
    }

    /// `multianewarray` (`0xc5`) at ANY arity, h23c (2026-09-22).
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   SUB   Xdims, FP, #|offset of the deepest dimension's slot|
    ///   <emit_helper_call_at_safepoint_consuming(multianewarray_n,
    ///        [ctx, site, ndims, dims], consumed = ndims) -> Xout>
    ///   CBNZ  Xout, join               ; 0 = a pending exception
    ///   <emit_stamp_throw_bci(bci)>
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    ///   <push Xout as a REFERENCE, marked as an oop>
    /// ```
    ///
    /// # The dimension buffer is the OPERAND AREA, and costs nothing
    ///
    /// x64 has to copy its dimension counts into frame scratch words before
    /// this call, because its operand stack is register-cached and its
    /// argument registers are four wide. Neither is true here. The helper
    /// reads `dims_ptr[0..ndims]` outermost-first from ascending addresses;
    /// this frame's operand words are indexed by depth and ascend in address
    /// with it (see the module header); and JVMS pushes a `multianewarray`'s
    /// dimensions in source order, so the OUTERMOST is the deepest of the
    /// `ndims` entries on top of the stack. The deepest one is therefore
    /// already `dims_ptr[0]`, and the spill that
    /// `emit_helper_call_at_safepoint_consuming` performs anyway is what
    /// writes the buffer.
    ///
    /// So the whole lowering is the one `SUB` that turns a frame offset into
    /// an address -- the same computation `emit_invoke` makes for
    /// `jit_invoke_dispatch`'s argument buffer, for the same reason.
    ///
    /// `consumed = ndims` is what makes the map exact: the entries are named
    /// by the oop map (a dimension is an `int`, but an entry BELOW them may be
    /// a reference), dropped from the model after it is taken, and not
    /// reloaded afterwards.
    ///
    /// # Arity
    ///
    /// Bounded by `MAX_JIT_MULTIANEWARRAY_DIMS`, which is `jit_scan`'s own
    /// bound, so a site that reached here with a wider arity would be one the
    /// scanner should already have refused. Checked rather than asserted: a
    /// `debug_assert!` is no guard in a release build, and the failure mode --
    /// consuming the wrong number of entries -- leaves the operand model out
    /// of step rather than faulting.
    fn emit_multianewarray(&mut self, ndims: usize) -> bool {
        if self.helpers.multianewarray_n == 0 {
            return false;
        }
        // The helper answers 0 with a pending exception (a negative dimension,
        // OOM, a failed resolution), and that leaves through the epilogue.
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };
        let Some(&site) = self.multianewarray_sites.get(&self.cur_bytecode_pc) else {
            return false;
        };
        if ndims == 0 || ndims > crate::x64::MAX_JIT_MULTIANEWARRAY_DIMS {
            return false;
        }
        let Some(surviving) = self.operand_stack.len().checked_sub(ndims) else {
            return false;
        };
        // Cast: bounded by MAX_JIT_MULTIANEWARRAY_DIMS just above.
        let Ok(ndims_imm) = i64::try_from(ndims) else {
            return false;
        };

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let dims_ptr = self.alloc_reg(false);
        let out = self.alloc_reg(false);
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        let Some(off) = self.spill_offset_for_depth(surviving) else {
            self.failed = true;
            return false;
        };
        // Frame offsets are negative from FP; the buffer grows upward in
        // address from this one word, exactly as `emit_invoke`'s does.
        let Some(magnitude) = off.checked_neg() else {
            self.failed = true;
            return false;
        };
        self.buffer.emit(Arm64Instruction::SubImm {
            rd: dims_ptr,
            rn: Arm64Register::FP,
            imm: magnitude,
        });

        cratonvm_jit_api::assert_helper_call_shape!(
            "multianewarray_n",
            int_args = 4,
            returns_value = true
        );
        if !self.emit_helper_call_at_safepoint_consuming(
            self.helpers.multianewarray_n,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(site),
                Arm64HelperArg::Imm(ndims_imm),
                Arm64HelperArg::Reg(dims_ptr),
            ],
            Some(out),
            safepoint_id,
            ndims,
        ) {
            return false;
        }

        // 0 (null) means the helper published a pending exception. Unlike the
        // `getfield` family there is no ambiguity to resolve: a successful
        // allocation never answers null.
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbnz {
            rt: out,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        self.push_reg(OperandKind::Ref, out);
        self.mark_top_operand_as_oop();
        true
    }

    // -- Monitors -----------------------------------------------------------

    /// `monitorenter` (`0xc2`) and `monitorexit` (`0xc3`), h23 (2026-09-22) —
    /// the fifth page's "monitors are dead code" item.
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   <emit_helper_call_at_safepoint(jit_monitor_enter|exit, [ctx, obj]) -> Xr>
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xr, Xs
    ///   B.NE  join
    ///   <emit_stamp_throw_bci(bci)>
    ///   MOV   X0, #i64::MIN            ; a pending exception -- leave
    ///   B     epilogue
    /// join:
    /// ```
    ///
    /// # Why a helper call and not an inline lock-word CAS
    ///
    /// Because that is what the other tier that lowers these does.
    /// `ir_lower`'s `Op::MonitorEnter`/`Op::MonitorExit` arm is this same
    /// shape — publish the map, call `jit_monitor_enter`/`jit_monitor_exit`,
    /// test the sentinel — and the thin-lock CAS lives inside the helper,
    /// where the contended path, the inflation and the JVMTI accounting are.
    /// An inline uncontended fast path with the helper as its fallback is a
    /// real optimisation and belongs with the other §4 items on
    /// `aarch64-backend-runs-no-java-on-a-real-machine-20260922.md`; it is not
    /// what "monitors are unwritten" meant, and doing it first would have
    /// meant writing the hard half before the correct half.
    ///
    /// This is also why [`ARM64_LOWERS_EXCLUSIVE_ACCESS`] stays `false`: no
    /// `LDAXR`/`STLXR` is emitted here. That constant answers "which ordering
    /// instructions does this backend emit", and the honest answer is still
    /// none of the exclusive ones.
    ///
    /// # Ordering
    ///
    /// The acquire and the release are the helper's, and it is a `BLR` away
    /// behind a full compiler and hardware barrier set by
    /// `monitor_enter_blocking` / the unlock CAS. This arm owes nothing extra
    /// for the same reason the `invoke*` arms do not: no lowering on this
    /// backend ever leaves a fence owed across a call. See
    /// [`opcode_has_ordered_lowering`].
    ///
    /// # The receiver, and the address it may come back as
    ///
    /// `jit_monitor_enter` returns the possibly-REMAPPED object, because a
    /// contended acquire parks this thread and the object can move while it is
    /// parked. `ir_lower` stores that back into the receiver's slot; there is
    /// nothing to store it back INTO here, because `monitorenter` consumes its
    /// operand and this arm has already popped it. That is not a gap:
    ///
    /// * the popped register is dead after the call — nothing reads it again;
    /// * every OTHER live reference to the same object is either an operand,
    ///   which [`Self::emit_helper_call_at_safepoint`] spills and names in the
    ///   map, or a reference local, which `home_reference_locals_for_call`
    ///   homes and `reload_after_safepoint` reloads — both from slots a moving
    ///   collector rewrites. The `synchronized` block's own
    ///   `astore`d copy, which its generated handler unlocks through, is one
    ///   of those locals.
    ///
    /// So the object this frame goes on to use is read back from a slot the
    /// collector maintained, never from the pre-park address.
    ///
    /// # The sentinel
    ///
    /// `i64::MIN` means the helper published a pending Java exception: a null
    /// receiver's `NullPointerException` for `enter`, an
    /// `IllegalMonitorStateException` for an `exit` this thread does not own.
    /// Unambiguous, unlike a `J`/`D` field read — a reference is never
    /// `i64::MIN`, and `jit_monitor_exit` answers `1` on success. The bci is
    /// stamped before leaving so the drain searches this method's own
    /// exception table from the right site, which for a `synchronized` block
    /// is the whole point: its generated `any` handler is what must run.
    fn emit_monitor_op(&mut self, enter: bool) -> bool {
        let helper = if enter {
            self.helpers.monitor_enter
        } else {
            self.helpers.monitor_exit
        };
        if helper == 0 {
            return false;
        }
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let out = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            helper,
            &[Arm64HelperArg::Reg(ctx), Arm64HelperArg::Reg(obj)],
            Some(out),
            safepoint_id,
        ) {
            return false;
        }

        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: out,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);
        true
    }

    // -- Allocation ---------------------------------------------------------

    /// `new` (`0xbb`), `newarray` (`0xbc`) and `anewarray` (`0xbd`), round 9
    /// wave 18 — the first lowerings that call a helper which can SAFEPOINT.
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   <emit_helper_call_at_safepoint(alloc helper, args) -> Xobj>
    ///   CBNZ  Xobj, join            ; null means the helper stashed an error
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    ///   <push Xobj as a REFERENCE, marked as an oop>
    /// ```
    ///
    /// The three differ only in their helper and their middle argument:
    ///
    /// | opcode | helper | arguments |
    /// | --- | --- | --- |
    /// | `new` | `jit_new_object` | `(vm, class_id, num_fields)` |
    /// | `newarray` | `jit_newarray` | `(vm, atype, length)` |
    /// | `anewarray` | `jit_anewarray_object` | `(vm, component_class_id, length)` |
    ///
    /// `newarray` needs no constant-pool resolution at all: its `atype` is an
    /// immediate operand byte. The other two need the class the caller
    /// resolved into an [`Arm64NewSite`], and refuse a site without one.
    ///
    /// # Why no fence
    ///
    /// The allocation publishes a header, and the JMM's freeze action belongs
    /// to whatever later store makes the reference visible to another thread.
    /// On this backend that store is a `putfield`/`putstatic` through a
    /// helper, which carries its own ordering. Until then the object is
    /// unreachable by construction: nothing but this frame has its address.
    /// x64 emits no fence here either.
    ///
    /// # The null result
    ///
    /// Every one of these helpers returns `0` on failure, having stashed a
    /// pending exception first — `OutOfMemoryError`,
    /// `NegativeArraySizeException`, or whatever a failed `<clinit>` raised.
    /// So leaving through the epilogue with the sentinel THROWS rather than
    /// re-running the method, which is what makes this safe without a deopt
    /// point (the same argument as `getfield` and `putstatic`).
    /// [`Self::emit_stamp_throw_bci`] on that edge is what lets the drain
    /// route it through THIS method's own exception table rather than an
    /// unknown pc. x64 reaches the same place through
    /// `emit_post_alloc_oom_check`; without that check the null is pushed and
    /// the next `arraylength` or field access dereferences it.
    ///
    /// Returns `false`, and the caller refuses the method, for an unwired
    /// helper, an unresolved site, a `set_throw_bci` neither wired nor unneeded
    /// (see [`Self::can_route_exception`]), or no context word.
    fn emit_allocation(&mut self, opcode: u8, operand: u16) -> bool {
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which `compile_pass`
        // published before dispatching and which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        // What the helper is, and what its SECOND argument is. The third is
        // either an immediate (the field count) or the length operand.
        let site = self.new_sites.get(&self.cur_bytecode_pc).copied();
        let (helper, class_or_atype) = match opcode {
            0xbb => match site {
                Some(s) => (self.helpers.new_object, i64::from(s.class_id)),
                None => return false,
            },
            // `atype` is an immediate operand byte, so there is nothing to
            // resolve and nothing that can be missing.
            0xbc => (self.helpers.newarray, i64::from(operand)),
            0xbd => match site {
                Some(s) => (self.helpers.anewarray_object, i64::from(s.class_id)),
                None => return false,
            },
            _ => return false,
        };
        if helper == 0 {
            return false;
        }

        // The length operand, for the two array forms. Popped BEFORE the
        // allocations below, so the allocator can see the register it frees.
        let length = match opcode {
            0xbc | 0xbd => {
                let reg = self.pop_kind(OperandKind::I32);
                if self.failed {
                    return false;
                }
                Some(reg)
            }
            _ => None,
        };
        let third = match (opcode, length) {
            (0xbb, _) => {
                let Some(s) = site else { return false };
                let Ok(fields) = i64::try_from(s.num_fields) else {
                    return false;
                };
                Arm64HelperArg::Imm(fields)
            }
            (_, Some(reg)) => Arm64HelperArg::Reg(reg),
            // Unreachable: the match above gave every non-`new` opcode a
            // length. Refuse rather than fabricate an argument.
            _ => return false,
        };

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let obj = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            helper,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(class_or_atype),
                third,
            ],
            Some(obj),
            safepoint_id,
        ) {
            return false;
        }

        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbnz {
            rt: obj,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        self.push_reg(OperandKind::Ref, obj);
        // A fresh object IS a reference, and the oop marks are what every map
        // recorded after this point names its slot from.
        self.mark_top_operand_as_oop();
        true
    }

    // -- Invoke -------------------------------------------------------------

    /// Every `invoke*` form (`0xb6..=0xba`), round 9 wave 22 -- the first
    /// bytecode CALL this backend has ever compiled.
    ///
    /// ```text
    ///   LDR   Xctx,  [FP, #context]
    ///   SUB   Xargs, FP, #|offset of the deepest argument's depth slot|
    ///   <call_at_safepoint(jit_invoke_dispatch,
    ///                      [ctx, &JitInvokeInfo, args, n]) -> Xv,
    ///    consuming the n argument entries>
    ///   <bail if Xv is the sentinel; for J/D/F ask dispatch_threw first>
    ///   <push Xv as the descriptor's return type, marked if a reference>
    /// ```
    ///
    /// # Why this needs no call-target resolution
    ///
    /// `emit_invoke` set `self.failed` unconditionally from this backend's
    /// first commit, and the reason given -- and carried by three successive
    /// known-issue pages -- was that there is no way here to turn a
    /// constant-pool index into an entry point. That is still true, and it is
    /// no longer the question. `jit_invoke_dispatch` is the interpreter's own
    /// dispatcher: it takes the site's `JitInvokeInfo` and RESOLVES the target
    /// itself, at run time, through the same constant-pool machinery the
    /// interpreter uses -- virtual and interface selection on the receiver,
    /// JVMS §6.5's `invokespecial` redirect, a compiled callee's entry when
    /// one exists and the interpreter when it does not. So the four pieces the
    /// page split this into (a direct call, an inline cache, an outgoing Java
    /// argument ABI, a self-recursion guard) are all on the far side of ONE
    /// helper call, and this backend has had one of those since wave 18.
    ///
    /// What is given up is the speed of a direct call and of an inline cache.
    /// That is the right trade for a backend whose current alternative is not
    /// compiling the method at all.
    ///
    /// # The argument buffer is the operand area
    ///
    /// The helper reads `args_ptr as *const i64` with `args[0]` at the LOWEST
    /// address, one word per operand-stack VALUE (this VM's compact
    /// convention: a `long` is one). This backend's operand area is a run of
    /// frame words indexed by depth, `Arm64SpillArea::operand_word(depth) =
    /// locals + depth`, and `spill_word_offset` scales word indices UPWARD in
    /// address from `spill_offset`. So depth `base` -- the deepest argument,
    /// which is `args[0]` -- already sits at the lowest address of a
    /// contiguous ascending run of exactly the right words. The buffer needs
    /// no copy and no separate region: it is `&[FP + offset_of(base)]`, and
    /// the spill the call performs anyway is what fills it.
    ///
    /// That rests on an invariant worth naming, because it is what would break
    /// first: **an operand in a SLOT is always in ITS OWN depth's slot.** The
    /// only two places that ever set `OperandLoc::Slot` --
    /// [`Self::spill_entry`] and [`Self::restore_stack_at`] -- both address it
    /// through `spill_offset_for_depth(depth)`, and every stack shuffle here
    /// goes through `pop_entry`/`push_like`, which materialize into a REGISTER
    /// rather than renaming a slot. So an argument that was spilled long
    /// before this bytecode is already in the right word, and the call's own
    /// spill writes the rest to theirs.
    ///
    /// That is also why the oop story here is BETTER than x64's, which is
    /// worth recording because x64's is a known cost. There, an outgoing
    /// reference argument is marshalled into a staging area no frame-slot map
    /// can name, so every direct call taking a reference raises
    /// `pending_staged_args_unmapped` and the method loses `fully_oop_covered`
    /// for good (`moving_young_osr_method_needs_fallback` measured 439 of 449
    /// coverage failures on `TestKillProcessWhileWriting` as that one shape).
    /// Here the arguments never leave the operand stack until after the map is
    /// recorded, so each reference among them is named where it lies.
    ///
    /// # Refusals
    ///
    /// * An unresolved site -- no `JitInvokeInfo` for this pc.
    /// * An unwired `jit_invoke_dispatch`, or an unwired `dispatch_threw` at a
    ///   `J`/`D`/`F`-returning site.
    /// * `set_throw_bci` neither wired nor unneeded -- see
    ///   [`Self::can_route_exception`]. `synchronized` is still kept out, just
    ///   no longer by this check: its generated `any -> monitorexit; athrow`
    ///   handler makes the exception table non-empty, same as any other
    ///   `try`/`catch`, but `monitorenter`/`monitorexit` have no arm at all,
    ///   so that method refuses there instead, same as before this existed.
    /// * Fewer operand entries than the site consumes, which is a malformed
    ///   method rather than a shape.
    pub fn emit_invoke(&mut self, opcode: u8) -> bool {
        // `invokedynamic` is NOT one of the five in any useful sense.
        // `JitInvokeInfo::invoke_kind` encodes virtual/special/interface/
        // static and has no value for a call site that is not a class at all,
        // so `jit_invoke_dispatch` could not resolve one; and a method holding
        // an `invokedynamic` cannot stay compiled on the x64 tier either.
        // Refused HERE as well as by the caller declining to resolve the site,
        // because "the site table happened to be empty" is a different fact
        // from "this opcode has no lowering".
        if opcode == 0xba {
            return false;
        }
        if !self.can_route_exception() || self.helpers.invoke_dispatch == 0 {
            return false;
        }
        let Some(site) = self.invoke_sites.get(&self.cur_bytecode_pc).copied() else {
            return false;
        };
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };
        let Some(surviving) = self.operand_stack.len().checked_sub(site.num_jit_args) else {
            return false;
        };
        // `Long.MIN_VALUE` and a `double` of `-0.0` are bit-identical to the
        // deopt sentinel, and so is a `float` the helper sign-extends, so
        // those three returns have to ASK whether a throw is pending instead
        // of reading the bits -- exactly as `getfield`'s `J`/`D` arms do, and
        // as x64's post-invoke check does for the same three tags.
        let ambiguous = matches!(site.return_type, b'J' | b'D' | b'F');
        if ambiguous && self.helpers.dispatch_threw == 0 {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`: the
        // sentinel test puts a call on one side of a join, and a register
        // allocated there would exist on one path only.
        let ctx = self.alloc_reg(false);
        let args_ptr = self.alloc_reg(false);
        let value = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        let probe = ambiguous.then(|| self.alloc_reg(false));
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        // The buffer address. A zero-argument site still passes a pointer the
        // helper will not read -- it checks `num_args > 0` first -- and FP is
        // a real mapped address, which a literal 0 is not.
        if site.num_jit_args == 0 {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: args_ptr,
                rm: Arm64Register::FP,
            });
        } else {
            let Some(off) = self.spill_offset_for_depth(surviving) else {
                self.failed = true;
                return false;
            };
            // Frame offsets are negative from FP; the buffer grows upward in
            // address from this one word.
            let Some(magnitude) = off.checked_neg() else {
                self.failed = true;
                return false;
            };
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: args_ptr,
                rn: Arm64Register::FP,
                imm: magnitude,
            });
        }

        // Cast: an address the caller keeps alive with the artifact, and a
        // count that came from a descriptor.
        let info = site.info_ptr as i64;
        let Ok(num_args) = i64::try_from(site.num_jit_args) else {
            self.failed = true;
            return false;
        };
        // The first use of x64's ABI assertion on this backend. Worth having
        // exactly here: `emit_helper_call*` marshals `args.len()` registers,
        // so an argument count is DATA at each site rather than a shape the
        // emitter enforces, and a helper that grew a parameter would be a
        // silent ABI error instead of a build failure.
        cratonvm_jit_api::assert_helper_call_shape!(
            "invoke_dispatch",
            int_args = 4,
            returns_value = true
        );
        if !self.emit_helper_call_at_safepoint_consuming(
            self.helpers.invoke_dispatch,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(info),
                Arm64HelperArg::Reg(args_ptr),
                Arm64HelperArg::Imm(num_args),
            ],
            Some(value),
            safepoint_id,
            site.num_jit_args,
        ) {
            return false;
        }

        // The sentinel edge, shaped exactly like `emit_getfield`'s.
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: value,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        if let Some(probe) = probe {
            let leave = self.buffer.new_label();
            if !self.emit_helper_call(self.helpers.dispatch_threw, &[], Some(probe)) {
                return false;
            }
            self.buffer.emit(Arm64Instruction::Cbnz {
                rt: probe,
                label: leave,
            });
            // No signal: the `i64::MIN` is the callee's real result.
            // Re-materialized because the probe call destroyed `value`, and on
            // this branch it can only have held the sentinel.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: value,
                imm: i64::MIN,
            });
            self.buffer.emit(Arm64Instruction::B { label: join });
            self.buffer.bind_label(leave);
        }
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        // The result, decoded from X0 the way the VM decodes every compiled
        // return: a `float` is the low 32 bits of the word, a `double` all 64.
        match site.return_type {
            b'V' => {}
            b'I' | b'Z' | b'B' | b'C' | b'S' => {
                // The helper hands back a widened `int`; the canonical form on
                // this backend is sign-extended, and stating it here is what
                // keeps `iadd`'s 32-bit wrap correct on the result.
                self.emit_sxtw(value);
                self.push_reg(OperandKind::I32, value);
            }
            b'J' => self.push_reg(OperandKind::I64, value),
            b'F' => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: value });
                self.push_reg(OperandKind::F32, v);
            }
            b'D' => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: value });
                self.push_reg(OperandKind::F64, v);
            }
            b'L' | b'[' => {
                self.push_reg(OperandKind::Ref, value);
                self.mark_top_operand_as_oop();
            }
            // A return tag no descriptor produces: refuse rather than guess a
            // width for it.
            _ => {
                self.failed = true;
                return false;
            }
        }
        !self.failed
    }

    /// `checkcast` (`0xc0`) and `instanceof` (`0xc1`), round 9 wave 23.
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   <call_at_safepoint(jit_checkcast | jit_instanceof_check,
    ///                      [ctx, obj, name_ptr, name_len]) -> Xv,
    ///    consuming the object entry>
    ///   <bail if Xv is the sentinel: a ClassCastException is pending>
    ///   <push Xv: the reference for 0xc0, an int for 0xc1>
    /// ```
    ///
    /// The object is NOT popped before the call, for the reason `aastore`
    /// established: an entry the model still holds is spilled to its depth
    /// slot and named in the map, so a collector that moves it during the
    /// helper -- which is a real possibility here, because resolving the
    /// target class on first use allocates its `java/lang/Class` mirror --
    /// rewrites the word the helper reads. `checkcast`'s answer is then the
    /// object's NEW address, because the helper returns the reference it
    /// resolved rather than the bits it was handed.
    ///
    /// x64 has inline fast paths in front of this call (a trusted-oop check, a
    /// `KIND_TAGS == 0` screen for arrays, a class-id compare). None is
    /// emitted here: they need the receiver's class id from the header, which
    /// this backend has no reason to read yet, and the point of this wave is
    /// that the opcodes COMPILE.
    ///
    /// Refuses without a resolved site, an unwired helper, a context word, or
    /// `set_throw_bci` neither wired nor unneeded (see
    /// [`Self::can_route_exception`]) -- a failed cast returns the sentinel
    /// with a `ClassCastException` stashed, and [`Self::emit_bail_on_sentinel`]
    /// stamps the throw bci on that edge so the drain routes it through this
    /// method's own exception table.
    fn emit_typecheck(&mut self, opcode: u8) -> bool {
        let helper = if opcode == 0xc0 {
            self.helpers.checkcast
        } else {
            self.helpers.instanceof_check
        };
        if helper == 0 || !self.can_route_exception() {
            return false;
        }
        let Some(site) = self.typecheck_sites.get(&self.cur_bytecode_pc).copied() else {
            return false;
        };
        // An unresolved target makes the helper answer "not an instance" for
        // every object, which is a WRONG answer rather than a missing one.
        let Ok(name_len) = i64::try_from(site.name_len) else {
            return false;
        };
        if site.name_ptr == 0 || name_len == 0 {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        // Left on the stack, and held. See the doc.
        let Some((obj, entry)) = self.materialize_entry(0) else {
            return false;
        };
        if entry.kind != OperandKind::Ref {
            self.failed = true;
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let value = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        // Cast: an interned address that outlives every artifact.
        let name_ptr = site.name_ptr as i64;
        // Both helpers have the same shape, and the `class_name_ptr` counts as
        // one integer argument -- it is loaded as a full 64-bit immediate,
        // exactly as x64's two sites load it.
        cratonvm_jit_api::assert_helper_call_shape!(
            "checkcast",
            int_args = 4,
            returns_value = true
        );
        cratonvm_jit_api::assert_helper_call_shape!(
            "instanceof_check",
            int_args = 4,
            returns_value = true
        );
        if !self.emit_helper_call_at_safepoint_consuming(
            helper,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(name_ptr),
                Arm64HelperArg::Imm(name_len),
            ],
            Some(value),
            safepoint_id,
            1,
        ) {
            return false;
        }
        // `i64::MIN` is neither a reference (a heap pointer is under 2^48) nor
        // a `0`/`1`, so it is unambiguous for both opcodes.
        self.emit_bail_on_sentinel(value, sentinel);
        if opcode == 0xc0 {
            self.push_reg(OperandKind::Ref, value);
            self.mark_top_operand_as_oop();
        } else {
            self.emit_sxtw(value);
            self.push_reg(OperandKind::I32, value);
        }
        !self.failed
    }

    // -- Return -------------------------------------------------------------

    /// `ireturn`/`lreturn`/`areturn`/`freturn`/`dreturn`.
    ///
    /// The VM calls compiled code as `extern "C" fn(i64, ..) -> i64` and reads
    /// EVERY result out of X0, decoding a `float` as `f32::from_bits(x0 as
    /// u32)` and a `double` as `f64::from_bits(x0)`. So an FP result is moved
    /// into X0 with a bit-exact `FMOV` (`FMOV W0, Sn` for a `float`), not left
    /// in V0. `freturn`/`dreturn` used to pop and DISCARD the value and emit a
    /// bare `RET`, skipping the epilogue: the caller's callee-saved registers
    /// were never restored, SP and FP were left pointing into this frame, and
    /// the "result" was whatever X0 held.
    fn emit_return_value(&mut self, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let inst = match kind {
            OperandKind::F32 => Arm64Instruction::FmovFromFpSingle {
                rd: Arm64Register::X0,
                vn: src,
            },
            OperandKind::F64 => Arm64Instruction::FmovFromFp {
                rd: Arm64Register::X0,
                vn: src,
            },
            _ => Arm64Instruction::Mov {
                rd: Arm64Register::X0,
                rm: src,
            },
        };
        self.buffer.emit(inst);
        if kind == OperandKind::I32 {
            self.emit_narrow_int_return();
        }
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.reachable = false;
    }

    /// JVMS §6.5 `ireturn`: narrow X0 to the declared `boolean`/`byte`/
    /// `char`/`short` return type, keeping this backend's `int` convention
    /// (sign-extended through all 64 bits): `Z` is `& 1` and `C` is `& 0xFFFF`
    /// (never negative, so zero-extension IS the sign extension), `B`/`S` are
    /// the X-form `SXTB`/`SXTH`. The x64 twin is `emit_narrow_int_return` in
    /// `x64/emit.rs`; before this the two backends disagreed on what a
    /// `boolean` method returning `2` hands back.
    fn emit_narrow_int_return(&mut self) {
        let x0 = Arm64Register::X0;
        let inst = match self.return_narrow {
            Some(b'Z') => Arm64Instruction::AndImm {
                rd: x0,
                rn: x0,
                imm: 1,
            },
            Some(b'B') => Arm64Instruction::Sxtb { rd: x0, rn: x0 },
            Some(b'C') => Arm64Instruction::AndImm {
                rd: x0,
                rn: x0,
                imm: 0xFFFF,
            },
            Some(b'S') => Arm64Instruction::Sxth { rd: x0, rn: x0 },
            _ => return,
        };
        self.buffer.emit(inst);
    }
}

// NOTE: A `detect_neon_patterns` / `NeonVectorizablePattern` bytecode scanner
// previously lived here. It was dead code — only ever called from its own unit
// tests, never wired into any aarch64 codegen path — and, worse, it emitted
// patterns with hardcoded placeholder local indices (`array_local: 0` etc.) with
// the operand analysis left unfinished, so it would have vectorized against the
// wrong locals (a miscompile) if ever consumed. aarch64 is not the production
// backend (x64.rs is). Removed in the 2026-06-10 JIT cleanup pass rather than
// gated, since nothing non-test referenced it. If NEON auto-vectorization is
// pursued, reuse the real operand resolution from the x64 BCE analysis
// (`analyze_array_access_operands` / `find_induction_variable`) instead of
// placeholders, and wire the result into codegen before adding it back.

// ---------------------------------------------------------------------------
// Machine code emission
// ---------------------------------------------------------------------------

// `to_reg` used to live here: `Arm64Register -> Option<Reg>` over 0..=31,
// whose one production caller was `r`. After A8 `r` builds a `RegSp` directly
// (31 is SP there, and `Reg` no longer has a 31 to give), so `to_reg` had no
// production caller left and was removed rather than kept as a second spelling
// of `Reg::from_u8`. Its C11 contract -- refuse by returning `None`, never
// assert, so a debug build declines a method exactly where a release build
// does -- is carried by the three conversions below and by `Reg::from_u8`.

/// Convert an `Arm64Register` to an `FpReg` for the emitter.
///
/// The FP half of the `Arm64Register` space is `32 + n` for `Dn`, and `FpReg`
/// models the full architectural file `D0..D31`, so the accepted range is
/// `32..=63`. This used to say "V0=32..V7=39", which is the register
/// ALLOCATOR's pool (`ARM64_LOCAL_FPS`) rather than this conversion's range —
/// a test was written against that sentence and asserted that `D8` was
/// unencodable.
///
/// Returns `None` if `r` is not a valid FP register encoding. See [`to_reg`]
/// for why this returns `Option` and why it does not assert.
fn to_fpreg(r: Arm64Register) -> Option<crate::aarch64::FpReg> {
    if r.0 < 32 {
        return None;
    }
    crate::aarch64::FpReg::from_u8(r.0 - 32)
}

thread_local! {
    /// Sticky "an `Arm64Register` was not a valid encoding" flag for the
    /// current [`emit_machine_code_inner`] call.
    ///
    /// A5. `r` and `fp` used to `.expect()`, i.e. **panic**, on a register the
    /// allocator should never have produced. Every other refusal in this file
    /// sets `self.failed` or returns `None` from `emit_machine_code`, and the
    /// crate's rule is that a compiler bug costs the method its compiled body,
    /// not the process. This is not hypothetical for the FP side: `D8`-`D15`
    /// once arrived as `Arm64Register(40..=47)` and tripped the `debug_assert`
    /// that the `expect` replaced (aarch64 parity audit §4.3).
    ///
    /// Modelled on `Aarch64Emitter::overflowed()` -- the same "note it, keep
    /// encoding, discard the buffer at the end" shape, which is what lets the
    /// two `r`/`fp` helpers keep their infallible signature at several hundred
    /// call sites. Thread-local rather than global because one method is
    /// encoded on one thread, so two concurrent compiles cannot poison each
    /// other's result.
    static INVALID_REGISTER_SEEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record that a register could not be encoded.
fn note_invalid_register() {
    INVALID_REGISTER_SEEN.with(|seen| seen.set(true));
}

/// Read and clear the sticky invalid-register flag.
fn take_invalid_register() -> bool {
    INVALID_REGISTER_SEEN.with(|seen| seen.replace(false))
}

/// Shorthand for `to_reg` at call sites where the register allocator
/// guarantees the register is a valid GPR. Centralising it keeps the regalloc
/// contract documented in one place.
///
/// A violated invariant is recorded in [`INVALID_REGISTER_SEEN`] and
/// `emit_machine_code_inner` then discards the whole body, so the register
/// returned here is a placeholder whose bytes are never published and its
/// identity does not matter. IP0 is used because it is the one register no
/// lowering treats as carrying a live value across an instruction boundary.
///
/// # A8: this is the SP-capable conversion, and says so in its type
///
/// It answers [`crate::aarch64::RegSp`], reading encoding 31 as the stack
/// pointer, because the fields it feeds -- a load/store base, an add/sub
/// immediate `rd`/`rn`, an extended-register `rd`/`rn` -- are the ones that read
/// 31 as SP: `SUB SP, SP, #imm` and `STP FP, LR, [SP, #-16]!` are real
/// instructions the prologue emits through here.
///
/// Before A8 it answered a plain `Reg`, and `Reg` contained `SP`. Removing `SP`
/// from `Reg` without changing this function would have COMPILED -- `to_reg`
/// would simply have started answering `None` for 31 -- and silently turned
/// every prologue into a refused compile that also encoded X16 where it meant
/// SP. That is the "half-finished split is silently wrong" failure A8's own
/// note warned of, caught by reading rather than by the compiler. The return
/// type is what now makes it a compiler error: a `RegSp` cannot reach a field
/// that reads 31 as XZR, so every GP-only operand has to go through [`r_gp`].
#[inline]
fn r(reg: Arm64Register) -> crate::aarch64::RegSp {
    match crate::aarch64::RegSp::from_u8(reg.0) {
        Some(reg) => reg,
        None => {
            note_invalid_register();
            crate::aarch64::RegSp::X(crate::aarch64::Reg::X16)
        }
    }
}

/// [`r`] for an operand field of a **shifted-register** instruction, where
/// encoding 31 is XZR and not SP.
///
/// # A8: the other place provenance is visible
///
/// Before A8, `XZR` and `Reg::SP` were one value, so no check on an operand
/// could say which one a caller meant -- see `aarch64::XZR`. Since A8 the TYPE
/// says it (this yields a GP-only `Reg`, [`r`] a `RegSp`, `r_zr` a `RegZr`), and
/// what follows is why this conversion also REFUSES 31. The arm of
/// `emit_machine_code_inner`'s match that is about to emit an `ADD Xd, Xn, Xm`
/// knows something the operand does not: it knows the ENCODING. In a
/// shifted-register form every register field reads 31 as XZR, so an
/// `Arm64Register` carrying 31 into one of those fields is never what the
/// caller meant — a real zero operand is written as the literal
/// `crate::aarch64::XZR` at the call site (the `Neg` arm does exactly that),
/// and a literal is not the thing that gets forwarded by accident.
///
/// So: this refuses 31 and [`r`] does not. `r` stays for the addsub-immediate,
/// extended-register and load/store arms, where 31 legitimately means SP — the
/// prologue's `SUB SP, SP, #imm` and `STP FP, LR, [SP, #-16]!` are that shape.
///
/// A refusal records through [`note_invalid_register`] and costs the method its
/// compiled body, which is this file's standard answer to a compiler bug (A5)
/// and is the safe direction: the alternative was `ADD X16, SP, X16` computing
/// `0 + X16` and sending an access to an absolute address.
///
/// **If this ever refuses something legitimate**, the fix is at the call site,
/// not here: write `crate::aarch64::XZR` instead of routing a 31 through an
/// `Arm64Register`.
#[inline]
fn r_gp(reg: Arm64Register) -> crate::aarch64::Reg {
    match crate::aarch64::Reg::from_u8_gp(reg.0) {
        Some(reg) => reg,
        None => {
            note_invalid_register();
            crate::aarch64::Reg::X16
        }
    }
}

/// [`r`] for a field that reads encoding 31 as the ZERO register and may
/// legitimately be given it.
///
/// # A8: the third conversion, and why it had to exist
///
/// After the split this file has three ways to turn an `Arm64Register` into an
/// operand, chosen by what the FIELD does with 31 -- never by the operand:
///
/// | conversion | yields | 31 becomes | for |
/// |---|---|---|---|
/// | [`r`] | `RegSp` | SP | load/store bases; add/sub-immediate and extended `rd`/`rn` |
/// | `r_zr` | `RegZr` | XZR | data fields that may be zero: a load/store `Rt`, `CBZ`'s register, the GP side of `scvtf`/`fcvtzs`/`fmov`, a branch target |
/// | [`r_gp`] | `Reg` | refused | fields where 31 is always a bug |
///
/// This one was NOT in the first draft of the split, and the omission was a
/// real regression caught by `a_large_frame_bangs_every_page_before_moving_sp`.
/// The first draft sent every non-SP field to `r_gp`, reasoning that a GP field
/// given 31 was a bug. But the stack-bang probe stores ZERO with
/// `STR XZR, [X16]` -- its `Rt` is 31 on purpose -- and `r_gp` refused it, so
/// every large-frame prologue stopped encoding. More generally, before A8 every
/// one of these fields accepted 31 and encoded it as 31, because `r` answered
/// `Reg::SP`; `r_zr` keeps exactly those bits, where `r_gp` would have turned
/// each into a refusal.
#[inline]
fn r_zr(reg: Arm64Register) -> crate::aarch64::RegZr {
    if reg.0 == 31 {
        return crate::aarch64::RegZr::Zr;
    }
    match crate::aarch64::Reg::from_u8(reg.0) {
        Some(reg) => crate::aarch64::RegZr::X(reg),
        None => {
            note_invalid_register();
            crate::aarch64::RegZr::X(crate::aarch64::Reg::X16)
        }
    }
}

/// Shorthand for `to_fpreg` at call sites where the register allocator
/// guarantees the register is a valid FP register. See [`r`].
#[inline]
fn fp(reg: Arm64Register) -> crate::aarch64::FpReg {
    match to_fpreg(reg) {
        Some(reg) => reg,
        None => {
            note_invalid_register();
            crate::aarch64::FpReg::D0
        }
    }
}

/// Convert an `Arm64Condition` to an `aarch64::Cond`.
fn to_cond(c: &Arm64Condition) -> crate::aarch64::Cond {
    match c {
        Arm64Condition::Eq => crate::aarch64::Cond::EQ,
        Arm64Condition::Ne => crate::aarch64::Cond::NE,
        Arm64Condition::Lt => crate::aarch64::Cond::LT,
        Arm64Condition::Le => crate::aarch64::Cond::LE,
        Arm64Condition::Gt => crate::aarch64::Cond::GT,
        Arm64Condition::Ge => crate::aarch64::Cond::GE,
        Arm64Condition::Hi => crate::aarch64::Cond::HI,
        Arm64Condition::Ls => crate::aarch64::Cond::LS,
        Arm64Condition::Cs => crate::aarch64::Cond::HS,
        Arm64Condition::Cc => crate::aarch64::Cond::LO,
        Arm64Condition::Mi => crate::aarch64::Cond::MI,
        Arm64Condition::Pl => crate::aarch64::Cond::PL,
        Arm64Condition::Vs => crate::aarch64::Cond::VS,
        Arm64Condition::Vc => crate::aarch64::Cond::VC,
        Arm64Condition::Al => crate::aarch64::Cond::AL,
    }
}

/// Materialize the effective address `base + offset` into IP0 (X16).
///
/// Bug-fix (ARM64 BUG #1): used by the `Ldr`/`Str` lowering when `offset`
/// falls outside the 9-bit signed range that LDUR/STUR can encode. We load the
/// (possibly large, possibly negative) byte offset into IP0 with `mov_imm64`
/// (sign-extended via the MOVN path for negatives) and add the base, so the
/// subsequent zero-offset access never touches the base register's value. IP0
/// (X16) is the AAPCS64 intra-procedure-call scratch register and is never a
/// regalloc output, so clobbering it here is safe.
///
/// Returns `false`, emitting nothing useful, when `base` is ITSELF IP0. The
/// sequence writes the offset into X16 *before* the ADD reads the base, so an
/// X16 base is destroyed before it is used and the resulting address is
/// `offset + offset`. The claim that "IP0 is never a regalloc output" is true
/// of the allocator and not of this backend's own X16 uses -- the stack-bang
/// probe, the `tableswitch` scratch and the poll/frame-record address
/// materialisation all put things in X16 -- so the caller must abandon the
/// method rather than assume the shape cannot arise. `emit_addsub_imm_safe`
/// and the `AndImm` lowering already guard the same hazard; this closes the
/// load/store arms.
#[must_use]
fn emit_addr_into_ip0(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    base: impl Into<crate::aarch64::RegSp>,
    offset: i32,
) -> bool {
    let base: crate::aarch64::RegSp = base.into();
    if base.enc() == 16 {
        return false;
    }
    // IP0 = (i64)offset  (mov_imm64 picks MOVN for negative values, giving a
    // correct two's-complement 64-bit value).
    emitter.mov_imm64(crate::aarch64::Reg::X16, offset as i64 as u64);
    // IP0 = base + IP0, in the EXTENDED-register form. The shifted-register
    // ADD reads register 31 as XZR, so with an SP base it computed `0 + offset`
    // and the access went to an absolute address. The extended form reads 31
    // as SP and is otherwise the same instruction.
    emitter.add_ext(
        crate::aarch64::Reg::X16,
        base,
        crate::aarch64::Reg::X16,
        crate::aarch64::Extend::UXTX,
        0,
    );
    true
}

/// Lower an ADD/SUB-immediate (`rd = rn ± imm`) safely.
///
/// Bug-fix (AArch64 LOW, immediate truncation): the ADD/SUB-immediate
/// encoding (`addsub_imm`) only carries a 12-bit unsigned field and silently
/// masks anything wider (`imm12 as u32 & 0xFFF`). The previous lowering passed
/// `imm as u16` straight through, so any immediate > 0xFFF (e.g. a large
/// `iinc` constant or a frame offset folded into an `AddImm`) was silently
/// truncated to a WRONG value with no diagnostic.
///
/// This helper picks the correct, lossless encoding:
///   * magnitude fits 12 bits (≤ 0xFFF)                  → ADD/SUB #imm12
///   * magnitude fits the LSL-#12 shifted 12-bit form
///     (low 12 bits zero, high 12 bits ≤ 0xFFF)          → ADD/SUB #imm12, LSL #12
///   * otherwise                                         → materialize the
///     immediate into IP0 (X16) with `mov_imm64` and use the register form
///     ADD/SUB `rd, rn, X16`.
///
/// The sign is normalized first: a negative immediate flips ADD↔SUB so the
/// magnitude handed to the encoding is always non-negative. IP0 (X16) is the
/// AAPCS64 intra-procedure-call scratch and is never a regalloc output, so the
/// register-form fallback never clobbers a live value.
///
/// The register fallback uses the EXTENDED-register form, so it is sound with
/// SP as `rd` or `rn` too. It used to be the shifted-register form, where 31
/// is XZR, so an SP adjustment wider than 12 bits had no lowering at all -- it
/// first emitted `BRK #0` while reporting success, later refused the method,
/// and either way a frame of 4096 bytes or more could not be allocated.
///
/// Returns `false` only when the fallback would clobber its own operand (`rn`
/// is IP0), in which case the caller must abandon the method.
#[must_use]
fn emit_addsub_imm_safe(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: impl Into<crate::aarch64::RegSp>,
    rn: impl Into<crate::aarch64::RegSp>,
    imm: i32,
    is_sub: bool,
) -> bool {
    let (rd, rn): (crate::aarch64::RegSp, crate::aarch64::RegSp) = (rd.into(), rn.into());
    // Normalize: fold the sign into the operation so `mag` is non-negative.
    // (i32::MIN's magnitude does not fit i32, so widen to i64 first.)
    let signed = if is_sub { -(imm as i64) } else { imm as i64 };
    let effective_sub = signed < 0;
    let mag = signed.unsigned_abs();

    if mag <= 0xFFF {
        // Fits the plain 12-bit immediate form.
        if effective_sub {
            emitter.sub_imm(rd, rn, mag as u16, false);
        } else {
            emitter.add_imm(rd, rn, mag as u16, false);
        }
        true
    } else if mag & 0xFFF == 0 && (mag >> 12) <= 0xFFF {
        // Fits the LSL #12 shifted 12-bit immediate form.
        let hi = (mag >> 12) as u16;
        if effective_sub {
            emitter.sub_imm(rd, rn, hi, true);
        } else {
            emitter.add_imm(rd, rn, hi, true);
        }
        true
    } else if rn.enc() == 16 {
        // The fallback materializes into IP0, which would destroy an operand
        // that is itself IP0 before it is read. No lowering emits that shape.
        false
    } else {
        // Out of immediate range: materialize into IP0 (X16) and use the
        // extended-register form, which reads 31 as SP on both sides. Never
        // silently truncate.
        emitter.mov_imm64(crate::aarch64::Reg::X16, mag);
        if effective_sub {
            emitter.sub_ext(
                rd,
                rn,
                crate::aarch64::Reg::X16,
                crate::aarch64::Extend::UXTX,
                0,
            );
        } else {
            emitter.add_ext(
                rd,
                rn,
                crate::aarch64::Reg::X16,
                crate::aarch64::Extend::UXTX,
                0,
            );
        }
        true
    }
}

/// Lower `rd = rn +/- imm` on W registers (`AddImmW`/`SubImmW`).
///
/// A 32-bit result depends only on the addend mod 2^32, so a wide immediate is
/// materialized as its low 32 bits. `false` when that would clobber `rn`.
#[must_use]
fn emit_addsub_imm_w(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: crate::aarch64::Reg,
    rn: crate::aarch64::Reg,
    imm: i32,
    is_sub: bool,
) -> bool {
    let signed = if is_sub {
        -i64::from(imm)
    } else {
        i64::from(imm)
    };
    let mag = signed.unsigned_abs();
    if mag <= 0xFFF {
        // Cast: bounds-checked immediately above.
        if signed < 0 {
            emitter.sub_imm_w(rd, rn, mag as u16, false);
        } else {
            emitter.add_imm_w(rd, rn, mag as u16, false);
        }
        true
    } else if rn.enc() == 16 {
        false
    } else {
        // Cast: the low 32 bits of the two's-complement addend.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(signed as u32));
        emitter.add_w(rd, rn, crate::aarch64::Reg::X16);
        true
    }
}

/// `rd = rn - value` on W registers, through IP0 when `value` is wide.
fn emit_w_sub_const(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: crate::aarch64::Reg,
    rn: crate::aarch64::Reg,
    value: i32,
) {
    let v = i64::from(value);
    if (0..=0xFFF).contains(&v) {
        // Cast: range-checked.
        emitter.sub_imm_w(rd, rn, v as u16, false);
    } else if (-0xFFF..0).contains(&v) {
        // Cast: range-checked.
        emitter.add_imm_w(rd, rn, (-v) as u16, false);
    } else {
        // Cast: the constant's 32-bit pattern.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(value as u32));
        emitter.sub_w(rd, rn, crate::aarch64::Reg::X16);
    }
}

/// Set the flags for `Wrn - value`: `CMP #imm`, `CMN #-imm`, or a compare
/// against IP0. Never a scratch register, so it cannot collide with `rn`.
fn emit_w_cmp_const(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rn: crate::aarch64::Reg,
    value: i32,
) {
    let v = i64::from(value);
    if (0..=0xFFF).contains(&v) {
        // Cast: range-checked.
        emitter.cmp_imm_w(rn, v as u16);
    } else if (-0xFFF..0).contains(&v) {
        // Cast: range-checked.
        emitter.cmn_imm_w(rn, (-v) as u16);
    } else {
        // Cast: the constant's 32-bit pattern.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(value as u32));
        emitter.cmp_w(rn, crate::aarch64::Reg::X16);
    }
}

/// Emit machine code bytes from the ARM64 pseudo-instruction sequence.
/// Uses `Aarch64Emitter` from `aarch64.rs` to encode each instruction.
///
/// Returns `None` — bail this method to the interpreter — when the pseudo-op
/// sequence cannot be encoded soundly. Six independent reasons (0–5 below);
/// reasons 4 and 5 were added by the 2026-09 encoder audit, which found the
/// scaled load/store encoders masking an over-wide offset into a valid-looking
/// one and the two register shorthands panicking instead of refusing:
///
/// 0. an `AddImm`/`SubImm` with no sound encoding — see
///    [`emit_addsub_imm_safe`], which returns `false` for the SP case that
///    used to emit `BRK #0` while still reporting a successful compile;
/// 1. `result.success == false` (an opcode arm refused the method);
/// 2. a branch or literal reference whose label is never bound (see the patch
///    loops at the end — an unbound label used to be left as "branch to
///    self", i.e. an infinite loop in *successfully* compiled code);
/// 3. `Aarch64Emitter::overflowed()` — a branch displacement that did not fit
///    its encoding field. `aarch64.rs` sets that sticky flag precisely so a
///    release build (where the parallel `debug_assert!` is compiled out) can
///    discard the buffer instead of executing a truncated branch. Until this
///    check was added the flag had **no production reader** anywhere in the
///    crate, so on a release `aarch64` build an out-of-range branch was
///    silently truncated and emitted as executable code.
/// 4. an immediate no load/store encoding can carry exactly — a scaled
///    unsigned offset that is misaligned or over 12 bits, an `i16`-overflowing
///    pair offset, or a far `[base + offset]` whose base or stored value is
///    IP0, the very register the address materialisation destroys;
/// 5. a register that is not a valid encoding, noted by [`r`]/[`fp`] in
///    [`INVALID_REGISTER_SEEN`] and checked once at the end. Those two used to
///    `.expect()`, so a regalloc bug killed the process instead of costing one
///    method its compiled body.
/// Encode a compiled method, and report where every pseudo-op landed.
///
/// The second element is `pseudo_offsets`: `pseudo_offsets[i]` is the byte
/// offset at which `result.instructions[i]` was encoded, and the vector has one
/// extra trailing entry equal to the total code length, so a safepoint whose
/// following pseudo-op is one past the end still resolves. This is the mapping
/// an aarch64 oop map has to be keyed through -- see [`Arm64PendingOopMap`] for
/// why `instruction_count * 4` is not it.
fn emit_machine_code_inner(result: &Arm64CompileResult) -> Option<(Vec<u8>, Vec<usize>)> {
    use crate::aarch64::Aarch64Emitter;

    // Start from a clean slate: an earlier call may have returned through one
    // of the `return None` paths below with the flag still set.
    let _ = take_invalid_register();

    if !result.success {
        return None;
    }

    let mut emitter = Aarch64Emitter::new();
    let mut label_offsets: HashMap<u32, usize> = HashMap::new();
    // (code_offset, label_id, is_cond) — is_cond distinguishes B from B.cond/CBZ/CBNZ
    let mut branch_patches: Vec<(usize, u32, bool)> = Vec::new();
    // (code_offset, label_id) for LDR literal instructions needing pool offset patching
    let mut literal_patches: Vec<(usize, u32)> = Vec::new();
    // (entry_offset, table_base, label_id) for jump-table words
    let mut table_patches: Vec<(usize, usize, u32)> = Vec::new();

    // One entry per pseudo-op, recorded BEFORE it is encoded, so
    // `pseudo_offsets[i]` is where instruction `i` begins.
    let mut pseudo_offsets: Vec<usize> = Vec::with_capacity(result.instructions.len() + 1);
    for inst in &result.instructions {
        pseudo_offsets.push(emitter.offset());
        match inst {
            Arm64Instruction::Label(id) => {
                label_offsets.insert(*id, emitter.offset());
            }
            Arm64Instruction::Comment(_) => { /* skip */ }
            Arm64Instruction::ConstantPoolEntry { label, value } => {
                // Bind the label to the current offset, then emit the raw 64-bit value.
                label_offsets.insert(*label, emitter.offset());
                emitter.emit_u64_data(*value);
            }

            // -- Arithmetic --
            Arm64Instruction::Add { rd, rn, rm } => {
                emitter.add(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::AddLsl { rd, rn, rm, shift } => {
                // The imm6 field holds 0..=63; the array widths need 0..=3 and
                // anything else is a lowering bug, not a wide immediate to
                // materialize, so refuse the method rather than truncate.
                if *shift > 3 {
                    return None;
                }
                emitter.add_lsl(r_gp(*rd), r_gp(*rn), r_gp(*rm), *shift);
            }
            Arm64Instruction::AddImm { rd, rn, imm } => {
                // Range-checked lowering: fits 12-bit / shifted-12-bit form, or
                // materializes into IP0 and uses the register form. Never
                // silently truncates a wide immediate (see emit_addsub_imm_safe).
                // `false` = no sound encoding exists → bail the method.
                if !emit_addsub_imm_safe(&mut emitter, r(*rd), r(*rn), *imm, false) {
                    return None;
                }
            }
            Arm64Instruction::Sub { rd, rn, rm } => {
                emitter.sub(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::SubImm { rd, rn, imm } => {
                if !emit_addsub_imm_safe(&mut emitter, r(*rd), r(*rn), *imm, true) {
                    return None;
                }
            }
            Arm64Instruction::Mul { rd, rn, rm } => {
                emitter.mul(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::SDiv { rd, rn, rm } => {
                emitter.sdiv(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Neg { rd, rn } => {
                // NEG Xd, Xn = SUB Xd, XZR, Xn. In the shifted form this emits,
                // register 31 is the zero register. Before A8 `XZR` and
                // `Reg::SP` were the same value and writing `SP` here compiled;
                // now `sub` takes a `RegZr` and `SP` is a `RegSp`, so it cannot.
                emitter.sub(r_gp(*rd), crate::aarch64::XZR, r_gp(*rn));
            }
            Arm64Instruction::Madd { rd, rn, rm, ra } => {
                emitter.madd(r_gp(*rd), r_gp(*rn), r_gp(*rm), r_gp(*ra));
            }
            Arm64Instruction::Msub { rd, rn, rm, ra } => {
                emitter.msub(r_gp(*rd), r_gp(*rn), r_gp(*rm), r_gp(*ra));
            }

            // -- Logical --
            Arm64Instruction::And { rd, rn, rm } => {
                emitter.and(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Orr { rd, rn, rm } => {
                emitter.orr(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Eor { rd, rn, rm } => {
                emitter.eor(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Lsl { rd, rn, rm } => {
                emitter.lsl(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Lsr { rd, rn, rm } => {
                emitter.lsr(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Asr { rd, rn, rm } => {
                emitter.asr(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }

            // -- Compare --
            Arm64Instruction::Cmp { rn, rm } => {
                emitter.cmp(r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::CmpImm { rn, imm } => {
                // Range-checked lowering. CMP #imm12 (= SUBS XZR, rn, #imm12)
                // only carries an unsigned 12-bit field. A wider or negative
                // immediate cannot be encoded directly, so we materialize it
                // into IP0 (X16) and use the register form `CMP rn, X16`
                // (= SUBS XZR, rn, X16). Never silently truncate (the old
                // `*imm as u16` masked anything > 0xFFFF and the encoder then
                // masked again to 12 bits). The shifted LSL #12 form is left to
                // the register-materialization path to avoid threading a new
                // shifted-immediate encoder through aarch64.rs; CMP immediates
                // in this backend are small (almost always 0) so this costs at
                // most one extra MOV in a cold path.
                let imm = *imm;
                if imm >= 0 && imm <= 0xFFF {
                    emitter.cmp_imm(r(*rn), imm as u16);
                } else {
                    // Negative or out-of-range: materialize and compare by reg.
                    emitter.mov_imm64(crate::aarch64::Reg::X16, imm as i64 as u64);
                    emitter.cmp(r_gp(*rn), crate::aarch64::Reg::X16);
                }
            }
            Arm64Instruction::Tst { rn, rm } => {
                emitter.tst(r_gp(*rn), r_gp(*rm));
            }

            // -- Move --
            Arm64Instruction::Mov { rd, rm } => {
                emitter.mov(r_gp(*rd), r_gp(*rm));
            }
            Arm64Instruction::MovImm { rd, imm } => {
                emitter.mov_imm64(r_gp(*rd), *imm as u64);
            }
            Arm64Instruction::MovK { rd, imm, shift } => {
                emitter.movk(r_gp(*rd), *imm, *shift);
            }

            // -- Load / Store --
            //
            // Bug-fix (ARM64 BUG #1, frame-slot corruption): plain
            // `[base + #offset]` access must NEVER use the pre-index
            // writeback form (`ldr_pre`/`str_pre`), which mutates the base
            // register `base = base + offset` as a side effect. Every frame
            // slot is addressed at a NEGATIVE offset from FP, so the old code
            // corrupted FP on every spill/reload. Routing:
            //   * offset >= 0 and 8-aligned and in scaled range → scaled
            //     unsigned-offset LDR/STR (`ldr_imm`/`str_imm`);
            //   * offset in the signed imm9 range (−256..=255) → unscaled
            //     non-writeback LDUR/STUR (`ldur`/`stur`);
            //   * otherwise → materialize the effective address into a scratch
            //     (IP0/X16) and use a zero-offset access.
            Arm64Instruction::Ldr { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 && *offset <= 32760 {
                    // Cast: range-checked on the line above, and `ldr_imm`
                    // re-checks and refuses rather than masking.
                    if !emitter.ldr_imm(r_zr(*rt), r(*rn), *offset as u16) {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    emitter.ldur(r_zr(*rt), r(*rn), *offset as i16);
                } else {
                    // Out of imm9 range: IP0 = base + offset, then LDR [IP0, #0].
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    emitter.ldur(r_zr(*rt), crate::aarch64::Reg::X16, 0);
                }
            }
            Arm64Instruction::Ldrb { rt, rn, offset } => {
                // Byte loads use an UNSCALED imm12 (units of 1), and only the
                // non-negative unsigned-offset form is encodable here. The one
                // caller is the safepoint poll, which uses offset 0.
                if *offset < 0 || *offset > 0xFFF {
                    return None;
                }
                // Cast: bounds-checked immediately above.
                if !emitter.ldrb_imm(r_zr(*rt), r(*rn), *offset as u16) {
                    return None;
                }
            }
            Arm64Instruction::Str { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 && *offset <= 32760 {
                    // Cast: range-checked on the line above; `str_imm` refuses
                    // anything it cannot encode exactly.
                    if !emitter.str_imm(r_zr(*rt), r(*rn), *offset as u16) {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    emitter.stur(r_zr(*rt), r(*rn), *offset as i16);
                } else {
                    // Out of imm9 range: IP0 = base + offset, then STR [IP0, #0].
                    //
                    // A4. `emit_addr_into_ip0` leaves the effective address in
                    // X16 and the store then reads `rt` -- so if `rt` IS X16,
                    // the value being stored was overwritten by its own
                    // address and this writes the address to memory. The
                    // `AndImm` arm further down already guards the sibling case
                    // (`rn.0 == 16`); this is the same hazard on the other
                    // operand, and the claim that "IP0 is never a regalloc
                    // output" is true of the ALLOCATOR but not of this
                    // backend's own X16 uses -- the stack-bang probe, the
                    // switch scratch and the poll/frame-record address
                    // materialisation all put things there.
                    if rt.0 == 16 {
                        return None;
                    }
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    emitter.stur(r_zr(*rt), crate::aarch64::Reg::X16, 0);
                }
            }
            Arm64Instruction::Ldp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                // The pseudo-op carries an `i32` and the encoder takes an
                // `i16`, so the conversion is a refusal point of its own: `as
                // i16` would wrap +32768 to -32768 before `ldp` ever sees it.
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.ldp(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            Arm64Instruction::Stp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.stp(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            // Bug-fix (ARM64 BUG #2): writeback prologue/epilogue pair ops.
            Arm64Instruction::StpPre {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.stp_pre(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            Arm64Instruction::LdpPost {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.ldp_post(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            Arm64Instruction::LdrLiteral { rt, label } => {
                // Emit a real LDR (literal) instruction. The offset to the
                // target label will be patched after all code is emitted.
                let pos = emitter.ldr_literal_x(r_zr(*rt));
                literal_patches.push((pos, *label));
            }

            // -- Branch --
            Arm64Instruction::B { label } => {
                let pos = emitter.b(0);
                branch_patches.push((pos, *label, false));
            }
            Arm64Instruction::BCond { cond, label } => {
                let pos = emitter.b_cond(to_cond(cond), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Bl { label } => {
                let pos = emitter.bl(0);
                branch_patches.push((pos, *label, false));
            }
            Arm64Instruction::Br { rn } => {
                emitter.br(r_zr(*rn));
            }
            Arm64Instruction::Blr { rn } => {
                emitter.blr(r_zr(*rn));
            }
            Arm64Instruction::Ret => {
                emitter.ret_lr();
            }
            Arm64Instruction::Cbz { rt, label } => {
                let pos = emitter.cbz(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Cbnz { rt, label } => {
                let pos = emitter.cbnz(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }

            // -- FP move --
            Arm64Instruction::FmovToFp { vd, rn } => {
                emitter.fmov_d_from_gp(fp(*vd), r_zr(*rn));
            }
            Arm64Instruction::FmovFromFp { rd, vn } => {
                emitter.fmov_gp_from_d(r_zr(*rd), fp(*vn));
            }
            Arm64Instruction::FmovFp { vd, vn } => {
                emitter.fmov_d(fp(*vd), fp(*vn));
            }

            // -- FP negate --
            Arm64Instruction::FnegDouble { vd, vn } => {
                emitter.fneg_d(fp(*vd), fp(*vn));
            }

            // -- FP single-precision --
            Arm64Instruction::FaddSingle { vd, vn, vm } => {
                emitter.fadd_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FsubSingle { vd, vn, vm } => {
                emitter.fsub_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FmulSingle { vd, vn, vm } => {
                emitter.fmul_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FdivSingle { vd, vn, vm } => {
                emitter.fdiv_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FcmpSingle { vn, vm } => {
                emitter.fcmp_s(fp(*vn), fp(*vm));
            }
            Arm64Instruction::FnegSingle { vd, vn } => {
                emitter.fneg_s(fp(*vd), fp(*vn));
            }

            // -- Conversion --
            Arm64Instruction::ScvtfDouble { vd, rn } => {
                emitter.scvtf_d_x(fp(*vd), r_zr(*rn));
            }
            Arm64Instruction::ScvtfSingle { vd, rn } => {
                emitter.scvtf_s_w(fp(*vd), r_zr(*rn));
            }
            Arm64Instruction::FcvtzsInt { rd, vn } => {
                emitter.fcvtzs_x_d(r_zr(*rd), fp(*vn));
            }
            Arm64Instruction::FcvtzsSingle { rd, vn } => {
                emitter.fcvtzs_w_s(r_zr(*rd), fp(*vn));
            }
            Arm64Instruction::FcvtSingleToDouble { vd, vn } => {
                emitter.fcvt_d_s(fp(*vd), fp(*vn));
            }
            Arm64Instruction::FcvtDoubleToSingle { vd, vn } => {
                emitter.fcvt_s_d(fp(*vd), fp(*vn));
            }

            // -- FP Load / Store --
            //
            // Bug-fix (aarch64 parity audit 2026-08-01, FP twin of ARM64 BUG #1):
            // the previous lowering was `*offset as u16` straight into the
            // SCALED UNSIGNED-offset form. Every FP spill slot is at a NEGATIVE
            // offset from FP (`Arm64FrameLayout::spill_offset` is always < 0),
            // and `-24i32 as u16` is 65512, which the encoder then scales by 8
            // — a load/store roughly 64 KiB ABOVE FP, i.e. into the caller's
            // frame. Silent memory corruption on every `fstore`/`fload` of a
            // spilled float or double.
            //
            // Routing now mirrors the GPR `Ldr`/`Str` arms exactly:
            //   * offset >= 0, correctly scaled, in range → scaled unsigned form
            //   * offset in the signed imm9 range (−256..=255) → unscaled,
            //     non-writeback LDUR/STUR (FP variants)
            //   * otherwise → materialize the address into IP0 (X16) and use a
            //     zero-offset access.
            Arm64Instruction::FpLdr {
                vt,
                rn,
                offset,
                is_double,
            } => {
                // Scaled unsigned form: imm12 scaled by the access size, so the
                // reachable byte range is 8*4095 for D and 4*4095 for S.
                let (scale, max_scaled) = if *is_double {
                    (8i32, 32760i32)
                } else {
                    (4, 16380)
                };
                if *offset >= 0 && *offset % scale == 0 && *offset <= max_scaled {
                    // Cast: range-checked on the line above, and the encoder
                    // re-checks and refuses rather than masking.
                    let ok = if *is_double {
                        emitter.ldr_fp_d(fp(*vt), r(*rn), *offset as u16)
                    } else {
                        emitter.ldr_fp_s(fp(*vt), r(*rn), *offset as u16)
                    };
                    if !ok {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    if *is_double {
                        emitter.ldur_fp_d(fp(*vt), r(*rn), *offset as i16);
                    } else {
                        emitter.ldur_fp_s(fp(*vt), r(*rn), *offset as i16);
                    }
                } else {
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    if *is_double {
                        emitter.ldur_fp_d(fp(*vt), crate::aarch64::Reg::X16, 0);
                    } else {
                        emitter.ldur_fp_s(fp(*vt), crate::aarch64::Reg::X16, 0);
                    }
                }
            }
            Arm64Instruction::FpStr {
                vt,
                rn,
                offset,
                is_double,
            } => {
                let (scale, max_scaled) = if *is_double {
                    (8i32, 32760i32)
                } else {
                    (4, 16380)
                };
                if *offset >= 0 && *offset % scale == 0 && *offset <= max_scaled {
                    // Cast: range-checked on the line above; the encoder
                    // refuses anything it cannot encode exactly.
                    let ok = if *is_double {
                        emitter.str_fp_d(fp(*vt), r(*rn), *offset as u16)
                    } else {
                        emitter.str_fp_s(fp(*vt), r(*rn), *offset as u16)
                    };
                    if !ok {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    if *is_double {
                        emitter.stur_fp_d(fp(*vt), r(*rn), *offset as i16);
                    } else {
                        emitter.stur_fp_s(fp(*vt), r(*rn), *offset as i16);
                    }
                } else {
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    if *is_double {
                        emitter.stur_fp_d(fp(*vt), crate::aarch64::Reg::X16, 0);
                    } else {
                        emitter.stur_fp_s(fp(*vt), crate::aarch64::Reg::X16, 0);
                    }
                }
            }

            // -- NEON FP (double) --
            Arm64Instruction::FaddDouble { vd, vn, vm } => {
                emitter.fadd_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FsubDouble { vd, vn, vm } => {
                emitter.fsub_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FmulDouble { vd, vn, vm } => {
                emitter.fmul_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FdivDouble { vd, vn, vm } => {
                emitter.fdiv_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FcmpDouble { vn, vm } => {
                emitter.fcmp_d(fp(*vn), fp(*vm));
            }

            // -- NEON SIMD (integer vector, 4x32) --
            Arm64Instruction::NeonLd1_4s { vt, rn } => {
                emitter.ld1_4s(fp(*vt), r(*rn));
            }
            Arm64Instruction::NeonSt1_4s { vt, rn } => {
                emitter.st1_4s(fp(*vt), r(*rn));
            }
            Arm64Instruction::NeonAdd4s { vd, vn, vm } => {
                emitter.add_v4s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::NeonMul4s { vd, vn, vm } => {
                emitter.mul_v4s(fp(*vd), fp(*vn), fp(*vm));
            }

            // -- 32-bit (W) integer forms --
            Arm64Instruction::AddW { rd, rn, rm } => emitter.add_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::SubW { rd, rn, rm } => emitter.sub_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::MulW { rd, rn, rm } => emitter.mul_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::AndW { rd, rn, rm } => emitter.and_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::OrrW { rd, rn, rm } => emitter.orr_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::EorW { rd, rn, rm } => emitter.eor_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::LslW { rd, rn, rm } => emitter.lsl_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::LsrW { rd, rn, rm } => emitter.lsr_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::AsrW { rd, rn, rm } => emitter.asr_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::NegW { rd, rn } => emitter.neg_w(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::SDivW { rd, rn, rm } => {
                emitter.sdiv_w(r_gp(*rd), r_gp(*rn), r_gp(*rm))
            }
            Arm64Instruction::MsubW { rd, rn, rm, ra } => {
                emitter.msub_w(r_gp(*rd), r_gp(*rn), r_gp(*rm), r_gp(*ra))
            }
            Arm64Instruction::AddImmW { rd, rn, imm } => {
                if !emit_addsub_imm_w(&mut emitter, r_gp(*rd), r_gp(*rn), *imm, false) {
                    return None;
                }
            }
            Arm64Instruction::SubImmW { rd, rn, imm } => {
                if !emit_addsub_imm_w(&mut emitter, r_gp(*rd), r_gp(*rn), *imm, true) {
                    return None;
                }
            }
            Arm64Instruction::CmpW { rn, rm } => emitter.cmp_w(r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::CmpImmW { rn, imm } => {
                emit_w_cmp_const(&mut emitter, r_gp(*rn), *imm)
            }
            Arm64Instruction::CbzW { rt, label } => {
                let pos = emitter.cbz_w(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::CbnzW { rt, label } => {
                let pos = emitter.cbnz_w(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Sxtw { rd, rn } => emitter.sxtw(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::Sxth { rd, rn } => emitter.sxth(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::Sxtb { rd, rn } => emitter.sxtb(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::AndImm { rd, rn, imm } => {
                if !emitter.and_imm(r_gp(*rd), r_gp(*rn), *imm) {
                    if rn.0 == 16 {
                        return None;
                    }
                    emitter.mov_imm64(crate::aarch64::Reg::X16, *imm);
                    emitter.and(r_gp(*rd), r_gp(*rn), crate::aarch64::Reg::X16);
                }
            }
            Arm64Instruction::Cset { rd, cond } => emitter.cset(r_gp(*rd), to_cond(cond)),
            Arm64Instruction::Cneg { rd, rn, cond } => {
                emitter.cneg(r_gp(*rd), r_gp(*rn), to_cond(cond));
            }

            // -- FP width forms --
            Arm64Instruction::FmovToFpSingle { vd, rn } => {
                emitter.fmov_s_from_w(fp(*vd), r_zr(*rn))
            }
            Arm64Instruction::FmovFromFpSingle { rd, vn } => {
                emitter.fmov_w_from_s(r_zr(*rd), fp(*vn))
            }
            Arm64Instruction::FmovFpSingle { vd, vn } => emitter.fmov_s(fp(*vd), fp(*vn)),
            Arm64Instruction::ScvtfDoubleW { vd, rn } => emitter.scvtf_d_w(fp(*vd), r_zr(*rn)),
            Arm64Instruction::ScvtfSingleX { vd, rn } => emitter.scvtf_s_x(fp(*vd), r_zr(*rn)),
            Arm64Instruction::FcvtzsIntW { rd, vn } => emitter.fcvtzs_w_d(r_zr(*rd), fp(*vn)),
            Arm64Instruction::FcvtzsSingleX { rd, vn } => emitter.fcvtzs_x_s(r_zr(*rd), fp(*vn)),

            // -- Switches --
            Arm64Instruction::TableSwitch {
                key,
                low,
                default,
                targets,
            } => {
                use crate::aarch64::{Cond, Reg};
                let Some(max_index) = targets
                    .len()
                    .checked_sub(1)
                    .and_then(|m| u32::try_from(m).ok())
                else {
                    return None;
                };
                // X17 = key - low, wrapping at 32 bits, so a key below `low`
                // becomes a huge index and fails the unsigned check below.
                emit_w_sub_const(&mut emitter, Reg::X17, r_gp(*key), *low);
                if max_index <= 0xFFF {
                    // Cast: range-checked.
                    emitter.cmp_imm_w(Reg::X17, max_index as u16);
                } else {
                    emitter.mov_imm64(Reg::X16, u64::from(max_index));
                    emitter.cmp_w(Reg::X17, Reg::X16);
                }
                let to_default = emitter.b_cond(Cond::HI, 0);
                branch_patches.push((to_default, *default, true));
                let adr = emitter.adr(Reg::X16, 0);
                emitter.ldrsw_reg_uxtw_scaled(Reg::X17, Reg::X16, Reg::X17);
                emitter.add(Reg::X16, Reg::X16, Reg::X17);
                emitter.br(Reg::X16);
                let table = emitter.offset();
                emitter.patch_adr(adr, table);
                for &label in targets {
                    let entry = emitter.emit_u32_data(0);
                    table_patches.push((entry, table, label));
                }
            }
            Arm64Instruction::LookupSwitch {
                key,
                pairs,
                default,
            } => {
                for &(value, label) in pairs {
                    emit_w_cmp_const(&mut emitter, r_gp(*key), value);
                    let pos = emitter.b_cond(crate::aarch64::Cond::EQ, 0);
                    branch_patches.push((pos, label, true));
                }
                let pos = emitter.b(0);
                branch_patches.push((pos, *default, false));
            }

            // -- Shared-memory access (round 9 wave 10) --
            //
            // Zero offset only: the pseudo-op's address is exactly `[rn]`. The
            // ordered forms have no offset field at all, and the plain forms
            // are given 0, which every unsigned-offset encoder accepts.
            Arm64Instruction::MemLoad {
                rt,
                rn,
                width,
                acquire,
            } => {
                if *acquire {
                    match width {
                        Arm64MemWidth::B8 => emitter.ldarb(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::H16 => emitter.ldarh(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::W32 => emitter.ldar_w(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::X64 => emitter.ldar(r_zr(*rt), r(*rn)),
                    }
                } else {
                    let ok = match width {
                        Arm64MemWidth::B8 => emitter.ldrb_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::H16 => emitter.ldrh_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::W32 => emitter.ldr_imm_w(r_gp(*rt), r_gp(*rn), 0),
                        Arm64MemWidth::X64 => emitter.ldr_imm(r_zr(*rt), r(*rn), 0),
                    };
                    if !ok {
                        return None;
                    }
                }
            }
            Arm64Instruction::MemStore {
                rt,
                rn,
                width,
                release,
            } => {
                if *release {
                    match width {
                        Arm64MemWidth::B8 => emitter.stlrb(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::H16 => emitter.stlrh(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::W32 => emitter.stlr_w(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::X64 => emitter.stlr(r_zr(*rt), r(*rn)),
                    }
                } else {
                    let ok = match width {
                        Arm64MemWidth::B8 => emitter.strb_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::H16 => emitter.strh_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::W32 => emitter.str_imm_w(r_gp(*rt), r_gp(*rn), 0),
                        Arm64MemWidth::X64 => emitter.str_imm(r_zr(*rt), r(*rn), 0),
                    };
                    if !ok {
                        return None;
                    }
                }
            }
            Arm64Instruction::DmbIsh => {
                // CRm = 0b1011 (ISH).
                emitter.dmb(0b1011);
            }

            // -- System --
            Arm64Instruction::Nop => {
                emitter.nop();
            }
            Arm64Instruction::Brk { imm } => {
                emitter.brk(*imm);
            }
        }
    }

    // Patch branches.
    //
    // An unbound label is NOT recoverable here. The emitted placeholder is a
    // branch with displacement 0 — i.e. a branch to itself, an infinite loop —
    // and nothing downstream patches it: `jit::try_compile_inner`'s aarch64
    // arm copies these bytes straight into an `ExecutableBuffer` and hands the
    // entry point to the VM. The previous code deliberately left the
    // placeholder "for unresolved calls"; since `emit_invoke` always fails the
    // method and `compile_method_with_info`'s discovery pass now binds every
    // back-edge target, an unbound label can only mean a branch target that is
    // not a valid instruction boundary (malformed or truncated bytecode), so
    // the only safe action is to discard the method and interpret it.
    for &(offset, label, is_cond) in &branch_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        if is_cond {
            emitter.patch_bcond(offset, target);
        } else {
            emitter.patch_branch(offset, target);
        }
    }

    // Patch LDR literal instructions to point to their target labels.
    // The label should reference a position containing a 64-bit constant
    // (e.g. a literal pool entry appended after all code). Same reasoning as
    // above: an unpatched LDR-literal reads whatever happens to sit at
    // `pc + 0`, which is the instruction itself — garbage, not a constant.
    for &(offset, label) in &literal_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        emitter.patch_ldr_literal(offset, target);
    }

    // Patch jump-table words: each is the signed distance from the table base
    // to its case, which the dispatch adds to the base it loaded with `ADR`.
    for &(entry, base, label) in &table_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        let Ok(delta) = i32::try_from(target as i64 - base as i64) else {
            return None;
        };
        // Cast: the two's-complement word `LDRSW` sign-extends back.
        emitter.patch_u32(entry, delta as u32);
    }

    // Sticky encoding-overflow check — see this function's doc comment. Must
    // come AFTER the patch loops: `patch_branch`/`patch_bcond`/
    // `patch_ldr_literal` are themselves able to trip the flag, and in a
    // release build they are the *only* thing that reports an out-of-range
    // patch (the `debug_assert!` inside `mark_branch_overflow` is compiled
    // out).
    if emitter.overflowed() {
        return None;
    }

    // A5. The same discipline for a register that could not be encoded: `r`
    // and `fp` noted it and kept going, and the whole body is discarded here.
    // Before this, both helpers `.expect()`ed and a regalloc bug killed the
    // process instead of costing one method its compiled form.
    if take_invalid_register() {
        return None;
    }

    // The sentinel: a safepoint recorded at the very end of the stream has a
    // following pseudo-op index of `instructions.len()`, whose byte offset is
    // the end of the code.
    pseudo_offsets.push(emitter.offset());
    Some((emitter.code().to_vec(), pseudo_offsets))
}

/// Encode a compiled method.
pub fn emit_machine_code(result: &Arm64CompileResult) -> Option<Vec<u8>> {
    emit_machine_code_inner(result).map(|(code, _)| code)
}

/// This frame's storage-class partition, for the GC's band verifier.
///
/// Without it `CompiledMethod::frame_layout` stays all-zero, and a zero layout
/// tells the verifier that NOTHING is a register image -- so the prologue's
/// saved FP/LR pair and the caller's saved X19-X28 all count as in-band words
/// of THIS frame. Those hold the CALLER's live references, which this frame's
/// maps have no business naming, so the oracle would report them `never_mapped`
/// and refute a coverage claim on pure noise. An oracle that cries wolf is
/// worse than one that is off.
///
/// The saved FP/LR pair is the frame record AT FP (`[FP]`, `[FP+8]`), outside
/// the `[FP - size, FP)` band altogether -- where x86-64 keeps its saved RBP and
/// return address. Below FP come the callee-saved GPRs, and the spill area
/// BELOW those, which is the mirror of x86-64's order. `callee_saved_shallow`
/// says so, so the verifier uses the range exclusion instead of x86-64's
/// "everything at or beyond `callee_saved_lo`" half-line, which here would
/// swallow the spill area -- the one region the oop maps describe.
///
/// Offsets are positive, meaning `[FP - off]`, matching the x64 convention the
/// consumer expects.
fn arm64_frame_layout(frame: &Arm64FrameLayout) -> crate::FrameLayout {
    let mut out = crate::FrameLayout::default();
    // Register images: the callee-saved GPRs, `[FP-8]` down to
    // `[FP-callee_save_bytes]`. With none saved there is no image in the band.
    if !frame.saved_regs.is_empty() {
        out.callee_saved_lo = 8;
        // `-callee_save_offset` is the DEEPEST saved-register offset; `+8`
        // makes the range half-open over it.
        out.callee_saved_hi = (-frame.callee_save_offset) + 8;
    }
    out.callee_saved_shallow = true;
    // The spill area: frame-homed locals, then operand slots, then the
    // safepoint homes and the sp-id word. Every one of those is described by
    // the dataflow or the operand marks, which is what lets the verifier treat
    // a word the active map does not name as DEAD rather than missed.
    if frame.num_spills > 0 {
        let deepest = -frame.spill_offset; // slot 0 is the deepest word
        let shallowest = -(frame.spill_offset + (frame.num_spills as i32 - 1) * 8);
        out.spill_lo = shallowest;
        out.spill_hi = deepest + 8;
    }
    // `java_locals_hi` is deliberately left 0: this backend has no
    // `[FP - (i+1)*8]` local convention -- a local is either in a callee-saved
    // register or in the spill area above.
    out
}

/// Build the publishable artifact for an aarch64 compilation: encode it, and
/// attach the oop maps the GC will read.
///
/// # Why this lives here and not at the call site
///
/// Its caller in `try_compile_inner` sits behind
/// `#[cfg(target_arch = "aarch64")]`, so on an x86-64 developer host or CI
/// runner that block is not compiled AT ALL -- it is never type-checked, never
/// linted and never tested. That is how the publication path came to drop
/// `oop_maps` on the floor without anything noticing: it built its
/// `CompiledMethod` with `CompiledMethod::new(buf)` and never transferred the
/// backend's maps, so even a correct map writer would have produced nothing
/// observable.
///
/// Keeping the logic in a function with no `cfg` on it means every `cargo test`
/// run on any host compiles and exercises it (see
/// `tests::a_published_artifact_carries_its_resolved_oop_maps`), and the gated
/// caller shrinks to one line that cannot silently rot.
pub fn publish_compiled_method(result: &Arm64CompileResult) -> Option<crate::CompiledMethod> {
    let (machine_code, oop_maps) = emit_machine_code_with_oop_maps(result)?;
    // `new_in`, not `new`: the process-wide JIT code arena carves this body
    // out of a shared executable reservation instead of taking its own
    // `VirtualAlloc`/`mmap`. Behaviour is byte-identical while
    // `CRATONVM_JIT_CODE_ARENA` is unset, because `new_in` falls straight
    // through to `new` then — so this call site is what makes the flag
    // reachable, not what changes the default.
    let mut buf = crate::ExecutableBuffer::new_in(
        crate::platform::shared_jit_code_arena(),
        machine_code.len().max(4096),
    )?;
    buf.set_tag("aarch64-backend");
    buf.emit(&machine_code);
    // `try_new`: a refused RW->RX flip declines the compile instead of
    // aborting the process (round 9 wave 3).
    //
    // ...and `try_new_with_context` when the body takes the VM context pointer
    // (round 9 wave 16). The two constructors differ in `needs_context` and in
    // nothing else, and that flag is what makes the VM enter through
    // `try_call_with_context` -- which passes the context in X0, which is
    // exactly where this body's prologue homes it from. Getting this wrong in
    // either direction shifts every Java argument by one register.
    let mut cm = if result.needs_context {
        crate::CompiledMethod::try_new_with_context(buf)?
    } else {
        crate::CompiledMethod::try_new(buf)?
    };
    // The transfer that was missing. `has_precise_oop_maps()` becomes true when
    // this is non-empty, which makes the walker enumerate these slots IN
    // ADDITION to its conservative sweep -- strictly additive, because
    // suppressing the sweep is gated on `fully_oop_covered`, which this backend
    // never sets -- not for want of the safepoint-id slot, which exists now,
    // but because suppressing the conservative scan is a claim no non-aarch64
    // host can earn.
    cm.oop_maps = oop_maps;
    // The slot the maps above are keyed through. Without it the runtime's
    // `active_safepoint_id` returns `None` and no map can be selected by id.
    cm.sp_id_slot_off = result.sp_id_slot_off;
    // The storage-class partition the band verifier needs to tell this frame's
    // words from the caller's saved registers. Publishing a zero layout would
    // make the oracle report the caller's live references as this frame's
    // missed roots.
    cm.frame_layout = arm64_frame_layout(&result.frame);
    // THE SIZE THAT LAYOUT IS READ OVER (round 9 wave 21), and the third
    // member of this wave's family: a map that is published and unreadable.
    //
    // `osr_frame_size` is named for OSR and is not about OSR -- "normal
    // compiled entries populate it too", says the band walker, and x64's
    // single-pass driver sets it from its own `frame_size`. It is how a
    // compiled frame says which words are ITS OWN: everything that walks one
    // (`scan_compiled_frame_bands`, `remap_active_jit_frames`,
    // `verify_precise_covers_conservative`) reads `[rbp - osr_frame_size, rbp)`
    // and REFUSES THE FRAME when the field is `0`. This backend never set it,
    // so no aarch64 frame was ever band-walked, remapped or verified -- which
    // is also why arming the oracle would have found nothing to look at.
    //
    // The value is the distance from FP down to SP, which is what the prologue
    // subtracts: `STP X29, X30, [SP, #-16]!` puts FP 16 bytes below the old
    // SP, and `SUB SP, SP, #(frame_size - 16)` is the rest. `frame_size`
    // itself would reach 16 bytes PAST SP into the callee's frame, and a false
    // positive there is exactly what `arm64_frame_layout` exists to prevent.
    // The saved FP/LR pair sits AT `[FP]`/`[FP+8]`, above the band, which is
    // where x86-64 keeps its saved RBP and return address.
    cm.osr_frame_size = result.frame.frame_size.saturating_sub(16);
    // The declaring classes of every static this body reads directly (round 9
    // wave 10), as the x64 tier records them: the interpreter's compiled-entry
    // path ensure-initializes them once per artifact before first execution.
    cm.static_init_classes = result.static_init_classes.clone();
    // FULLY OOP COVERED -- the claim that lets the collector SUPPRESS its
    // conservative scan of these frames, so every term is a thing that had to
    // be built rather than assumed:
    //
    //   * an id slot, or `active_safepoint_id` returns `None` and no map can be
    //     selected at all;
    //   * at least one map, so the claim is not vacuously true for a method
    //     that emitted no safepoint (a method with no poll is not "covered",
    //     it is unobserved);
    //   * one map per safepoint, so every id resolves -- `find_oop_map_for_safepoint_id`
    //     finding nothing is indistinguishable from a frame that is not covered;
    //   * and NO safepoint that failed to describe what was live at it
    //     (`incomplete_oop_maps`), which is a count and not a set of bytecode
    //     pcs, because two safepoints can share one bci and a set lets a
    //     complete map mask an incomplete one beside it.
    //
    //   * and A PUBLISHED FRAME BASE (`frame_base_published`, round 9 wave
    //     21). Every term above is addressed from it -- the id at
    //     `[frame_base - sp_id_slot_off]`, each slot at `[frame_base - off]` --
    //     and the runtime learns it only from `emit_frame_record`. Without
    //     that call `PreciseFrameInfo::exact_rbp` stays `0` and every precise
    //     path declines the frame, so the maps are unreadable and a coverage
    //     claim over them describes nothing. It is reachable: the helper slot
    //     is `0` under `CRATONVM_NO_PRECISE_JIT_MAPS=1` with no moving young
    //     generation.
    //
    // WHAT IT STILL RESTS ON, stated because it is the whole risk: the
    // operand-oop marks are exact BY CONSTRUCTION (lockstep push/pop, `dup`
    // carrying its mark, and references entering only through `aconst_null` and
    // `aload*`), and the local oop-ness comes from the flow-sensitive dataflow.
    // `CRATONVM_DBG_VERIFY_OOP_MAPS` is the runtime oracle that turns it from a
    // construction into evidence, and it arms itself on this backend rather
    // than waiting to be remembered (see `verify_oop_maps_enabled`).
    //
    // ROUND 9 WAVE 21 DROPPED `polls_enabled`. Wave 18 added it to withhold the
    // claim from a method whose only safepoints are allocation calls, on the
    // grounds that it was the strongest claim the backend makes and that wave
    // was not the wave to widen it in. The argument for widening was already
    // written there and is unchanged -- with polls off, the frame can be
    // stopped only inside a call, and every call this backend emits either
    // records a map (`emit_helper_call_at_safepoint`: the allocation helpers,
    // `jit_aaload`, `jit_aastore_type_check`, `jit_aastore`) or carries a
    // specific argument for why it cannot safepoint (`emit_helper_call`: the
    // field helpers and `dispatch_threw`; `jit_frame_record`, which writes one
    // thread-local; the NPE, AIOOBE, arithmetic and `athrow` stubs, which
    // allocate nothing and run no Java, because the exception object is built
    // later by the interpreter's drain on a stack these frames have left).
    //
    // What changed is that the claim was not yet WORTH widening: wave 18 also
    // shipped the frame-record gap above, so an allocation-only method's maps
    // could not be selected in the first place and a coverage bit over them
    // would have been exactly the vacuous claim
    // `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`
    // warns about. With the base published the claim is about something.
    cm.fully_oop_covered = result.frame_base_published
        && result.sp_id_slot_off != 0
        && !cm.oop_maps.is_empty()
        && cm.oop_maps.len() == result.safepoint_count
        && result.incomplete_oop_maps == 0;
    Some(cm)
}

/// Encode a compiled method AND resolve its oop maps to real byte offsets.
///
/// This is the only way an `OopMapEntry` is ever produced on this backend. The
/// compiler records [`Arm64PendingOopMap`]s keyed by pseudo-op index because a
/// byte offset is not knowable until the encoder has run: the pseudo-op stream
/// is not fixed-width (`Label`/`Comment` emit nothing, `ConstantPoolEntry`
/// emits 8 bytes, `MovImm`/`AddImm`/`CmpImm` and out-of-range `Ldr`/`Str`
/// expand to 1-4 words). Keying a map by `instruction_count * 4` -- what this
/// backend used to do before the writer was made fail-closed in the 2026-08-01
/// parity audit -- lands the GC on the WRONG FRAME SLOTS at a real safepoint.
///
/// Fails closed: a pending map whose `pseudo_index` is not a valid index
/// discards the whole method rather than publish a map with a guessed PC.
pub fn emit_machine_code_with_oop_maps(
    result: &Arm64CompileResult,
) -> Option<(Vec<u8>, Vec<crate::OopMapEntry>)> {
    let (code, pseudo_offsets) = emit_machine_code_inner(result)?;
    let mut maps: Vec<crate::OopMapEntry> = Vec::with_capacity(result.pending_oop_maps.len());
    for pending in &result.pending_oop_maps {
        let idx = pending.pseudo_index as usize;
        // `pseudo_offsets` carries the trailing sentinel, so a safepoint at the
        // very end of the stream is in range; anything past that is a bug in
        // the writer, not a method we may publish a map for.
        let Some(&byte_off) = pseudo_offsets.get(idx) else {
            return None;
        };
        let Ok(native_pc_offset) = u32::try_from(byte_off) else {
            return None;
        };
        maps.push(crate::OopMapEntry {
            native_pc_offset,
            // The safepoint id this map belongs to, and the reason the frame
            // stamps the same value into its sp-id slot: this is what
            // `find_oop_map_for_safepoint_id` matches on, so the collector
            // selects the map for the site the frame is ACTUALLY standing at
            // rather than a union over every safepoint in the method.
            bytecode_pc: pending.safepoint_id,
            frame_slot_offsets: pending.frame_slot_offsets.clone(),
            // No shadow stack and no relocation support on this backend, and
            // register-resident oops are covered only by the CONSERVATIVE walk
            // (see `emit_oop_map_for_safepoint`) -- which marks but cannot
            // rewrite. Claiming moving-young coverage here would be the exact
            // false claim `relocation_coverage_complete` exists to prevent.
            moving_young_coverage_complete: false,
            // AArch64 publishes no blind GPR spill image, so there is no
            // slot set for a mask to narrow. `None` is the honest answer.
            reg_oop_mask: None,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
    }
    Some((code, maps))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- to_reg / to_fpreg tests (C11) --------------------------------------
    //
    // Note: `to_reg` / `to_fpreg` return `Option` and neither asserts. The
    // `debug_assert!`s they used to carry fired on exactly the inputs the
    // refusal path exists for, which made that path untestable and made a
    // debug build abort where a release build merely declined the method.

    #[test]
    fn every_gpr_encoding_converts_and_thirty_one_is_not_a_gpr() {
        // 0..=30 are general-purpose registers through every conversion.
        for n in 0..=30u8 {
            assert!(crate::aarch64::Reg::from_u8(n).is_some(), "X{n} is a GPR");
            assert_eq!(r_zr(Arm64Register(n)).enc(), u32::from(n), "r_zr X{n}");
            assert_eq!(r(Arm64Register(n)).enc(), u32::from(n), "r X{n}");
        }
        // 31 is not a GPR. It is SP to `r` and XZR to `r_zr` -- the same bits,
        // and before A8 the same VALUE, which is the whole reason for the split.
        assert!(crate::aarch64::Reg::from_u8(31).is_none());
        assert_eq!(r(Arm64Register(31)), crate::aarch64::SP);
        assert_eq!(r_zr(Arm64Register(31)), crate::aarch64::XZR);
        assert_eq!(r(Arm64Register(31)).enc(), 31);
        assert_eq!(r_zr(Arm64Register(31)).enc(), 31);
        assert!(!take_invalid_register(), "none of the above is a violation");
    }

    #[test]
    fn to_fpreg_accepts_v0_through_v7() {
        for n in 32..=39u8 {
            assert!(
                to_fpreg(Arm64Register(n)).is_some(),
                "to_fpreg({n}) should be Some for valid FP reg"
            );
        }
    }

    /// A5. A register the allocator should never have produced must cost the
    /// METHOD its compiled body, not the process.
    ///
    /// `r` and `fp` used to `.expect()`. The FP one has already been a live
    /// panic once, when `D8`-`D15` arrived as `Arm64Register(40..=47)`.
    #[test]
    fn an_unencodable_register_refuses_the_body_instead_of_panicking() {
        assert_eq!(
            crate::aarch64::Reg::from_u8(32),
            None,
            "32 is not a GPR encoding"
        );
        // 40 is D8, which IS encodable: `FpReg` models D0..D31, so the FP half
        // of the register file runs 32..=63. An earlier revision asserted
        // `None` here on the strength of `to_fpreg`'s doc comment, which said
        // "V0=32..V7=39" — that describes the register ALLOCATOR's pool, not
        // the conversion's range, and the comment is corrected with this test.
        assert_eq!(
            to_fpreg(Arm64Register(40)),
            Some(crate::aarch64::FpReg::D8),
            "40 is D8 and is a valid encoding"
        );
        assert_eq!(to_fpreg(Arm64Register(64)), None, "64 is past D31");

        // The shorthands must not panic, and must leave the sticky flag set.
        let _ = take_invalid_register();
        let _ = r(Arm64Register(32));
        assert!(
            take_invalid_register(),
            "`r` must record an unencodable register"
        );
        let _ = fp(Arm64Register(64));
        assert!(
            take_invalid_register(),
            "`fp` must record an unencodable register"
        );
        // ...and reading it clears it, so one bad method does not poison the
        // next one compiled on this thread.
        assert!(!take_invalid_register());
    }

    /// A8. A shifted-register operand field must refuse encoding 31.
    ///
    /// `r` and `r_gp` disagree about exactly one input, and that disagreement is
    /// the contract at `aarch64::XZR` made checkable. `Arm64Register::SP` and
    /// `Arm64Register::XZR` are the same value, so neither function can tell
    /// which one a caller meant — but the CALL SITE knows which encoding it is
    /// about to emit, and in a shifted-register form 31 is never SP.
    ///
    /// A legitimate zero operand is written as the literal `aarch64::XZR`, which
    /// is a constant and therefore cannot be a forwarded parameter. The `Neg`
    /// arm is the one production instance and it does exactly that.
    #[test]
    fn a_shifted_register_operand_refuses_encoding_thirty_one() {
        let _ = take_invalid_register();

        // `r` accepts it, because `SUB SP, SP, #imm` and `STP FP, LR, [SP, #-16]!`
        // are real instructions this prologue emits.
        assert_eq!(
            r(Arm64Register::SP),
            crate::aarch64::SP,
            "the immediate and load/store arms still need SP"
        );
        assert!(
            !take_invalid_register(),
            "and accepting it must not record a violation"
        );

        // `r_gp` refuses it, and records, so `emit_machine_code_inner` discards
        // the body rather than emitting `ADD Xd, XZR, Xm` where the caller
        // meant `ADD Xd, SP, Xm`.
        let _ = r_gp(Arm64Register::SP);
        assert!(
            take_invalid_register(),
            "a 31 reaching a shifted-register field must cost the method its body"
        );
        // `XZR` is the same value, which is why this is a statement about the
        // path and not about the register.
        let _ = r_gp(Arm64Register::XZR);
        assert!(take_invalid_register());

        // Everything the allocator can actually produce passes through
        // unchanged: X19-X28 is `regalloc::ARM64_LOCAL_GPRS`.
        for n in 0..=30u8 {
            // `r_gp` answers a `Reg`, `r` the SP-capable `RegSp`; on X0..X30
            // they must name the same register.
            assert_eq!(
                crate::aarch64::RegSp::X(r_gp(Arm64Register(n))),
                r(Arm64Register(n)),
                "X{n} must be unaffected"
            );
        }
        assert!(
            !take_invalid_register(),
            "and none of them may record a violation"
        );
    }

    /// A4. A far `[base + offset]` store whose stored VALUE is IP0 writes the
    /// address instead of the value.
    ///
    /// `emit_addr_into_ip0` leaves the effective address in X16 and the `STUR`
    /// then reads `rt`. If `rt` is X16 the value is already gone. The `AndImm`
    /// arm guards the sibling case (`rn.0 == 16`) and three arms did not; the
    /// claim that "IP0 is never a regalloc output" is true of the ALLOCATOR and
    /// not of this backend's own X16 uses (the stack-bang probe, the
    /// `tableswitch` scratch, the poll and frame-record address
    /// materialisation).
    #[test]
    fn a_far_store_refuses_when_its_value_or_base_is_ip0() {
        let far = 100_000i32; // past both the scaled and the imm9 forms
        let with = |inst: Arm64Instruction| {
            let mut result = make_backend_with_method(1, 1, &[0xb1]); // return
            result.instructions.push(inst);
            emit_machine_code(&result)
        };

        // The control: a far store with ordinary registers still encodes.
        assert!(
            with(Arm64Instruction::Str {
                rt: Arm64Register::X0,
                rn: Arm64Register::X1,
                offset: far,
            })
            .is_some(),
            "a far store off an ordinary base must still encode"
        );

        // The value is IP0: the address materialisation destroyed it.
        assert!(
            with(Arm64Instruction::Str {
                rt: Arm64Register(16),
                rn: Arm64Register::X1,
                offset: far,
            })
            .is_none(),
            "STR with rt = IP0 and a far offset would store the ADDRESS"
        );

        // The base is IP0: `mov_imm64` overwrote it before the ADD read it.
        assert!(
            with(Arm64Instruction::Str {
                rt: Arm64Register::X0,
                rn: Arm64Register(16),
                offset: far,
            })
            .is_none(),
            "STR with rn = IP0 and a far offset computes offset + offset"
        );

        // The load has only the base hazard -- writing rt after the address is
        // consumed is fine -- but the base hazard is the same one.
        assert!(
            with(Arm64Instruction::Ldr {
                rt: Arm64Register::X0,
                rn: Arm64Register(16),
                offset: far,
            })
            .is_none(),
            "LDR with rn = IP0 and a far offset computes offset + offset"
        );
    }

    /// A3, end to end: a pair offset the encoder cannot carry must discard the
    /// body rather than encode a different offset.
    #[test]
    fn a_pair_offset_outside_the_signed_imm7_discards_the_body() {
        let with_offset = |offset: i32| {
            let mut result = make_backend_with_method(1, 1, &[0xb1]);
            result.instructions.push(Arm64Instruction::Stp {
                rt1: Arm64Register::X0,
                rt2: Arm64Register::X1,
                rn: Arm64Register::SP,
                offset,
            });
            emit_machine_code(&result)
        };

        assert!(with_offset(504).is_some(), "504 is encodable");
        assert!(with_offset(-512).is_some(), "-512 is encodable");

        for offset in [512i32, 1024, -520, -12, 70_000] {
            assert!(
                with_offset(offset).is_none(),
                "STP offset {offset} is not encodable and must discard the body"
            );
        }
    }

    /// The flag has to be consulted, not merely set: a body containing an
    /// unencodable register must not be published.
    #[test]
    fn emit_machine_code_discards_a_body_with_an_unencodable_register() {
        let mut result = make_backend_with_method(1, 1, &[0xb1]); // return
        assert!(
            emit_machine_code(&result).is_some(),
            "the control: this method encodes"
        );
        // Plant the shape a regalloc bug would produce: a "GPR" of 32.
        result.instructions.push(Arm64Instruction::Mov {
            rd: Arm64Register(32),
            rm: Arm64Register::X0,
        });
        assert!(
            emit_machine_code(&result).is_none(),
            "a body containing an unencodable register must be discarded"
        );
        // And the refusal is not sticky across methods.
        let clean = make_backend_with_method(1, 1, &[0xb1]);
        assert!(emit_machine_code(&clean).is_some());
    }

    // -- Arm64Register tests ------------------------------------------------

    #[test]
    fn register_callee_saved_x19_through_x28() {
        for i in 19..=28u8 {
            assert!(
                Arm64Register(i).is_callee_saved(),
                "X{} should be callee-saved",
                i
            );
        }
        assert!(!Arm64Register::X0.is_callee_saved());
        assert!(!Arm64Register::FP.is_callee_saved());
        assert!(!Arm64Register::LR.is_callee_saved());
    }

    #[test]
    fn register_arg_regs() {
        for i in 0..=7u8 {
            assert!(Arm64Register(i).is_arg_reg(), "X{} should be arg reg", i);
        }
        assert!(!Arm64Register::X8.is_arg_reg());
        assert!(!Arm64Register::X19.is_arg_reg());
    }

    #[test]
    fn register_index() {
        assert_eq!(Arm64Register::X0.index(), 0);
        assert_eq!(Arm64Register::FP.index(), 29);
        assert_eq!(Arm64Register::SP.index(), 31);
    }

    // -- Arm64Condition tests -----------------------------------------------

    #[test]
    fn condition_all_variants_defined() {
        // Verify all 11 variants are distinct and constructible.
        let conds = [
            Arm64Condition::Eq,
            Arm64Condition::Ne,
            Arm64Condition::Lt,
            Arm64Condition::Le,
            Arm64Condition::Gt,
            Arm64Condition::Ge,
            Arm64Condition::Hi,
            Arm64Condition::Ls,
            Arm64Condition::Cs,
            Arm64Condition::Cc,
            Arm64Condition::Al,
        ];
        assert_eq!(conds.len(), 11);
        // Check they're not all equal.
        assert_ne!(conds[0], conds[1]);
    }

    // -- Entry convention tests ---------------------------------------------

    /// A `double` argument arrives in an X register, not in `D0`.
    ///
    /// This is the fact that makes the type NOT AAPCS64 (finding A9), and it
    /// was invisible: the type was named `Arm64CallingConvention` and
    /// documented as "AAPCS64 calling convention constants and helpers", so the
    /// obvious reading — first FP argument in `V0` — was wrong and nothing
    /// said so. The prologue is the authority, so the prologue is what this
    /// asserts: `f(double)` must home `X0`, and nothing may read `V0`.
    #[test]
    fn the_entry_convention_passes_every_argument_in_an_x_register() {
        let mut b = Arm64Backend::new();
        // One `double` parameter (two JVM slots) in a frame-homed local, and a
        // body that does nothing: only the prologue's homing is under test.
        b.local_regs = vec![None, None];
        b.float_local_regs = vec![None, None];
        b.install_frame(
            2,
            Arm64SpillArea {
                locals: 2,
                operands: 2,
                ..Arm64SpillArea::default()
            },
            &[],
        );
        b.emit_argument_homing(&[0]);
        assert!(!b.failed, "one argument must home");

        let ops = b.buffer.instructions();
        assert!(
            ops.iter().any(|i| matches!(
                i,
                Arm64Instruction::Str { rt, .. } if *rt == Arm64Register::X0
            )),
            "the argument is read out of X0, whatever its Java type: {ops:?}"
        );
        assert!(
            !ops.iter().any(|i| matches!(
                i,
                Arm64Instruction::FpStr { vt, .. } if *vt == Arm64Register::V0
            )),
            "nothing may read V0 — AAPCS64 would put the double there and this \
             prologue would never see it"
        );

        // And the ninth argument is refused rather than read from the caller's
        // frame: this convention has no stack argument area.
        let mut b2 = Arm64Backend::new();
        b2.local_regs = vec![None; 9];
        b2.float_local_regs = vec![None; 9];
        b2.install_frame(
            9,
            Arm64SpillArea {
                locals: 9,
                ..Arm64SpillArea::default()
            },
            &[],
        );
        b2.emit_argument_homing(&[0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(b2.failed, "a ninth argument has nowhere to come from");
    }

    #[test]
    fn calling_convention_int_arg_regs() {
        assert_eq!(Arm64EntryConvention::INT_ARG_REGS.len(), 8);
        assert_eq!(
            Arm64EntryConvention::int_arg_reg(0),
            Some(Arm64Register::X0)
        );
        assert_eq!(
            Arm64EntryConvention::int_arg_reg(7),
            Some(Arm64Register::X7)
        );
        assert_eq!(Arm64EntryConvention::int_arg_reg(8), None);
    }

    #[test]
    fn calling_convention_callee_saved_count() {
        assert_eq!(Arm64EntryConvention::CALLEE_SAVED.len(), 10);
    }

    #[test]
    fn calling_convention_local_reg_mapping() {
        assert_eq!(Arm64EntryConvention::local_reg(0), Some(Arm64Register::X19));
        assert_eq!(Arm64EntryConvention::local_reg(9), Some(Arm64Register::X28));
        assert_eq!(Arm64EntryConvention::local_reg(10), None);
    }

    #[test]
    fn calling_convention_stack_alignment_is_16() {
        assert_eq!(Arm64EntryConvention::STACK_ALIGNMENT, 16);
    }

    #[test]
    fn calling_convention_no_red_zone() {
        assert_eq!(Arm64EntryConvention::RED_ZONE, 0);
    }

    // -- FrameLayout tests --------------------------------------------------

    #[test]
    fn frame_layout_zero_locals() {
        let frame = Arm64FrameLayout::compute(0, 0, &[]);
        assert_eq!(frame.frame_size % 16, 0, "frame must be 16-byte aligned");
        assert_eq!(frame.num_spills, 0);
        assert_eq!(frame.num_reg_locals, 0);
    }

    #[test]
    fn frame_layout_five_locals() {
        let saved: Vec<_> = (0..5)
            .map(|i| Arm64EntryConvention::CALLEE_SAVED[i])
            .collect();
        let frame = Arm64FrameLayout::compute(5, 2, &saved);
        assert_eq!(frame.num_reg_locals, 5);
        assert_eq!(frame.num_spills, 2);
        assert_eq!(frame.saved_regs.len(), 5);
    }

    #[test]
    fn frame_layout_16_byte_alignment() {
        for n in 0..20 {
            let num_saved = n.min(Arm64EntryConvention::CALLEE_SAVED.len());
            let saved: Vec<_> = (0..num_saved)
                .map(|i| Arm64EntryConvention::CALLEE_SAVED[i])
                .collect();
            let frame = Arm64FrameLayout::compute(n, n, &saved);
            assert_eq!(
                frame.frame_size % 16,
                0,
                "frame_size {} not aligned for n={}",
                frame.frame_size,
                n
            );
        }
    }

    // -- CodeBuffer tests ---------------------------------------------------

    #[test]
    fn code_buffer_emit_and_count() {
        let mut buf = Arm64CodeBuffer::new();
        assert_eq!(buf.instruction_count(), 0);
        buf.emit(Arm64Instruction::Nop);
        buf.emit(Arm64Instruction::Ret);
        assert_eq!(buf.instruction_count(), 2);
    }

    #[test]
    fn code_buffer_label_creation_and_binding() {
        let mut buf = Arm64CodeBuffer::new();
        let l1 = buf.new_label();
        let l2 = buf.new_label();
        assert_ne!(l1, l2);
        buf.emit(Arm64Instruction::Nop);
        buf.bind_label(l1);
        assert!(buf.labels.contains_key(&l1));
        assert_eq!(*buf.labels.get(&l1).unwrap(), 1); // after the Nop
    }

    #[test]
    fn code_buffer_estimated_size_4_bytes_per_instr() {
        let mut buf = Arm64CodeBuffer::new();
        for _ in 0..10 {
            buf.emit(Arm64Instruction::Nop);
        }
        assert_eq!(buf.estimated_size(), 40);
    }

    // -- Backend: prologue / epilogue tests ---------------------------------

    fn make_backend_with_method(
        num_locals: usize,
        num_params: usize,
        bytecode: &[u8],
    ) -> Arm64CompileResult {
        let mut backend = Arm64Backend::new();
        backend.compile_method(num_locals, num_params, 4, bytecode)
    }

    #[test]
    fn backend_prologue_emits_stp_fp_lr() {
        let result = make_backend_with_method(0, 0, &[0xb1]); // return void
                                                              // Bug-fix (ARM64 BUG #2): the prologue now uses the writeback
                                                              // `STP FP, LR, [SP, #-16]!` form (StpPre) so SP is decremented as part
                                                              // of the save, instead of the old non-writeback `Stp` with an unsound
                                                              // "16 already consumed" SUB fudge.
        let has_stp_fp_lr = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::StpPre { rt1, rt2, offset: -16, .. }
                    if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
            )
        });
        assert!(
            has_stp_fp_lr,
            "prologue must save FP/LR with writeback STP (StpPre, #-16)"
        );
    }

    #[test]
    fn backend_epilogue_emits_ldp_fp_lr_and_ret() {
        let result = make_backend_with_method(0, 0, &[0xb1]);
        // Bug-fix (ARM64 BUG #2): the epilogue now restores FP/LR with the
        // matching post-index `LDP FP, LR, [SP], #16` (LdpPost).
        let has_ldp = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::LdpPost { rt1, rt2, offset: 16, .. }
                    if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
            )
        });
        let has_ret = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Ret));
        assert!(
            has_ldp,
            "epilogue must restore FP/LR with post-index LDP (LdpPost, #16)"
        );
        assert!(has_ret, "epilogue must emit RET");
    }

    // -- Backend: instruction emission tests --------------------------------

    #[test]
    fn backend_iconst_emits_movimm() {
        // iconst_5 (opcode 0x08) then ireturn (0xac)
        let result = make_backend_with_method(0, 0, &[0x08, 0xac]);
        let _has_mov = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 2, .. }));
        // iconst_5 = opcode 0x08 - 3 = 5
        let has_mov5 = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 5, .. }));
        assert!(has_mov5, "iconst_5 should emit MovImm with imm=5");
    }

    #[test]
    fn backend_int_add_emits_add() {
        // iconst_1, iconst_2, iadd, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x60, 0xac]);
        let has_add = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::AddW { .. }));
        assert!(has_add, "iadd should emit the 32-bit AddW");
    }

    #[test]
    fn backend_int_sub_emits_sub() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x64, 0xac]);
        let has_sub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::SubW { .. }));
        assert!(has_sub, "isub should emit the 32-bit SubW");
    }

    #[test]
    fn backend_int_mul_emits_mul() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x68, 0xac]);
        let has_mul = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MulW { .. }));
        assert!(has_mul, "imul should emit the 32-bit MulW");
    }

    /// `idiv` is REFUSED (see the module header and
    /// `backend_idiv_bails_to_interpreter`), and the refusal must survive
    /// compile-time-constant operands too: a constant-folding or peephole path
    /// that resolved `iconst_1 / iconst_2` before the opcode arm ran would
    /// reintroduce the `BRK #1`/SIGTRAP lowering for the shape that looks
    /// safest. Written as "no `SDiv` is emitted" rather than only
    /// "`!success`", so it still fails if the arm is re-wired.
    #[test]
    fn backend_int_div_bails_with_constant_operands() {
        // iconst_1, iconst_2, idiv, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x6c, 0xac]);
        assert!(
            !result.success,
            "idiv must bail, constant operands included"
        );
        let has_div = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::SDiv { .. }));
        assert!(!has_div, "no SDiv may be emitted for a refused idiv");
    }

    #[test]
    fn backend_int_neg_emits_neg() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x74, 0xac]);
        let has_neg = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::NegW { .. }));
        assert!(has_neg, "ineg should emit the 32-bit NegW");
    }

    #[test]
    fn backend_iload_from_register() {
        // iload_0 (0x1a) then ireturn (0xac), with 1 local
        let result = make_backend_with_method(1, 0, &[0x1a, 0xac]);
        // iload from a register local emits Mov from a callee-saved register.
        let has_mov_from_callee_saved = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Mov { rm, .. } if rm.is_callee_saved()
            )
        });
        assert!(
            has_mov_from_callee_saved,
            "iload_0 with register local should emit Mov from callee-saved reg"
        );
    }

    #[test]
    fn backend_iload_from_spill_slot() {
        // 12 locals all loaded simultaneously to force spills (ARM64 has 10 callee-saved GPRs).
        // Load all 12, then return.
        let mut code = Vec::new();
        for i in 0..12u8 {
            code.push(0x15); // iload
            code.push(i);
        }
        code.push(0xac); // ireturn
        let result = make_backend_with_method(12, 12, &code);
        // At least 2 locals must be spilled → at least 2 Ldr from [FP + offset]
        let ldr_from_fp = result
            .instructions
            .iter()
            .filter(|inst| {
                matches!(
                    inst,
                    Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
                )
            })
            .count();
        assert!(
            ldr_from_fp >= 2,
            "with 12 simultaneous locals, at least 2 should spill, got {ldr_from_fp} LDRs from FP"
        );
    }

    #[test]
    fn backend_istore_to_register() {
        // iconst_1, istore_0, return
        let result = make_backend_with_method(1, 0, &[0x04, 0x3b, 0xb1]);
        let has_mov_to_callee_saved = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Mov { rd, .. } if rd.is_callee_saved()
            )
        });
        assert!(
            has_mov_to_callee_saved,
            "istore_0 with register local should emit Mov to callee-saved reg"
        );
    }

    #[test]
    fn backend_return_int_uses_x0() {
        // iconst_3, ireturn
        let result = make_backend_with_method(0, 0, &[0x06, 0xac]);
        let has_mov_x0 = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0
            )
        });
        assert!(has_mov_x0, "ireturn should move result to X0");
    }

    /// JVMS §6.5 `ireturn` narrowing parity with x64 (`emit_narrow_int_return`,
    /// `x64/emit.rs`): a `Z`/`B`/`C`/`S` method narrows X0 right after the
    /// result lands there, an `I` method does not, and the narrowing encodes to
    /// the exact X-form words (so the sign-extended `int` convention holds).
    #[test]
    fn r9w9_ireturn_narrows_boolean_byte_char_short_like_x64() {
        // iconst_2; ireturn
        let code = [0x05u8, 0xac];
        let cases: [(&str, Option<u32>); 5] = [
            ("()Z", Some(0x9240_0000)), // AND  X0, X0, #1
            ("()B", Some(0x9340_1C00)), // SXTB X0, W0
            ("()C", Some(0x9240_3C00)), // AND  X0, X0, #0xFFFF
            ("()S", Some(0x9340_3C00)), // SXTH X0, W0
            ("()I", None),
        ];
        for (desc, word) in cases {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            b.set_method_descriptor(desc, true);
            let result = b.compile_method(0, 0, 4, &code);
            assert!(result.success, "{desc} must compile");
            let insts = result.instructions.clone();
            let mov_at = insts
                .iter()
                .position(
                    |i| matches!(i, Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0),
                )
                .expect("ireturn moves its value into X0");
            let next = insts
                .get(mov_at + 1)
                .expect("a branch to the epilogue follows");
            let narrowed = match (desc, next) {
                ("()Z", Arm64Instruction::AndImm { rd, rn, imm: 1 })
                | (
                    "()C",
                    Arm64Instruction::AndImm {
                        rd,
                        rn,
                        imm: 0xFFFF,
                    },
                )
                | ("()B", Arm64Instruction::Sxtb { rd, rn })
                | ("()S", Arm64Instruction::Sxth { rd, rn }) => {
                    *rd == Arm64Register::X0 && *rn == Arm64Register::X0
                }
                _ => false,
            };
            assert_eq!(
                narrowed,
                word.is_some(),
                "{desc}: narrowing after the X0 move, got {next:?}"
            );
            let bytes = emit_machine_code(&result).expect("encodes");
            if let Some(w) = word {
                let found = bytes
                    .chunks_exact(4)
                    .any(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) == w);
                assert!(found, "{desc}: expected word {w:#010x} in the body");
            }
        }
    }

    // -- Round 9 wave 10: `getstatic`, `MemLoad`/`MemStore`/`DmbIsh` --------

    fn words_of(bytes: &[u8]) -> Vec<u32> {
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    fn static_field(
        class_id: u32,
        field_index: usize,
        type_tag: u8,
        is_volatile: bool,
    ) -> Arm64StaticField {
        Arm64StaticField {
            class_id,
            field_index,
            base_cell: 0x1000,
            type_tag,
            is_volatile,
        }
    }

    /// Compile `code` (a static `()`-method) with `fields` resolved.
    fn compile_with_statics(
        descriptor: &str,
        code: &[u8],
        fields: &[(usize, Arm64StaticField)],
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_method_descriptor(descriptor, true);
        b.set_static_field_info(fields.iter().copied().collect());
        b.compile_method(0, 0, 4, code)
    }

    /// Every width and ordering of the two new memory pseudo-ops, and the
    /// barrier, encode to the ARM ARM words -- in particular a `release` store
    /// is `STLR*` and an `acquire` load is `LDAR*`, never the plain form.
    #[test]
    fn r9w10_mem_load_store_pseudo_ops_encode_width_and_ordering() {
        use super::Arm64MemWidth::{B8, H16, W32, X64};
        let (rt, rn) = (Arm64Register::X9, Arm64Register::X10);
        let mut insts = Vec::new();
        for ordered in [false, true] {
            for width in [B8, H16, W32, X64] {
                insts.push(Arm64Instruction::MemLoad {
                    rt,
                    rn,
                    width,
                    acquire: ordered,
                });
            }
            for width in [B8, H16, W32, X64] {
                insts.push(Arm64Instruction::MemStore {
                    rt,
                    rn,
                    width,
                    release: ordered,
                });
            }
        }
        insts.push(Arm64Instruction::DmbIsh);
        let bytes = emit_machine_code(&result_from_instructions(insts)).expect("encodes");
        assert_eq!(
            words_of(&bytes),
            vec![
                0x3940_0149u32, // LDRB  W9, [X10]
                0x7940_0149,    // LDRH  W9, [X10]
                0xB940_0149,    // LDR   W9, [X10]
                0xF940_0149,    // LDR   X9, [X10]
                0x3900_0149,    // STRB  W9, [X10]
                0x7900_0149,    // STRH  W9, [X10]
                0xB900_0149,    // STR   W9, [X10]
                0xF900_0149,    // STR   X9, [X10]
                0x08DF_FD49,    // LDARB W9, [X10]
                0x48DF_FD49,    // LDARH W9, [X10]
                0x88DF_FD49,    // LDAR  W9, [X10]
                0xC8DF_FD49,    // LDAR  X9, [X10]
                0x089F_FD49,    // STLRB W9, [X10]
                0x489F_FD49,    // STLRH W9, [X10]
                0x889F_FD49,    // STLR  W9, [X10]
                0xC89F_FD49,    // STLR  X9, [X10]
                0xD503_3BBF,    // DMB ISH
            ]
        );
    }

    /// `getstatic` of a plain and of a `volatile` `int`: the x64 inline
    /// `getstatic` shape (pointer cell -> statics block -> 16-byte cell's
    /// 32-bit payload), a plain `LDR W` or an `LDAR W`, then `SXTW` into this
    /// backend's int form.
    #[test]
    fn r9w10_getstatic_int_reads_the_cell_payload_with_ldar_when_volatile() {
        // getstatic #1; ireturn
        let code = [0xb2u8, 0x00, 0x01, 0xac];
        for is_volatile in [false, true] {
            let result =
                compile_with_statics("()I", &code, &[(0, static_field(7, 2, b'I', is_volatile))]);
            assert!(result.success, "volatile={is_volatile}: must compile");
            assert_eq!(result.static_init_classes, vec![7]);
            let load = result
                .instructions
                .iter()
                .find_map(|i| match i {
                    Arm64Instruction::MemLoad { width, acquire, .. } => Some((*width, *acquire)),
                    _ => None,
                })
                .expect("a MemLoad reads the static");
            assert_eq!(load, (Arm64MemWidth::W32, is_volatile));
            let words = words_of(&emit_machine_code(&result).expect("encodes"));
            let seq_ldr = [0xF940_0129u32, 0x9100_9129]; // LDR X9,[X9]; ADD X9,X9,#36
            let value_load = if is_volatile {
                0x88DF_FD29 // LDAR W9, [X9]
            } else {
                0xB940_0129 // LDR  W9, [X9]
            };
            let want = [seq_ldr[0], seq_ldr[1], value_load, 0x9340_7D29]; // .., SXTW X9, W9
            assert!(
                words.windows(4).any(|w| w == want),
                "volatile={is_volatile}: expected {want:08x?} in {words:08x?}"
            );
            // A volatile READ owes no barrier: no DMB anywhere in the body.
            assert!(
                !words.contains(&0xD503_3BBF),
                "volatile={is_volatile}: a read must not emit DMB ISH"
            );
        }
    }

    /// `long` reads the 64-bit payload (offset 8) with a 64-bit `LDAR` when
    /// `volatile` (JLS §17.7 single-copy atomicity); `float`/`double` land in
    /// an FP register through a bit move.
    #[test]
    fn r9w10_getstatic_long_float_double_widths() {
        let long = compile_with_statics(
            "()J",
            &[0xb2, 0x00, 0x01, 0xad],
            &[(0, static_field(3, 0, b'J', true))],
        );
        assert!(long.success);
        let words = words_of(&emit_machine_code(&long).expect("encodes"));
        // ADD X9, X9, #8; LDAR X9, [X9]
        assert!(
            words.windows(2).any(|w| w == [0x9100_2129u32, 0xC8DF_FD29]),
            "{words:08x?}"
        );

        for (desc, ret, tag, width) in [
            ("()F", 0xaeu8, b'F', Arm64MemWidth::W32),
            ("()D", 0xaf, b'D', Arm64MemWidth::X64),
        ] {
            let r = compile_with_statics(
                desc,
                &[0xb2, 0x00, 0x01, ret],
                &[(0, static_field(3, 1, tag, false))],
            );
            assert!(r.success, "{desc} must compile");
            let at = r
                .instructions
                .iter()
                .position(|i| matches!(i, Arm64Instruction::MemLoad { width: w, acquire: false, .. } if *w == width))
                .expect("a plain MemLoad of the payload width");
            let moved = match (tag, r.instructions.get(at + 1)) {
                (b'F', Some(Arm64Instruction::FmovToFpSingle { .. })) => true,
                (b'D', Some(Arm64Instruction::FmovToFp { .. })) => true,
                _ => false,
            };
            assert!(
                moved,
                "{desc}: the payload is bit-moved into an FP register"
            );
        }
    }

    /// What `getstatic` still refuses: no resolution for the site, an
    /// unrecognised type tag, a zero base cell. And the refusal is by the
    /// arm, not by a silently-compiled read.
    ///
    /// h23 (2026-09-22): a REFERENCE static is no longer among them -- see
    /// `h23_getstatic_lowers_a_reference_static_as_an_oop` just below. The
    /// unresolved and mis-keyed cases still refuse a reference site, which is
    /// what the first and last assertions here now pin.
    #[test]
    fn r9w10_getstatic_refuses_unresolved_reference_and_unbased_sites() {
        let code = [0xb2u8, 0x00, 0x01, 0xb0];
        let unresolved = compile_with_statics("()Ljava/lang/Object;", &code, &[]);
        assert!(!unresolved.success);
        // A tag no descriptor produces: still refused, by the same arm that
        // used to refuse `L`.
        let unknown = compile_with_statics(
            "()Ljava/lang/Object;",
            &code,
            &[(0, static_field(1, 0, b'Q', false))],
        );
        assert!(!unknown.success, "an unrecognised type tag is not lowered");
        let mut unbased = static_field(1, 0, b'I', false);
        unbased.base_cell = 0;
        let r = compile_with_statics("()I", &[0xb2, 0x00, 0x01, 0xac], &[(0, unbased)]);
        assert!(!r.success, "a zero base cell is not an address");
        // Resolved at the WRONG pc: still refused.
        let r = compile_with_statics(
            "()I",
            &[0x00, 0xb2, 0x00, 0x01, 0xac],
            &[(0, static_field(1, 0, b'I', false))],
        );
        assert!(!r.success, "the table is keyed by the getstatic's own pc");
    }

    /// h23 (2026-09-22): a REFERENCE static reads the same 64-bit payload
    /// word `J`/`D` read and lands on the operand stack as an oop.
    ///
    /// The three things that make it correct, each pinned separately:
    /// the WIDTH (a reference is a whole word, not the 32-bit payload the int
    /// category reads), the ORDERING (`LDAR` when volatile, a plain `LDR`
    /// otherwise -- a reference static is as volatile as an `int` one), and
    /// the OOP MARK, which is what a safepoint's map is built from and the
    /// only reason this was refused for thirteen waves.
    #[test]
    fn h23_getstatic_lowers_a_reference_static_as_an_oop() {
        for (tag, desc) in [(b'L', "()Ljava/lang/Object;"), (b'[', "()[I")] {
            for is_volatile in [false, true] {
                let r = compile_with_statics(
                    desc,
                    &[0xb2, 0x00, 0x01, 0xb0],
                    &[(0, static_field(3, 0, tag, is_volatile))],
                );
                let what = format!("tag={} volatile={is_volatile}", char::from(tag));
                assert!(r.success, "{what}: a reference static must compile");
                // A whole word, from the 64-bit payload, with acquire exactly
                // when the field is volatile.
                assert!(
                    r.instructions.iter().any(|i| matches!(
                        i,
                        Arm64Instruction::MemLoad {
                            width: Arm64MemWidth::X64,
                            acquire,
                            ..
                        } if *acquire == is_volatile
                    )),
                    "{what}: a 64-bit load whose ordering matches the field"
                );
                // Never the int category's 32-bit read, which would truncate
                // the upper half of a pointer.
                assert!(
                    !r.instructions.iter().any(|i| matches!(
                        i,
                        Arm64Instruction::MemLoad {
                            width: Arm64MemWidth::W32,
                            ..
                        }
                    )),
                    "{what}: a reference is never read 32 bits wide"
                );
                // A read owes no barrier, exactly as the primitive arms.
                assert!(
                    !r.instructions
                        .iter()
                        .any(|i| matches!(i, Arm64Instruction::DmbIsh)),
                    "{what}: a volatile READ emits no DMB"
                );
            }
        }

        // The mark itself, read off the model rather than inferred from the
        // instruction stream: `areturn` consumes it, so the check is made on
        // a body that leaves the value on the stack.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_method_descriptor("()Ljava/lang/Object;", true);
        b.set_static_field_info([(0usize, static_field(3, 0, b'L', false))].into_iter().collect());
        b.cur_bytecode_pc = 0;
        assert!(
            b.emit_getstatic(static_field(3, 0, b'L', false)),
            "the arm lowers a reference static"
        );
        let top = b.operand_stack.last().expect("one entry pushed");
        assert_eq!(top.kind, OperandKind::Ref, "pushed as a reference");
        assert!(top.oop, "and marked for the collector");
    }

    /// Two reads of statics of one class record the class once; a read after
    /// a merge point is described by the analysis the table feeds.
    #[test]
    fn r9w10_getstatic_records_declaring_classes_once() {
        // getstatic #1; getstatic #2; iadd; ireturn
        let code = [0xb2u8, 0x00, 0x01, 0xb2, 0x00, 0x02, 0x60, 0xac];
        let r = compile_with_statics(
            "()I",
            &code,
            &[
                (0, static_field(9, 0, b'I', false)),
                (3, static_field(9, 1, b'S', true)),
            ],
        );
        assert!(r.success);
        assert_eq!(r.static_init_classes, vec![9]);
    }

    /// The site walk decodes instructions, so a `0xb2` operand byte is not
    /// mistaken for a `getstatic`.
    #[test]
    fn r9w10_static_field_sites_walk_instructions_not_bytes() {
        // bipush 0xb2; getstatic #5; ireturn
        let code = [0x10u8, 0xb2, 0xb2, 0x00, 0x05, 0xac];
        assert_eq!(static_field_sites(&code), vec![(2, 0xb2, 5)]);
    }

    // -- Round 9 wave 11: the `ArithmeticException` throw path -------------

    const R9W11_THROW_ARITH: usize = 0x7FFF_0000_2000;

    /// Compile a static method with the division throw path wired as the
    /// caller would wire it: `helper` as `throw_arithmetic`, and the caller's
    /// word on whether the exception table is empty.
    fn compile_div(
        descriptor: &str,
        slots: usize,
        code: &[u8],
        helper: usize,
        table_empty: bool,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.throw_arithmetic = helper;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(table_empty);
        b.compile_method(slots, slots, 4, code)
    }

    /// The two new W-form pseudo-ops and the X forms they sit beside encode
    /// to the ARM ARM words.
    #[test]
    fn r9w11_sdiv_msub_w_and_x_forms_encode() {
        let (d, n, m) = (Arm64Register::X11, Arm64Register::X9, Arm64Register::X10);
        let insts = vec![
            Arm64Instruction::SDivW {
                rd: d,
                rn: n,
                rm: m,
            },
            Arm64Instruction::MsubW {
                rd: d,
                rn: d,
                rm: m,
                ra: n,
            },
            Arm64Instruction::SDiv {
                rd: d,
                rn: n,
                rm: m,
            },
            Arm64Instruction::Msub {
                rd: d,
                rn: d,
                rm: m,
                ra: n,
            },
        ];
        let bytes = emit_machine_code(&result_from_instructions(insts)).expect("encodes");
        assert_eq!(
            words_of(&bytes),
            vec![
                0x1ACA_0D2Bu32, // SDIV W11, W9, W10
                0x1B0A_A56B,    // MSUB W11, W11, W10, W9
                0x9ACA_0D2B,    // SDIV X11, X9, X10
                0x9B0A_A56B,    // MSUB X11, X11, X10, X9
            ]
        );
    }

    /// With the throw path wired, `idiv`/`irem`/`ldiv`/`lrem` compile to a
    /// zero test of the DIVISOR that branches to one out-of-line stub, which
    /// calls `throw_arithmetic` through X16 and leaves through the shared
    /// epilogue; then `SDIV` (and `MSUB` for a remainder), sign-extended back
    /// to the int form for the `int` pair only. The `CBZ`'s encoded target is
    /// checked against where the stub really landed.
    #[test]
    fn r9w11_div_rem_guard_the_divisor_and_branch_to_the_throw_stub() {
        let cases: [(u8, &str, usize, [u8; 4], bool, bool); 4] = [
            // iload_0; iload_1; idiv|irem; ireturn
            (0x6c, "(II)I", 2, [0x1a, 0x1b, 0x6c, 0xac], false, false),
            (0x70, "(II)I", 2, [0x1a, 0x1b, 0x70, 0xac], false, true),
            // lload_0; lload_2; ldiv|lrem; lreturn
            (0x6d, "(JJ)J", 4, [0x1e, 0x20, 0x6d, 0xad], true, false),
            (0x71, "(JJ)J", 4, [0x1e, 0x20, 0x71, 0xad], true, true),
        ];
        for (op, desc, slots, code, wide, rem) in cases {
            let r = compile_div(desc, slots, &code, R9W11_THROW_ARITH, true);
            assert!(
                r.success,
                "0x{op:02x}: must compile with the throw path wired"
            );
            let ops = &r.instructions;

            // The guard: a CBZ (X for long, W for int) on the divisor.
            let (guard_at, guard_rt, throw_label) = ops
                .iter()
                .enumerate()
                .find_map(|(i, inst)| match (wide, inst) {
                    (true, Arm64Instruction::Cbz { rt, label }) => Some((i, *rt, *label)),
                    (false, Arm64Instruction::CbzW { rt, label }) => Some((i, *rt, *label)),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("0x{op:02x}: no zero-divisor guard"));

            // The division reads the guarded register as its divisor.
            let (div_at, dst, dividend) = ops
                .iter()
                .enumerate()
                .find_map(|(i, inst)| match (wide, inst) {
                    (true, Arm64Instruction::SDiv { rd, rn, rm }) if *rm == guard_rt => {
                        Some((i, *rd, *rn))
                    }
                    (false, Arm64Instruction::SDivW { rd, rn, rm }) if *rm == guard_rt => {
                        Some((i, *rd, *rn))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("0x{op:02x}: no SDIV by the guarded divisor"));
            assert!(
                guard_at < div_at,
                "0x{op:02x}: the guard runs before the divide"
            );
            assert_ne!(
                dst, guard_rt,
                "0x{op:02x}: the result must not clobber the divisor"
            );
            assert_ne!(
                dst, dividend,
                "0x{op:02x}: the result must not clobber the dividend"
            );
            let mut next = div_at + 1;
            if rem {
                let msub_ok = match (wide, &ops[next]) {
                    (true, Arm64Instruction::Msub { rd, rn, rm, ra })
                    | (false, Arm64Instruction::MsubW { rd, rn, rm, ra }) => {
                        *rd == dst && *rn == dst && *rm == guard_rt && *ra == dividend
                    }
                    _ => false,
                };
                assert!(msub_ok, "0x{op:02x}: remainder is MSUB dst, dst, b, a");
                next += 1;
            }
            let sxtw_follows = matches!(
                &ops[next],
                Arm64Instruction::Sxtw { rd, rn } if *rd == dst && *rn == dst
            );
            assert_eq!(
                sxtw_follows, !wide,
                "0x{op:02x}: SXTW follows the int forms only"
            );
            assert!(
                !ops.iter()
                    .any(|i| matches!(i, Arm64Instruction::Brk { .. })),
                "0x{op:02x}: the SIGTRAP guard must never come back"
            );

            // The stub: after the epilogue's RET, bound to the guard's label.
            let ret_at = ops
                .iter()
                .rposition(|i| matches!(i, Arm64Instruction::Ret))
                .expect("an epilogue RET");
            let stub_at = ops
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == throw_label))
                .expect("the guard's label is bound");
            assert!(stub_at > ret_at, "0x{op:02x}: the stub is out of line");
            assert!(matches!(
                &ops[stub_at + 1],
                Arm64Instruction::MovImm { rd: Arm64Register::X16, imm }
                    if *imm == R9W11_THROW_ARITH as i64
            ));
            assert!(matches!(
                &ops[stub_at + 2],
                Arm64Instruction::Blr {
                    rn: Arm64Register::X16
                }
            ));
            // ...and leaves through the SAME epilogue the `*return` uses.
            let ret_label = ops[..ret_at]
                .iter()
                .find_map(|i| match i {
                    Arm64Instruction::B { label } => Some(*label),
                    _ => None,
                })
                .expect("the return branches to the epilogue");
            assert!(
                matches!(&ops[stub_at + 3], Arm64Instruction::B { label } if *label == ret_label),
                "0x{op:02x}: the stub must run the epilogue, not RET past it"
            );

            // Encoded: the CBZ's displacement lands exactly on the stub.
            let (bytes, offsets) = emit_machine_code_inner(&r).expect("encodes");
            let word_at = |off: usize| {
                u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
            };
            let cbz = word_at(offsets[guard_at]);
            let want_base = if wide { 0xB400_0000u32 } else { 0x3400_0000 };
            assert_eq!(cbz & 0xFF00_001F, want_base | u32::from(guard_rt.0));
            // Cast: sign-extend the 19-bit word displacement.
            let imm19 = ((cbz >> 5) & 0x7FFFF) as i32;
            let disp = ((imm19 << 13) >> 13) * 4;
            assert_eq!(
                offsets[guard_at] as i64 + i64::from(disp),
                offsets[stub_at] as i64,
                "0x{op:02x}: CBZ must branch to the stub"
            );
            assert!(words_of(&bytes).contains(&0xD63F_0200), "BLR X16");
        }
    }

    /// Every division of a method shares ONE stub; a method with no division
    /// emits none.
    #[test]
    fn r9w11_divisions_share_one_throw_stub_and_none_is_emitted_without_one() {
        // iload_0; iload_1; idiv; iload_1; irem; ireturn
        let r = compile_div(
            "(II)I",
            2,
            &[0x1a, 0x1b, 0x6c, 0x1b, 0x70, 0xac],
            R9W11_THROW_ARITH,
            true,
        );
        assert!(r.success);
        let labels: Vec<u32> = r
            .instructions
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::CbzW { label, .. } => Some(*label),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 2, "one guard per division");
        assert_eq!(labels[0], labels[1], "both guards share the stub");
        let blrs = r
            .instructions
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .count();
        assert_eq!(blrs, 1, "one stub, one call");

        // iload_0; ireturn -- wired, but nothing to guard.
        let plain = compile_div("(I)I", 1, &[0x1a, 0xac], R9W11_THROW_ARITH, true);
        assert!(plain.success);
        assert!(!plain
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Blr { .. })));
    }

    /// The throw path is exact only with BOTH halves wired. A method whose
    /// exception table is not known empty could have a handler for the
    /// exception, which the stub's unknown throw pc cannot select correctly;
    /// a missing helper is no throw path at all. Either refuses, by the arm,
    /// before any division is emitted.
    #[test]
    fn r9w11_div_refuses_without_the_helper_or_the_empty_table_word() {
        let code = [0x1au8, 0x1b, 0x6c, 0xac];
        for (helper, table_empty, why) in [
            (R9W11_THROW_ARITH, false, "exception table not known empty"),
            (0, true, "no throw_arithmetic helper"),
            (0, false, "neither"),
        ] {
            let r = compile_div("(II)I", 2, &code, helper, table_empty);
            assert!(!r.success, "{why}: idiv must refuse");
            assert!(
                !r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::SDivW { .. } | Arm64Instruction::SDiv { .. }
                )),
                "{why}: no division may be emitted"
            );
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Comment(c) if c.contains("no exact ArithmeticException path")
                )),
                "{why}: refused by the division arm"
            );
        }
    }

    // -- Round 9 wave 12: the NPE throw path and `arraylength` -------------

    const R9W12_NPE: usize = 0x7FFF_0000_3000;

    /// Compile a static method with the NPE throw path wired as the caller
    /// would wire it.
    fn compile_npe(
        descriptor: &str,
        slots: usize,
        code: &[u8],
        helper: usize,
        table_empty: bool,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.jit_npe_with_action = helper;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(table_empty);
        b.compile_method(slots, slots, 4, code)
    }

    /// `arraylength` null-checks its receiver against the per-action stub and
    /// then reads the header's length word with a PLAIN load at
    /// `ARRAY_LENGTH_OFFSET`, canonicalized by `SXTW`.
    ///
    /// The plain load is the claim worth pinning: the length is written once,
    /// before publication, and the load's address depends on the reference, so
    /// ARMv8 orders it without `LDAR` and without a fence. If someone later
    /// "fixes" this to an acquire load, the encoding assertion below is what
    /// says the argument was reconsidered rather than forgotten.
    #[test]
    fn r9w12_arraylength_null_checks_then_loads_the_length_word() {
        // aload_0; arraylength; ireturn
        let r = compile_npe("([I)I", 1, &[0x2a, 0xbe, 0xac], R9W12_NPE, true);
        assert!(r.success, "a wired arraylength compiles");

        let at = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Cbz { .. }))
            .expect("the receiver is null-checked with a 64-bit CBZ");
        let Arm64Instruction::Cbz { rt: recv, label } = r.instructions[at] else {
            unreachable!()
        };
        match (&r.instructions[at + 1], &r.instructions[at + 2]) {
            (
                Arm64Instruction::AddImm { rd, rn, imm },
                Arm64Instruction::MemLoad {
                    rt,
                    rn: base,
                    width: Arm64MemWidth::W32,
                    acquire: false,
                },
            ) => {
                assert_eq!(*rn, recv, "the length address is the receiver plus a disp");
                assert_eq!(
                    *imm,
                    // Cast: a small header constant, as the lowering passes it.
                    cratonvm_types::ARRAY_LENGTH_OFFSET as i32,
                    "the disp is the header's length offset"
                );
                assert_eq!(*rd, *base);
                assert_eq!(*rt, *base, "the length lands where its address was");
                assert_ne!(
                    *rd, recv,
                    "the address must not clobber the checked receiver"
                );
            }
            (a, b) => panic!("expected ADD #len_off then a plain 32-bit MemLoad, got {a:?} {b:?}"),
        }
        assert!(
            matches!(r.instructions[at + 3], Arm64Instruction::Sxtw { .. }),
            "an int producer is a W form plus SXTW on this backend"
        );

        // The stub: one per action, out of line after the epilogue's RET, and
        // it materializes the deopt sentinel ITSELF (the helper returns `()`).
        let stub = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == label))
            .expect("the CBZ's label is bound");
        let ret = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Ret))
            .expect("the epilogue returns");
        assert!(stub > ret, "the stub is out of line, after the RET");
        match &r.instructions[stub + 1..stub + 6] {
            [Arm64Instruction::MovImm {
                rd: a0,
                imm: action,
            }, Arm64Instruction::MovImm {
                rd: scratch,
                imm: helper,
            }, Arm64Instruction::Blr { rn: called }, Arm64Instruction::MovImm {
                rd: sentinel_reg,
                imm: sentinel,
            }, Arm64Instruction::B { .. }] => {
                assert_eq!(*a0, Arm64Register::X0);
                assert_eq!(*action, i64::from(npe_action::ARRAY_LENGTH));
                // Cast: the helper address as the pseudo-op carries it.
                assert_eq!(*helper, R9W12_NPE as i64);
                assert_eq!(*called, *scratch);
                assert_eq!(*sentinel_reg, Arm64Register::X0);
                assert_eq!(
                    *sentinel,
                    i64::MIN,
                    "the stub owes the deopt sentinel: jit_npe_with_action returns ()"
                );
            }
            other => panic!("unexpected NPE stub shape: {other:?}"),
        }

        // No ordering instruction is emitted for this read.
        let words = words_of(&emit_machine_code(&r).expect("encodes"));
        assert!(
            !words.contains(&0xD503_3BBF),
            "an address-dependent length read owes no DMB ISH: {words:08x?}"
        );
        // The plain `LDR Wt, [Xn, #0]` is `0xB9400000 | ..`; `LDAR Wt, [Xn]`
        // would be `0x88DFFC00 | ..`. Check which one the encoder produced.
        assert!(
            words.iter().any(|w| w & 0xFFC0_0000 == 0xB940_0000),
            "the length load encodes as a plain LDR Wt, [Xn]: {words:08x?}"
        );
        assert!(
            !words.iter().any(|w| w & 0xFFFF_FC00 == 0x88DF_FC00),
            "no LDAR W is emitted for an array length: {words:08x?}"
        );
    }

    /// Two `arraylength`s in one method share ONE stub, and a method with no
    /// null check emits none.
    #[test]
    fn r9w12_npe_stubs_are_one_per_action_and_none_without_one() {
        // aload_0; arraylength; aload_0; arraylength; iadd; ireturn
        let two = compile_npe(
            "([I)I",
            1,
            &[0x2a, 0xbe, 0x2a, 0xbe, 0x60, 0xac],
            R9W12_NPE,
            true,
        );
        assert!(two.success);
        let labels: Vec<u32> = two
            .instructions
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::Cbz { label, .. } => Some(*label),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 2, "each receiver is checked");
        assert_eq!(labels[0], labels[1], "both share the ARRAY_LENGTH stub");
        let calls = two
            .instructions
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .count();
        assert_eq!(calls, 1, "one stub, one call");

        // aload_0; areturn -- wired, but nothing to check.
        let plain = compile_npe("([I)[I", 1, &[0x2a, 0xb0], R9W12_NPE, true);
        assert!(plain.success);
        assert!(
            !plain
                .instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Blr { .. })),
            "no null check, no stub"
        );
    }

    /// The encoded `CBZ` displacement really lands on the stub, not merely on
    /// a label the pseudo-op stream happens to contain.
    #[test]
    fn r9w12_the_encoded_null_check_branches_to_the_stub() {
        let r = compile_npe("([I)I", 1, &[0x2a, 0xbe, 0xac], R9W12_NPE, true);
        assert!(r.success);
        let words = words_of(&emit_machine_code(&r).expect("encodes"));
        // CBZ Xt, label: 0xB4000000 | (imm19 << 5) | Rt.
        let (at, word) = words
            .iter()
            .enumerate()
            .find(|(_, w)| *w & 0xFF00_0000 == 0xB400_0000)
            .map(|(i, w)| (i, *w))
            .expect("a 64-bit CBZ is encoded");
        // Sign-extend the 19-bit displacement, which counts INSTRUCTIONS.
        // Cast: a 19-bit field, then a small signed instruction count.
        let imm19 = ((word >> 5) & 0x7_FFFF) as i32;
        let disp = if imm19 & 0x4_0000 != 0 {
            imm19 - 0x8_0000
        } else {
            imm19
        };
        // Cast: an instruction index in a method of a handful of words.
        let target = at as i32 + disp;
        assert!(target > 0, "the branch goes forward, to the stub");
        // Cast: checked positive above.
        let target = target as usize;
        // The stub's first word is `MOVZ X0, #action`.
        assert_eq!(
            words[target],
            0xD280_0000 | (u32::from(npe_action::ARRAY_LENGTH) << 5),
            "the CBZ must land on MOVZ X0, #ARRAY_LENGTH: {words:08x?} target={target}"
        );
    }

    /// The NPE path is exact only with BOTH halves wired, exactly as the
    /// `ArithmeticException` path is: a method whose exception table is not
    /// known empty could catch the NPE, and the drain's unknown throw pc
    /// cannot pick the handler. Either missing refuses by the arm, with
    /// nothing emitted.
    #[test]
    fn r9w12_arraylength_refuses_without_the_helper_or_the_empty_table_word() {
        for (helper, table_empty, why) in [
            (R9W12_NPE, false, "exception table not known empty"),
            (0, true, "no jit_npe_with_action helper"),
            (0, false, "neither"),
        ] {
            let r = compile_npe("([I)I", 1, &[0x2a, 0xbe, 0xac], helper, table_empty);
            assert!(!r.success, "{why}: arraylength must refuse");
            assert!(
                !r.instructions
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::MemLoad { .. })),
                "{why}: no length load may be emitted"
            );
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Comment(c)
                        if c.contains("no exact NullPointerException path")
                )),
                "{why}: refused by the arraylength arm"
            );
        }
    }

    // -- Round 9 wave 13: array elements, bounds checks and the AIOOBE path -

    const R9W13_AIOOBE: usize = 0x7FFF_0000_4000;

    /// Compile a static method with BOTH array throw paths wired as the caller
    /// would wire them.
    fn compile_array(
        descriptor: &str,
        slots: usize,
        code: &[u8],
        npe: usize,
        aioobe: usize,
        table_empty: bool,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.jit_npe_with_action = npe;
        h.throw_aioobe = aioobe;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(table_empty);
        b.compile_method(slots, slots, 4, code)
    }

    /// `ADD Xd, Xn, Xm, LSL #amount` encodes to the ARM ARM word for every
    /// element scale a Java array has, and a scale the table cannot produce
    /// refuses the method rather than truncating into the imm6.
    #[test]
    fn r9w13_add_lsl_encodes_every_array_scale() {
        let (rd, rn, rm) = (Arm64Register::X11, Arm64Register::X9, Arm64Register::X10);
        let insts: Vec<Arm64Instruction> = (0u8..=3)
            .map(|shift| Arm64Instruction::AddLsl { rd, rn, rm, shift })
            .collect();
        let bytes = emit_machine_code(&result_from_instructions(insts)).expect("encodes");
        assert_eq!(
            words_of(&bytes),
            vec![
                0x8B0A_012Bu32, // ADD X11, X9, X10           (byte[]/boolean[])
                0x8B0A_052B,    // ADD X11, X9, X10, LSL #1   (char[]/short[])
                0x8B0A_092B,    // ADD X11, X9, X10, LSL #2   (int[]/float[])
                0x8B0A_0D2B,    // ADD X11, X9, X10, LSL #3   (long[]/double[])
            ]
        );
        let bad = result_from_instructions(vec![Arm64Instruction::AddLsl {
            rd,
            rn,
            rm,
            shift: 4,
        }]);
        assert!(
            emit_machine_code(&bad).is_none(),
            "a scale outside 0..=3 is a lowering bug, not a wide immediate"
        );
    }

    /// Every primitive array LOAD: the guards, the scaled address, the
    /// element-width load and the JVMS narrowing.
    #[test]
    fn r9w13_array_loads_guard_scale_and_narrow() {
        for (opcode, descriptor, ret, width, shift, action, extend) in [
            (
                0x2eu8,
                "([II)I",
                0xacu8,
                Arm64MemWidth::W32,
                2u8,
                npe_action::ALOAD_INT,
                None,
            ),
            (
                0x2f,
                "([JI)J",
                0xad,
                Arm64MemWidth::X64,
                3,
                npe_action::ALOAD_LONG,
                None,
            ),
            (
                0x33,
                "([BI)I",
                0xac,
                Arm64MemWidth::B8,
                0,
                npe_action::ALOAD_BYTE,
                Some("sxtb"),
            ),
            (
                0x34,
                "([CI)I",
                0xac,
                Arm64MemWidth::H16,
                1,
                npe_action::ALOAD_CHAR,
                None,
            ),
            (
                0x35,
                "([SI)I",
                0xac,
                Arm64MemWidth::H16,
                1,
                npe_action::ALOAD_SHORT,
                Some("sxth"),
            ),
        ] {
            // aload_0; iload_1; <load>; <return>
            let r = compile_array(
                descriptor,
                2,
                &[0x2a, 0x1b, opcode, ret],
                R9W12_NPE,
                R9W13_AIOOBE,
                true,
            );
            assert!(r.success, "0x{opcode:02x} must compile");

            let at = r
                .instructions
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Cbz { .. }))
                .expect("the receiver is null-checked");
            let Arm64Instruction::Cbz {
                rt: arr,
                label: npe,
            } = r.instructions[at]
            else {
                unreachable!()
            };
            // The NPE stub names this opcode's own action.
            let stub = r
                .instructions
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == npe))
                .expect("the NPE label is bound");
            assert!(
                matches!(
                    r.instructions[stub + 1],
                    Arm64Instruction::MovImm { imm, .. } if imm == i64::from(action)
                ),
                "0x{opcode:02x}: the null check reports its own JEP-358 action"
            );

            // The length load, then an UNSIGNED compare and a B.HS.
            let rest = &r.instructions[at + 1..];
            let len_at = rest
                .iter()
                .position(|i| matches!(i, Arm64Instruction::CmpW { .. }))
                .expect("the index is compared against the length");
            match (&rest[len_at - 2], &rest[len_at - 1], &rest[len_at]) {
                (
                    Arm64Instruction::AddImm { rd, rn, imm },
                    Arm64Instruction::MemLoad {
                        rt,
                        width: Arm64MemWidth::W32,
                        acquire: false,
                        ..
                    },
                    Arm64Instruction::CmpW { rm, .. },
                ) => {
                    assert_eq!(*rn, arr, "the length is read off the array");
                    // Cast: a small header constant, as the lowering passes it.
                    assert_eq!(*imm, cratonvm_types::ARRAY_LENGTH_OFFSET as i32);
                    assert_eq!(*rd, *rt);
                    assert_eq!(*rm, *rt, "the compare's rhs IS the loaded length");
                }
                other => panic!("0x{opcode:02x}: unexpected bounds-check shape: {other:?}"),
            }
            assert!(
                matches!(
                    rest[len_at + 1],
                    Arm64Instruction::BCond {
                        cond: Arm64Condition::Cs,
                        ..
                    }
                ),
                "0x{opcode:02x}: the bounds branch is the UNSIGNED >= that also catches idx < 0"
            );

            // The scaled address, then the element load at its own width.
            let scaled = match shift {
                0 => matches!(rest[len_at + 2], Arm64Instruction::Add { .. }),
                s => {
                    matches!(rest[len_at + 2], Arm64Instruction::AddLsl { shift, .. } if shift == s)
                }
            };
            assert!(
                scaled,
                "0x{opcode:02x}: the index is scaled by the element size ({shift}): {:?}",
                rest[len_at + 2]
            );
            assert!(
                matches!(
                    rest[len_at + 3],
                    // Cast: a small header constant, as the lowering passes it.
                    Arm64Instruction::AddImm { imm, .. } if imm == cratonvm_types::ARRAY_DATA_OFFSET as i32
                ),
                "0x{opcode:02x}: the data area offset is added"
            );
            assert!(
                matches!(
                    rest[len_at + 4],
                    Arm64Instruction::MemLoad { width: w, acquire: false, .. } if w == width
                ),
                "0x{opcode:02x}: a plain load of the element's own width: {:?}",
                rest[len_at + 4]
            );
            let narrowing = match extend {
                Some("sxtb") => matches!(rest[len_at + 5], Arm64Instruction::Sxtb { .. }),
                Some("sxth") => matches!(rest[len_at + 5], Arm64Instruction::Sxth { .. }),
                _ => true,
            };
            assert!(
                narrowing,
                "0x{opcode:02x}: expected {extend:?} after the load, got {:?}",
                rest[len_at + 5]
            );
        }
    }

    /// `faload`/`daload` land the raw bits in an FP register;
    /// `fastore`/`dastore` take them back out.
    #[test]
    fn r9w13_fp_array_elements_move_through_a_bit_exact_fmov() {
        let f = compile_array(
            "([FI)F",
            2,
            &[0x2a, 0x1b, 0x30, 0xae],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(f.success);
        assert!(f
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovToFpSingle { .. })));
        let d = compile_array(
            "([DI)D",
            2,
            &[0x2a, 0x1b, 0x31, 0xaf],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(d.success);
        assert!(d
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovToFp { .. })));

        // aload_0; iload_1; fload_2 (0x24 -- 0x22 is fload_0); fastore; return
        let fs = compile_array(
            "([FIF)V",
            3,
            &[0x2a, 0x1b, 0x24, 0x51, 0xb1],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(fs.success);
        assert!(fs
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovFromFpSingle { .. })));
        // aload_0; iload_1; dload_2 (0x28 -- 0x26 is dload_0); dastore; return
        let ds = compile_array(
            "([DID)V",
            4,
            &[0x2a, 0x1b, 0x28, 0x52, 0xb1],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(ds.success);
        assert!(ds
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovFromFp { .. })));
    }

    /// Every primitive array STORE stores at the element's own width -- which
    /// IS the JVMS narrowing for `castore`/`sastore`/`bastore` -- and only
    /// `bastore` additionally masks, behind a runtime `boolean[]` test.
    #[test]
    fn r9w13_array_stores_truncate_and_only_bastore_masks() {
        for (opcode, descriptor, slots, value_op, width, masks) in [
            (0x4fu8, "([III)V", 3usize, 0x1cu8, Arm64MemWidth::W32, false),
            (0x50, "([JIJ)V", 4, 0x20, Arm64MemWidth::X64, false),
            (0x54, "([BII)V", 3, 0x1c, Arm64MemWidth::B8, true),
            (0x55, "([CII)V", 3, 0x1c, Arm64MemWidth::H16, false),
            (0x56, "([SII)V", 3, 0x1c, Arm64MemWidth::H16, false),
        ] {
            // aload_0; iload_1; <value load>; <store>; return
            let r = compile_array(
                descriptor,
                slots,
                &[0x2a, 0x1b, value_op, opcode, 0xb1],
                R9W12_NPE,
                R9W13_AIOOBE,
                true,
            );
            assert!(r.success, "0x{opcode:02x} must compile");
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MemStore { width: w, release: false, .. } if *w == width
                )),
                "0x{opcode:02x}: stores at the element's own width"
            );
            let masked = r
                .instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::AndImm { imm: 1, .. }));
            assert_eq!(
                masked, masks,
                "0x{opcode:02x}: only bastore narrows by `& 1`"
            );
            let probes = r.instructions.iter().any(
                |i| matches!(i, Arm64Instruction::Ldrb { offset, .. } if *offset == cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32),
            );
            assert_eq!(
                probes, masks,
                "0x{opcode:02x}: only bastore reads the header's element-type byte"
            );
        }
    }

    /// The `bastore` mask is guarded by a compare against the exact `[Z`
    /// kind/element byte, and is SKIPPED when the compare fails -- a `byte[]`
    /// keeps its low-byte truncation.
    #[test]
    fn r9w13_the_bastore_mask_tests_the_boolean_array_tag_and_skips_otherwise() {
        let tag = cratonvm_types::primitive_array_kind_tags_byte("[Z").expect("[Z has a tag");
        let r = compile_array(
            "([BII)V",
            3,
            &[0x2a, 0x1b, 0x1c, 0x54, 0xb1],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(r.success);
        let at = r
            .instructions
            .iter()
            .position(
                |i| matches!(i, Arm64Instruction::Ldrb { offset, .. } if *offset == cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32),
            )
            .expect("the element-type byte is read");
        match (&r.instructions[at + 1], &r.instructions[at + 2]) {
            (
                Arm64Instruction::CmpImmW { imm, .. },
                Arm64Instruction::BCond {
                    cond: Arm64Condition::Ne,
                    label,
                },
            ) => {
                assert_eq!(*imm, i32::from(tag), "compared against the `[Z` tag");
                // The AND is between the branch and the label it skips to.
                assert!(matches!(
                    r.instructions[at + 3],
                    Arm64Instruction::AndImm { imm: 1, .. }
                ));
                assert!(
                    matches!(r.instructions[at + 4], Arm64Instruction::Label(l) if l == *label),
                    "the B.NE skips exactly the AND"
                );
            }
            other => panic!("unexpected boolean-mask shape: {other:?}"),
        }
    }

    /// The AIOOBE stub is one per SITE, out of line, and marshals the index,
    /// the length, the array and the bci into the helper's four argument
    /// registers.
    #[test]
    fn r9w13_aioobe_stubs_are_per_site_and_marshal_four_arguments() {
        // aload_0; iload_1; iaload; aload_0; iload_1; iaload; iadd; ireturn
        let r = compile_array(
            "([II)I",
            2,
            &[0x2a, 0x1b, 0x2e, 0x2a, 0x1b, 0x2e, 0x60, 0xac],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(r.success);
        let bounds: Vec<(usize, u32)> = r
            .instructions
            .iter()
            .enumerate()
            .filter_map(|(i, inst)| match inst {
                Arm64Instruction::BCond {
                    cond: Arm64Condition::Cs,
                    label,
                } => Some((i, *label)),
                _ => None,
            })
            .collect();
        assert_eq!(bounds.len(), 2, "each access is bounds-checked");
        assert_ne!(
            bounds[0].1, bounds[1].1,
            "two sites cannot share a stub: the bci and the registers differ"
        );

        let ret = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Ret))
            .expect("the epilogue returns");
        let mut seen_bcis = Vec::new();
        for (_, label) in bounds {
            let stub = r
                .instructions
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == label))
                .expect("the stub label is bound");
            assert!(stub > ret, "the stub is out of line, after the RET");
            match &r.instructions[stub + 1..stub + 8] {
                [Arm64Instruction::Mov { rd: a0, .. }, Arm64Instruction::Mov { rd: a1, .. }, Arm64Instruction::Mov { rd: a2, .. }, Arm64Instruction::MovImm { rd: a3, imm: bci }, Arm64Instruction::MovImm {
                    rd: scratch,
                    imm: helper,
                }, Arm64Instruction::Blr { rn: called }, Arm64Instruction::B { .. }] => {
                    assert_eq!(
                        [*a0, *a1, *a2, *a3],
                        [
                            Arm64Register::X0,
                            Arm64Register::X1,
                            Arm64Register::X2,
                            Arm64Register::X3
                        ],
                        "index, length, array, bci"
                    );
                    // Cast: the helper address as the pseudo-op carries it.
                    assert_eq!(*helper, R9W13_AIOOBE as i64);
                    assert_eq!(*called, *scratch);
                    seen_bcis.push(*bci);
                }
                other => panic!("unexpected AIOOBE stub shape: {other:?}"),
            }
        }
        assert_eq!(
            seen_bcis,
            vec![2, 5],
            "each stub carries its OWN site's bci"
        );
    }

    /// The encoded bounds branch really lands on its stub.
    #[test]
    fn r9w13_the_encoded_bounds_branch_reaches_the_stub() {
        let r = compile_array(
            "([II)I",
            2,
            &[0x2a, 0x1b, 0x2e, 0xac],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(r.success);
        let words = words_of(&emit_machine_code(&r).expect("encodes"));
        // B.cond: 0x54000000 | (imm19 << 5) | cond; HS/CS is cond 0b0010.
        let (at, word) = words
            .iter()
            .enumerate()
            .find(|(_, w)| *w & 0xFF00_001F == 0x5400_0002)
            .map(|(i, w)| (i, *w))
            .expect("a B.HS is encoded");
        // Cast: a 19-bit field, then a small signed instruction count.
        let imm19 = ((word >> 5) & 0x7_FFFF) as i32;
        let disp = if imm19 & 0x4_0000 != 0 {
            imm19 - 0x8_0000
        } else {
            imm19
        };
        // Cast: an instruction index in a method of a handful of words.
        let target = at as i32 + disp;
        assert!(target > 0, "the branch goes forward, to the stub");
        // Cast: checked positive above.
        let target = target as usize;
        // The stub's first word is `MOV X0, Xidx`, i.e. `ORR X0, XZR, Xidx`:
        // 0xAA0003E0 | (Rm << 16).
        assert_eq!(
            words[target] & 0xFFE0_FFFF,
            0xAA00_03E0,
            "the B.HS must land on MOV X0, Xindex: {words:08x?} target={target}"
        );
    }

    /// A reference element is not lowered, by either opcode, and the refusal
    /// is by the array arm rather than by the shared-memory gate.
    #[test]
    fn r9w13_aaload_and_aastore_still_refuse() {
        // aload_0; iload_1; aaload; areturn
        let load = compile_array(
            "([Ljava/lang/Object;I)Ljava/lang/Object;",
            2,
            &[0x2a, 0x1b, 0x32, 0xb0],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(!load.success, "aaload has no aarch64 lowering");
        // aload_0; iload_1; aload_2; aastore; return
        let store = compile_array(
            "([Ljava/lang/Object;ILjava/lang/Object;)V",
            3,
            &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(
            !store.success,
            "aastore owes an SATB barrier and a store check"
        );
        for r in [load, store] {
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Comment(c) if c.contains("a reference element")
                )),
                "refused by the array arm, naming the reason"
            );
        }
    }

    /// An array access needs BOTH throw paths; either missing refuses with
    /// nothing emitted.
    #[test]
    fn r9w13_array_access_refuses_without_both_throw_paths() {
        for (npe, aioobe, table_empty, why) in [
            (
                R9W12_NPE,
                R9W13_AIOOBE,
                false,
                "exception table not known empty",
            ),
            (0, R9W13_AIOOBE, true, "no NPE helper"),
            (R9W12_NPE, 0, true, "no AIOOBE helper"),
            (0, 0, true, "neither helper"),
        ] {
            let r = compile_array(
                "([II)I",
                2,
                &[0x2a, 0x1b, 0x2e, 0xac],
                npe,
                aioobe,
                table_empty,
            );
            assert!(!r.success, "{why}: iaload must refuse");
            assert!(
                !r.instructions
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::MemLoad { .. })),
                "{why}: no element load may be emitted"
            );
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Comment(c) if c.contains("no exact array exception path")
                )),
                "{why}: refused by the array arm"
            );
        }
    }

    #[test]
    fn backend_return_void_emits_branch_to_epilogue() {
        let result = make_backend_with_method(0, 0, &[0xb1]);
        let has_b = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::B { .. }));
        assert!(has_b, "return void should emit B to epilogue");
    }

    #[test]
    fn compile_result_success_flag() {
        let good = make_backend_with_method(0, 0, &[0xb1]); // return void
        assert!(good.success, "simple void return should succeed");

        let bad = make_backend_with_method(0, 0, &[0xFF]); // invalid opcode
        assert!(!bad.success, "unsupported opcode should set success=false");
    }

    #[test]
    fn arm64_stack_alignment_is_16() {
        assert_eq!(Arm64EntryConvention::STACK_ALIGNMENT, 16);
    }

    // -- Additional integration-level tests ---------------------------------

    #[test]
    fn compile_simple_add_method() {
        // int add(int a, int b) { return a + b; }
        // Bytecode: iload_0, iload_1, iadd, ireturn
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
        assert!(result.success);
        let has_add = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::AddW { .. }));
        assert!(has_add);
    }

    #[test]
    fn compile_bipush() {
        // bipush 42, ireturn
        let result = make_backend_with_method(0, 0, &[0x10, 42, 0xac]);
        assert!(result.success);
        let has_42 = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 42, .. }));
        assert!(has_42);
    }

    // -- Float/double operation tests ------------------------------------------

    /// `float` arithmetic lowers to the S forms and `double` to the D forms.
    ///
    /// Both used to lower to the D forms: `float` was modelled as `double`
    /// end to end, so rounding, overflow and the bits handed back to the VM
    /// were a double's.
    #[test]
    fn float_and_double_arithmetic_use_their_own_widths() {
        fn single(op: u8) -> Arm64CompileResult {
            make_backend_with_method(0, 0, &[0x0c, 0x0c, op, 0xb1])
        }
        fn double(op: u8) -> Arm64CompileResult {
            make_backend_with_method(0, 0, &[0x0f, 0x0f, op, 0xb1])
        }
        fn has(r: &Arm64CompileResult, f: fn(&Arm64Instruction) -> bool) -> bool {
            r.success && r.instructions.iter().any(|i| f(i))
        }
        assert!(has(&single(0x62), |i| matches!(
            i,
            Arm64Instruction::FaddSingle { .. }
        )));
        assert!(has(&single(0x66), |i| matches!(
            i,
            Arm64Instruction::FsubSingle { .. }
        )));
        assert!(has(&single(0x6a), |i| matches!(
            i,
            Arm64Instruction::FmulSingle { .. }
        )));
        assert!(has(&single(0x6e), |i| matches!(
            i,
            Arm64Instruction::FdivSingle { .. }
        )));
        assert!(has(&double(0x63), |i| matches!(
            i,
            Arm64Instruction::FaddDouble { .. }
        )));
        assert!(has(&double(0x67), |i| matches!(
            i,
            Arm64Instruction::FsubDouble { .. }
        )));
        assert!(has(&double(0x6b), |i| matches!(
            i,
            Arm64Instruction::FmulDouble { .. }
        )));
        assert!(has(&double(0x6f), |i| matches!(
            i,
            Arm64Instruction::FdivDouble { .. }
        )));
        assert!(
            !single(0x62)
                .instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::FaddDouble { .. })),
            "fadd must not use the double form"
        );
        // A float operand is not a double: `dadd` of two `fconst`s is refused.
        assert!(!single(0x63).success, "dadd over floats is ill-typed");
    }

    /// `i2f` converts from the W register into an S register, `i2d` into a D
    /// register, and `l2f`/`l2d` from the X register.
    #[test]
    fn integer_to_fp_conversions_pick_source_and_destination_widths() {
        fn conv(code: &[u8]) -> Arm64CompileResult {
            make_backend_with_method(0, 0, code)
        }
        let i2f = conv(&[0x04, 0x86, 0xb1]);
        assert!(i2f.success);
        assert!(i2f
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfSingle { .. })));
        let i2d = conv(&[0x04, 0x87, 0xb1]);
        assert!(i2d
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfDoubleW { .. })));
        let l2f = conv(&[0x0a, 0x89, 0xb1]);
        assert!(l2f
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfSingleX { .. })));
        let l2d = conv(&[0x0a, 0x8a, 0xb1]);
        assert!(l2d
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfDouble { .. })));
        // f2d / d2f are real conversions now, not no-ops.
        let f2d = conv(&[0x0c, 0x8d, 0xb1]);
        assert!(f2d
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FcvtSingleToDouble { .. })));
        let d2f = conv(&[0x0f, 0x90, 0xb1]);
        assert!(d2f
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FcvtDoubleToSingle { .. })));
    }

    #[test]
    fn backend_emit_machine_code_produces_bytes() {
        let result = make_backend_with_method(0, 0, &[0xb1]); // return void
        assert!(result.success);
        let code = emit_machine_code(&result);
        assert!(code.is_some(), "should produce machine code bytes");
        let bytes = code.unwrap();
        assert!(!bytes.is_empty(), "machine code should not be empty");
        // ARM64 instructions are 4 bytes each
        assert_eq!(bytes.len() % 4, 0, "machine code must be 4-byte aligned");
    }

    #[test]
    fn backend_emit_simple_add_produces_code() {
        // int add(int a, int b) { return a + b; }
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
        assert!(result.success);
        let code = emit_machine_code(&result);
        assert!(code.is_some());
        let bytes = code.unwrap();
        assert!(
            bytes.len() >= 16,
            "add method should produce at least a few instructions"
        );
    }

    /// `fneg`/`dneg` flip the sign bit with FNEG.
    ///
    /// Formerly `backend_float_neg_emits_fsub`, which asserted the bug: the
    /// lowering computed `0.0 - x`, and `0.0 - 0.0` is `+0.0`, so `-(0.0f)`
    /// lost its sign.
    #[test]
    fn fneg_and_dneg_use_fneg_not_a_subtraction_from_zero() {
        for (code, single) in [
            (&[0x0c, 0x76, 0xb1][..], true),
            (&[0x0f, 0x77, 0xb1][..], false),
        ] {
            let result = make_backend_with_method(0, 0, code);
            assert!(result.success);
            let neg = result.instructions.iter().any(|i| {
                if single {
                    matches!(i, Arm64Instruction::FnegSingle { .. })
                } else {
                    matches!(i, Arm64Instruction::FnegDouble { .. })
                }
            });
            assert!(neg, "negation must be FNEG of the operand's own width");
            assert!(
                !result.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::FsubDouble { .. } | Arm64Instruction::FsubSingle { .. }
                )),
                "0.0 - x turns -0.0 into +0.0"
            );
        }
    }

    #[test]
    fn backend_float_rem_bails_to_interpreter() {
        // fconst_1, fconst_1, frem (0x72), return void.
        //
        // frem/drem have no EXACT ARM64 lowering in this backend (the
        // truncating FCVTZS round-trip saturates for large operands), so the
        // dispatch bails to the interpreter rather than emit a silently-wrong
        // result. The compile must report failure and emit_machine_code must
        // return None.
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x72, 0xb1]);
        assert!(
            !result.success,
            "frem should bail (success=false), not emit an inexact remainder"
        );
        assert!(
            emit_machine_code(&result).is_none(),
            "a failed compile must not produce machine code"
        );
    }

    #[test]
    fn backend_fconst_bit_exact_for_non_integral() {
        // A constant must move the IEEE-754 bit pattern, NOT perform an
        // integer→float conversion, which would round 2.5 down to 2.0. And a
        // `float` constant must be the SINGLE pattern into an S register.
        let mut single = Arm64Backend::new();
        single.emit_fconst(2.5);
        assert!(single.buffer.instructions().iter().any(
            |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == i64::from(2.5f32.to_bits()))
        ));
        assert!(single
            .buffer
            .instructions()
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovToFpSingle { .. })));
        assert_eq!(single.operand_stack[0].kind, OperandKind::F32);

        let mut backend = Arm64Backend::new();
        backend.emit_dconst(2.5);
        let expected_bits = 2.5f64.to_bits() as i64;
        let has_movimm_bits = backend.buffer.instructions().iter().any(
            |inst| matches!(inst, Arm64Instruction::MovImm { imm, .. } if *imm == expected_bits),
        );
        assert!(
            has_movimm_bits,
            "emit_fconst should materialize the exact f64 bit pattern {:#018x}",
            expected_bits as u64
        );
        let has_fmov = backend
            .buffer
            .instructions()
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FmovToFp { .. }));
        assert!(
            has_fmov,
            "emit_fconst should bit-move the pattern into FP via FmovToFp"
        );
        let has_scvtf = backend
            .buffer
            .instructions()
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::ScvtfDouble { .. }));
        assert!(
            !has_scvtf,
            "emit_fconst must NOT use ScvtfDouble (integer→float conversion)"
        );
    }

    #[test]
    fn backend_ldc_bails_to_interpreter() {
        // ldc (0x12) #1, return void. The backend has no constant pool, so it
        // must bail (success=false) rather than guess at the constant.
        let result = make_backend_with_method(0, 0, &[0x12, 0x01, 0xb1]);
        assert!(
            !result.success,
            "ldc should bail to the interpreter (no constant pool available)"
        );
        // ldc2_w (0x14) #1, return void — same reasoning (long/double constant).
        let result2 = make_backend_with_method(0, 0, &[0x14, 0x00, 0x01, 0xb1]);
        assert!(!result2.success, "ldc2_w should bail to the interpreter");
    }

    #[test]
    fn addsub_imm_safe_no_truncation_for_wide_immediate() {
        use crate::aarch64::{Aarch64Emitter, Reg};

        // Small immediate (fits 12 bits): one ADD-immediate instruction.
        let mut e_small = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(
            &mut e_small,
            Reg::X9,
            Reg::X9,
            5,
            false
        ));
        assert_eq!(
            e_small.code().len(),
            4,
            "a 12-bit immediate should lower to a single ADD-imm"
        );

        // Wide immediate (> 0xFFF, not 4 KiB-aligned): must NOT be truncated to
        // a single ADD-imm. It is materialized into X16 (one or more MOV-wide)
        // then added by register, so the sequence is longer than one
        // instruction and never encodes the (wrong) masked immediate.
        let wide = 5000i32; // 0x1388 — low 12 bits 0x388 != 0, > 0xFFF
        let mut e_wide = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(
            &mut e_wide,
            Reg::X9,
            Reg::X9,
            wide,
            false
        ));
        assert!(
            e_wide.code().len() > 4,
            "a >12-bit immediate must not collapse into one (truncated) ADD-imm"
        );

        // Compare against what a (buggy) single truncated ADD-imm would encode,
        // and assert the safe lowering's first 4 bytes are NOT that instruction.
        let mut e_trunc = Aarch64Emitter::new();
        // The old buggy path: add_imm with the value masked to 12 bits.
        e_trunc.add_imm(Reg::X9, Reg::X9, (wide as u16) & 0xFFF, false);
        let trunc_first = &e_trunc.code()[0..4];
        assert_ne!(
            &e_wide.code()[0..4],
            trunc_first,
            "safe lowering must not begin with the truncated ADD-imm encoding"
        );
    }

    #[test]
    fn backend_fcmp_emits_fcmpdouble() {
        // fconst_1, fconst_1, fcmpl (0x95), return void
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x95, 0xb1]);
        assert!(result.success, "fcmpl should succeed");
        let has_fcmp = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FcmpSingle { .. }));
        assert!(has_fcmp, "fcmpl of two floats should emit FcmpSingle");
    }

    #[test]
    fn backend_emit_machine_code_with_float_ops() {
        // fconst_1, fconst_1, fadd, return void
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x62, 0xb1]);
        assert!(result.success);
        let code = emit_machine_code(&result);
        assert!(
            code.is_some(),
            "float operations should produce machine code"
        );
        let bytes = code.unwrap();
        assert_eq!(bytes.len() % 4, 0, "machine code must be 4-byte aligned");
    }

    // --- M28 fix tests: comparison opcodes, lcmp, idiv ---

    #[test]
    fn backend_iflt_opcode() {
        // iconst_1, iflt +3 (offset to return), return
        // 0x04=iconst_1, 0x9b=iflt, 00 06=offset +6 (to return), 0xb1=return
        let result = make_backend_with_method(0, 0, &[0x04, 0x9b, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "iflt should compile successfully");
    }

    #[test]
    fn backend_ifge_opcode() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x9c, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "ifge should compile successfully");
    }

    #[test]
    fn backend_ifgt_opcode() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x9d, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "ifgt should compile successfully");
    }

    #[test]
    fn backend_ifle_opcode() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x9e, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "ifle should compile successfully");
    }

    #[test]
    fn backend_lcmp_opcode() {
        // lconst_0, lconst_1, lcmp, ireturn
        // 0x09=lconst_0, 0x0a=lconst_1, 0x94=lcmp, 0xac=ireturn
        let result = make_backend_with_method(0, 0, &[0x09, 0x0a, 0x94, 0xac]);
        assert!(result.success, "lcmp should compile successfully");
    }

    // `backend_idiv_opcode` (iconst_2, iconst_1, idiv, ireturn — asserted
    // `success`) was removed with the 82a9d08fc div/rem refusal: inverted it
    // would assert strictly less than
    // `backend_int_div_bails_with_constant_operands` above, on the same shape.

    #[test]
    fn backend_ifeq_uses_label_not_raw_target() {
        // iconst_0, ifeq +5 (to second return), return, return
        // 0x03=iconst_0, 0x99=ifeq, 0x00 0x05=branch offset
        let result = make_backend_with_method(0, 0, &[0x03, 0x99, 0x00, 0x05, 0xb1, 0xb1]);
        assert!(
            result.success,
            "ifeq with label should compile successfully"
        );
    }

    #[test]
    fn backend_bounds_check_on_truncated_bytecode() {
        // Truncated ifeq: only 2 bytes instead of 3 (opcode + 2 offset bytes)
        let result = make_backend_with_method(0, 0, &[0x99, 0x00]);
        // Should fail gracefully, not panic
        assert!(
            !result.success || result.instructions.is_empty(),
            "truncated branch should not produce valid code"
        );
    }

    // ===================================================================
    // Phase 95 — ARM64 graph-coloring register allocator integration
    // ===================================================================

    #[test]
    fn p95_backend_uses_graph_coloring_for_locals() {
        // int f(int a, int b) { int c = a + b; return c; }
        // iload_0, iload_1, iadd, istore_2, iload_2, ireturn
        let result = make_backend_with_method(3, 2, &[0x1a, 0x1b, 0x60, 0x3d, 0x1c, 0xac]);
        assert!(result.success);
        // Graph coloring may assign 2 or 3 regs — local 2 might share a register
        // with local 0 or 1 since their live ranges don't fully overlap.
        let saved_count = result.frame.saved_regs.len();
        assert!(
            saved_count >= 2 && saved_count <= 3,
            "graph coloring should identify 2-3 used callee-saved regs, got {}",
            saved_count
        );
        for reg in &result.frame.saved_regs {
            assert!(
                reg.is_callee_saved(),
                "saved reg X{} should be callee-saved",
                reg.0
            );
        }
    }

    #[test]
    fn p95_backend_non_interfering_locals_can_share_nothing_extra() {
        // Locals used in sequence (no simultaneous live ranges):
        // iload_0; istore_2; iload_1; ireturn
        // Locals 0, 1 are params (live at entry), local 2 is temp.
        let result = make_backend_with_method(3, 2, &[0x1a, 0x3d, 0x1b, 0xac]);
        assert!(result.success);
        // All 3 locals should get registers (no spills needed).
        let saved_count = result.frame.saved_regs.len();
        assert!(
            saved_count <= 3 && saved_count >= 2,
            "expected 2-3 callee-saved regs, got {saved_count}"
        );
    }

    #[test]
    fn p95_backend_spill_slot_for_11th_local() {
        // 11 locals, all loaded → 10 get regs, 1 spills.
        // Load local 10 (spilled) and return it.
        let result = make_backend_with_method(11, 0, &[0x15, 10, 0xac]);
        assert!(result.success);
        // Local 10 should be loaded from a frame spill slot (Ldr from FP).
        let has_spill_load = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
            )
        });
        assert!(has_spill_load, "11th local should spill to stack");
    }

    /// Float locals must NOT be homed in `D8`–`D15`.
    ///
    /// Replaces `p95_backend_float_local_uses_fp_reg`, which asserted the
    /// opposite. `regalloc::ARM64_LOCAL_FPS` is `D8..D15`, which AAPCS64 makes
    /// **callee-saved**, and this backend's prologue/epilogue save only GPRs —
    /// so homing a float local there destroyed the caller's copy. See the
    /// comment on the `float_local_regs` loop in `compile_pass`.
    ///
    /// The observable consequence is that a spilled float local goes through a
    /// frame slot (`FpStr`/`FpLdr`) or a GPR bit-move, never `FmovFp` from a
    /// callee-saved D register.
    #[test]
    fn float_locals_never_use_callee_saved_fp_regs() {
        // fconst_1 (0x0c), fstore_0 (0x43), fload_0 (0x22), return void (0xb1)
        let result = make_backend_with_method(1, 0, &[0x0c, 0x43, 0x22, 0xb1]);
        assert!(result.success);

        // No instruction may name an FP register outside the caller-saved
        // scratch set V0-V7 (encoded 32..=39). D8-D15 would appear as 40..=47.
        let mut offenders: Vec<u8> = Vec::new();
        fn note(reg: Arm64Register, offenders: &mut Vec<u8>) {
            if reg.0 >= 40 {
                offenders.push(reg.0);
            }
        }
        for inst in &result.instructions {
            match inst {
                Arm64Instruction::FmovFp { vd, vn } => {
                    note(*vd, &mut offenders);
                    note(*vn, &mut offenders);
                }
                Arm64Instruction::FmovToFp { vd, .. } => note(*vd, &mut offenders),
                Arm64Instruction::FmovFromFp { vn, .. } => note(*vn, &mut offenders),
                Arm64Instruction::FpLdr { vt, .. } | Arm64Instruction::FpStr { vt, .. } => {
                    note(*vt, &mut offenders)
                }
                _ => {}
            }
        }
        assert!(
            offenders.is_empty(),
            "float locals must not be homed in callee-saved D8-D15 \
             (this backend never saves them); saw register encodings {offenders:?}"
        );

        // And the value really does round-trip through a frame slot, i.e. the
        // spill path — not silently dropped.
        let has_fp_spill = result
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FpStr { .. }));
        let has_gpr_home = result
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovFromFp { .. }));
        assert!(
            has_fp_spill || has_gpr_home,
            "fstore_0 must store the float somewhere (frame slot or GPR)"
        );
    }

    /// The FP frame-slot lowering must never use the scaled unsigned-offset
    /// form for a negative displacement.
    ///
    /// `Arm64FrameLayout::spill_offset` is always negative, and the previous
    /// lowering did `offset as u16` — turning −24 into 65512, which the encoder
    /// then scales by 8. That is a load/store ~64 KiB *above* FP, inside the
    /// caller's frame. This checks the encoded bytes directly: an unscaled
    /// LDUR/STUR (bit 24 clear) rather than the scaled form (bit 24 set).
    #[test]
    fn fp_frame_slot_access_uses_unscaled_form_for_negative_offsets() {
        let result = result_from_instructions(vec![
            Arm64Instruction::FpStr {
                vt: Arm64Register::V0,
                rn: Arm64Register::FP,
                offset: -24,
                is_double: true,
            },
            Arm64Instruction::FpLdr {
                vt: Arm64Register::V1,
                rn: Arm64Register::FP,
                offset: -24,
                is_double: true,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("FP frame access must encode");
        assert_eq!(bytes.len(), 3 * 4);

        // STUR D0, [X29, #-24] = 0xFC1E83A0; LDUR D1, [X29, #-24] = 0xFC5E83A1.
        // (Derived from the GPR STUR/LDUR words with the V bit — 1<<26 — set;
        // imm9 = -24 & 0x1FF = 0x1E8.)
        let w0 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let w1 = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(w0, 0xFC1E_83A0, "STUR D0, [X29, #-24]");
        assert_eq!(w1, 0xFC5E_83A1, "LDUR D1, [X29, #-24]");

        for (name, w) in [("store", w0), ("load", w1)] {
            assert_eq!(
                (w >> 24) & 1,
                0,
                "{name} must NOT be the scaled unsigned-offset form (that form \
                 cannot encode a negative displacement — it reads -24 as 65512)"
            );
            assert_eq!(
                (w >> 10) & 0x3,
                0b00,
                "{name} must not write back to the base register (FP)"
            );
        }
    }

    /// A positive, correctly-scaled FP offset must still take the compact
    /// scaled form, so the fix above is a routing change and not a blanket
    /// switch to the (shorter-range) unscaled encoding.
    #[test]
    fn fp_positive_aligned_offset_still_uses_scaled_form() {
        let result = result_from_instructions(vec![
            Arm64Instruction::FpLdr {
                vt: Arm64Register::V0,
                rn: Arm64Register::X1,
                offset: 16,
                is_double: true,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("must encode");
        let w = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        // LDR D0, [X1, #16] — scaled unsigned form has bit 24 set, imm12 = 2.
        assert_eq!(
            (w >> 24) & 1,
            1,
            "positive aligned offset uses the scaled form"
        );
        assert_eq!((w >> 10) & 0xFFF, 2, "imm12 must be 16/8 == 2");
    }

    /// An FP offset outside the imm9 range must materialize the address rather
    /// than truncate. 4 words: MOVN/MOVZ(+MOVK) into IP0, ADD, then the access.
    #[test]
    fn fp_far_offset_materializes_address() {
        let result = result_from_instructions(vec![
            Arm64Instruction::FpStr {
                vt: Arm64Register::V0,
                rn: Arm64Register::FP,
                offset: -100_000,
                is_double: true,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("must encode");
        assert!(
            bytes.len() > 2 * 4,
            "a far FP offset must expand into an address materialization, \
             not a single truncated access"
        );
        // Last instruction before RET is the zero-offset access off IP0 (X16).
        let n = bytes.len();
        let access = u32::from_le_bytes([bytes[n - 8], bytes[n - 7], bytes[n - 6], bytes[n - 5]]);
        assert_eq!((access >> 5) & 0x1F, 16, "access base must be IP0 (X16)");
        assert_eq!(
            (access >> 12) & 0x1FF,
            0,
            "materialized access uses offset 0"
        );
    }

    // ===================================================================
    // Phase 95.2 — New bytecode compilation tests
    // ===================================================================

    #[test]
    fn p95_aload_astore_compiles() {
        // aload_0, astore_1, aload_1, areturn
        let result = make_backend_with_method(2, 1, &[0x2a, 0x4c, 0x2b, 0xb0]);
        assert!(result.success, "aload/astore should compile");
    }

    #[test]
    fn p95_iinc_compiles() {
        // iload_0, iinc 0 5, iload_0, ireturn
        let result = make_backend_with_method(1, 1, &[0x1a, 0x84, 0x00, 0x05, 0x1a, 0xac]);
        assert!(result.success, "iinc should compile");
        // Should emit AddImm for the increment
        let has_add = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::AddImmW { imm: 5, .. }));
        assert!(has_add, "iinc +5 should emit the 32-bit AddImmW with imm=5");
    }

    #[test]
    fn p95_iinc_negative_compiles() {
        // iinc 0 -1
        let result = make_backend_with_method(1, 1, &[0x84, 0x00, 0xFF, 0x1a, 0xac]);
        assert!(result.success, "iinc -1 should compile");
        let has_sub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::SubImmW { imm: 1, .. }));
        assert!(has_sub, "iinc -1 should emit the 32-bit SubImmW with imm=1");
    }

    #[test]
    fn p95_iushr_compiles() {
        // iconst_1, iconst_1, iushr, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x04, 0x7c, 0xac]);
        assert!(result.success, "iushr should compile");
        let has_lsr = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::LsrW { .. }));
        assert!(
            has_lsr,
            "iushr should emit the 32-bit LsrW, which shifts by the amount mod 32"
        );
    }

    /// Constant-operand counterpart of
    /// `backend_irem_and_lrem_bail_to_interpreter`. The old `a - (a/b)*b`
    /// lowering is what made `x % 0` return `x` (AArch64 `SDIV` by zero yields
    /// 0 instead of trapping), so the assertion is that no `Msub` is emitted,
    /// not merely that the method bailed.
    #[test]
    fn p95_irem_bails_with_constant_operands() {
        // iconst_5, iconst_2, irem, ireturn
        let result = make_backend_with_method(0, 0, &[0x08, 0x05, 0x70, 0xac]);
        assert!(
            !result.success,
            "irem must bail, constant operands included"
        );
        let has_msub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Msub { .. }));
        assert!(
            !has_msub,
            "no Msub may be emitted: `a - (a/b)*b` is exactly the lowering that \
             returned `a` for `a % 0`"
        );
    }

    #[test]
    fn p95_dup_x1_compiles() {
        // iconst_1, iconst_2, dup_x1, pop, pop, pop, return
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x5a, 0x57, 0x57, 0x57, 0xb1]);
        assert!(result.success, "dup_x1 should compile");
    }

    #[test]
    fn p95_dup2_compiles() {
        // iconst_1, iconst_2, dup2, pop, pop, pop, pop, return
        let result =
            make_backend_with_method(0, 0, &[0x04, 0x05, 0x5c, 0x57, 0x57, 0x57, 0x57, 0xb1]);
        assert!(result.success, "dup2 should compile");
    }

    #[test]
    fn p95_tableswitch_compiles() {
        // iconst_1, tableswitch { low=0, high=2, default→8, 0→8, 1→8, 2→8 }, return
        // PC 0: iconst_1 (opcode 0x04)
        // PC 1: tableswitch (0xaa)
        // Padding to align to 4: 2 bytes (pc 2, 3)
        // PC 4: default offset: +7 → target PC 8
        // PC 8: low = 0
        // PC 12: high = 2
        // PC 16: offset[0] = +7 → target PC 8
        // PC 20: offset[1] = +7
        // PC 24: offset[2] = +7
        // PC 28: return
        let bytecode = &[
            0x04, // 0: iconst_1
            0xaa, // 1: tableswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1b, // 4: default → +27 → PC 28
            0x00, 0x00, 0x00, 0x00, // 8: low = 0
            0x00, 0x00, 0x00, 0x02, // 12: high = 2
            0x00, 0x00, 0x00, 0x1b, // 16: case 0 → +27 → PC 28
            0x00, 0x00, 0x00, 0x1b, // 20: case 1 → +27
            0x00, 0x00, 0x00, 0x1b, // 24: case 2 → +27
            0xb1, // 28: return
        ];
        let result = make_backend_with_method(0, 0, bytecode);
        assert!(result.success, "tableswitch should compile");
    }

    #[test]
    fn p95_lookupswitch_compiles() {
        // iconst_1, lookupswitch { npairs=2, default→done, 1→done, 42→done }, return
        let _bytecode = &[
            0x04, // 0: iconst_1
            0xab, // 1: lookupswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1c, // 4: default → +28 → PC 29
            0x00, 0x00, 0x00, 0x02, // 8: npairs = 2
            0x00, 0x00, 0x00, 0x01, // 12: match 1
            0x00, 0x00, 0x00, 0x1c, // 16: → +28
            0x00, 0x00, 0x00, 0x2a, // 20: match 42
            0x00, 0x00, 0x00, 0x1c, // 24: → +28
            0xb1, // 28: return (at PC 28 but target is 29, let me fix)
        ];
        // Target PC should be 1 + 28 = 29, but that's past the bytecode. Let me make it target PC 28.
        let bytecode2 = &[
            0x04, // 0: iconst_1
            0xab, // 1: lookupswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1b, // 4: default → +27 → PC 28
            0x00, 0x00, 0x00, 0x02, // 8: npairs = 2
            0x00, 0x00, 0x00, 0x01, // 12: match 1
            0x00, 0x00, 0x00, 0x1b, // 16: → +27 → PC 28
            0x00, 0x00, 0x00, 0x2a, // 20: match 42
            0x00, 0x00, 0x00, 0x1b, // 24: → +27 → PC 28
            0xb1, // 28: return
        ];
        let result = make_backend_with_method(0, 0, bytecode2);
        assert!(result.success, "lookupswitch should compile");
    }

    #[test]
    fn p95_if_acmpeq_compiles() {
        // aconst_null, aconst_null, if_acmpeq +5, return, return
        let result = make_backend_with_method(0, 0, &[0x01, 0x01, 0xa5, 0x00, 0x05, 0xb1, 0xb1]);
        assert!(result.success, "if_acmpeq should compile");
    }

    #[test]
    fn p95_lshl_lushr_compiles() {
        // lconst_1, iconst_1, lshl, iconst_1, lushr, lreturn. The shift amount
        // is an `int` (JVMS 6.5 `lshl`); an earlier version of this test shifted
        // by a `long`, which the typed operand stack now refuses.
        let result = make_backend_with_method(0, 0, &[0x0a, 0x04, 0x79, 0x04, 0x7d, 0xad]);
        assert!(result.success, "lshl/lushr should compile");
    }

    #[test]
    fn p95_nop_compiles() {
        let result = make_backend_with_method(0, 0, &[0x00, 0xb1]);
        assert!(result.success, "nop should compile");
    }

    /// A loop must be REFUSED, because this backend emits no safepoint poll.
    ///
    /// Formerly `p95_backend_fibonacci_compiles`, which asserted the opposite.
    /// See [`Arm64Backend::label_for_pc`]: x86-64 polls
    /// `helpers.safepoint_flag_addr` at every back-edge
    /// (`x64::Backend::emit_safepoint_poll`); this backend has no poll and no
    /// way to emit one, so a compiled loop is a region a thread can sit in
    /// forever without ever reaching a stop-the-world request — the GC then
    /// hangs the VM. Refusing the method and interpreting it is the only sound
    /// option, and interpretation restores the interpreter's own polls.
    #[test]
    fn loop_method_bails_no_safepoint_poll() {
        // Fibonacci-like: int fib(int n) with loop
        // local 0 = n (param), local 1 = a = 0, local 2 = b = 1, local 3 = tmp
        // istore_1(a=0), iconst_1, istore_2(b=1), iload_0, ifle done,
        // loop: iload_2, iload_1, iadd, istore_3, iload_2, istore_1, iload_3, istore_2,
        //       iinc 0 -1, iload_0, ifgt loop, done: iload_1, ireturn
        let bytecode: &[u8] = &[
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1  (a = 0)
            0x04, // 2: iconst_1
            0x3d, // 3: istore_2  (b = 1)
            0x1a, // 4: iload_0   (n)
            0x9e, 0x00, 0x14, // 5: ifle +20 → 25
            // loop body at PC 8:
            0x1c, // 8: iload_2   (b)
            0x1b, // 9: iload_1   (a)
            0x60, // 10: iadd     (a+b)
            0x3e, // 11: istore_3 (tmp = a+b)
            0x1c, // 12: iload_2  (b)
            0x3c, // 13: istore_1 (a = b)
            0x1d, // 14: iload_3  (tmp)
            0x3d, // 15: istore_2 (b = tmp)
            0x84, 0x00, 0xff, // 16: iinc 0, -1  (n--)
            0x1a, // 19: iload_0  (n)
            0x9d, 0xff, 0xf3, // 20: ifgt -13 → 8
            // done at PC 23:
            0x1b, // 23: iload_1  (a)
            0xac, // 24: ireturn
        ];
        let result = make_backend_with_method(4, 1, bytecode);
        assert!(
            !result.success,
            "a method with a loop back-edge must bail: there is no safepoint \
             poll to place on the back-edge, so a thread in this loop would \
             never reach a stop-the-world request"
        );
        assert!(
            emit_machine_code(&result).is_none(),
            "the encoder must also refuse a bailed result"
        );

        // Negative control: the same shape of body with only a FORWARD branch
        // still compiles, so the bail is specific to the back-edge and is not a
        // blanket refusal of branching methods.
        let straight_line: &[u8] = &[
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x04, // 2: iconst_1
            0x3d, // 3: istore_2
            0x1a, // 4: iload_0
            0x9e, 0x00, 0x0a, // 5: ifle +10 → pc 15 (forward)
            0x1c, // 8: iload_2
            0x1b, // 9: iload_1
            0x60, // 10: iadd
            0x3e, // 11: istore_3
            0x1b, // 12: iload_1
            0xac, // 13: ireturn
            0x00, // 14: nop
            0x1b, // 15: iload_1
            0xac, // 16: ireturn
        ];
        let ok = make_backend_with_method(4, 1, straight_line);
        assert!(
            ok.success,
            "the same body with only a forward branch must still compile"
        );
    }

    /// A single backward `goto` — the minimal back-edge — must bail, including
    /// the degenerate `goto 0` self-loop.
    #[test]
    fn backward_goto_and_self_loop_both_bail() {
        // nop; goto -1 from pc 1 → target pc 0, the tightest possible loop.
        let self_loop = make_backend_with_method(0, 0, &[0x00, 0xa7, 0xff, 0xff]);
        assert!(!self_loop.success, "a one-instruction loop must bail");

        // nop; nop; goto -2 (back to pc 0)
        let back = make_backend_with_method(0, 0, &[0x00, 0x00, 0xa7, 0xff, 0xfe]);
        assert!(!back.success, "a backward goto must bail");

        // Forward goto over a return — still fine.
        let fwd = make_backend_with_method(0, 0, &[0xa7, 0x00, 0x04, 0xb1, 0xb1]);
        assert!(fwd.success, "a forward goto must still compile");
    }

    /// A backward `tableswitch` case target is a back-edge too, and switch
    /// targets take a different code path (`label_for_pc` from the switch arm
    /// rather than from an `if*` arm), so it gets its own guard.
    #[test]
    fn backward_switch_case_target_bails() {
        // pc 0: nop
        // pc 1: nop
        // pc 2: nop
        // pc 3: iconst_1
        // pc 4: tableswitch, padding to pc 8, default +12 (→ pc 16), low=0,
        //       high=0, case 0 offset = -4 (→ pc 0, a back-edge)
        let bytecode: &[u8] = &[
            0x00, 0x00, 0x00, // 0-2: nop nop nop
            0x04, // 3: iconst_1
            0xaa, // 4: tableswitch
            0x00, 0x00, 0x00, // 5-7: padding to 4-byte boundary
            0x00, 0x00, 0x00, 0x14, // 8: default → +20 → pc 24 (forward)
            0x00, 0x00, 0x00, 0x00, // 12: low = 0
            0x00, 0x00, 0x00, 0x00, // 16: high = 0
            0xff, 0xff, 0xff, 0xfc, // 20: case 0 → -4 → pc 0 (BACK-EDGE)
            0xb1, // 24: return
        ];
        let result = make_backend_with_method(0, 0, bytecode);
        assert!(
            !result.success,
            "a backward switch case target is a back-edge and must bail too"
        );
    }

    // ===================================================================
    // Phase 95.3 — NEON vectorization tests
    // ===================================================================

    // NOTE: the `p95_neon_pattern_detection_*` tests were removed alongside the
    // dead `detect_neon_patterns` / `NeonVectorizablePattern` scanner (2026-06-10
    // JIT cleanup). That scanner was never wired into codegen and emitted
    // placeholder local indices; see the removal note near the top of this file.

    #[test]
    fn p95_neon_machine_code_emission() {
        let mut backend = Arm64Backend::new();
        backend.buffer = Arm64CodeBuffer::new();
        backend.frame = Some(Arm64FrameLayout::compute(0, 16, &[]));
        backend.local_regs = Vec::new();
        backend.float_local_regs = Vec::new();

        // Emit a small NEON sequence
        backend.buffer.emit(Arm64Instruction::NeonLd1_4s {
            vt: Arm64Register::V0,
            rn: Arm64Register::X0,
        });
        backend.buffer.emit(Arm64Instruction::NeonAdd4s {
            vd: Arm64Register::V0,
            vn: Arm64Register::V0,
            vm: Arm64Register::V1,
        });
        backend.buffer.emit(Arm64Instruction::NeonMul4s {
            vd: Arm64Register::V2,
            vn: Arm64Register::V0,
            vm: Arm64Register::V1,
        });
        backend.buffer.emit(Arm64Instruction::NeonSt1_4s {
            vt: Arm64Register::V2,
            rn: Arm64Register::X1,
        });
        backend.buffer.emit(Arm64Instruction::Ret);

        let result = Arm64CompileResult {
            safepoint_count: 0,
            incomplete_oop_maps: 0,
            static_init_classes: Vec::new(),
            sp_id_slot_off: 0,
            instructions: backend.buffer.instructions().to_vec(),
            frame: Arm64FrameLayout::compute(0, 0, &[]),
            labels: backend.buffer.labels.clone(),
            success: true,
            pending_oop_maps: Vec::new(),
            needs_context: false,
            polls_enabled: false,
            frame_base_published: false,
        };

        let code = emit_machine_code(&result);
        assert!(
            code.is_some(),
            "NEON instructions should produce machine code"
        );
        let bytes = code.unwrap();
        assert_eq!(
            bytes.len(),
            5 * 4,
            "5 instructions × 4 bytes each = 20 bytes"
        );
        // All bytes should be non-zero (valid ARM64 encodings)
        assert!(
            bytes.iter().any(|&b| b != 0),
            "encoded bytes should be non-trivial"
        );
    }

    // -----------------------------------------------------------------------
    // aarch64-coverage audit (2026-07-26): `emit_machine_code` soundness gates
    // -----------------------------------------------------------------------

    /// Build a minimal `Arm64CompileResult` around a caller-supplied
    /// instruction sequence, with `success = true` so the only thing under
    /// test is `emit_machine_code`'s own validation.
    fn result_from_instructions(instructions: Vec<Arm64Instruction>) -> Arm64CompileResult {
        Arm64CompileResult {
            instructions,
            frame: Arm64FrameLayout::compute(0, 0, &[]),
            labels: HashMap::new(),
            success: true,
            pending_oop_maps: Vec::new(),
            sp_id_slot_off: 0,
            safepoint_count: 0,
            incomplete_oop_maps: 0,
            static_init_classes: Vec::new(),
            needs_context: false,
            polls_enabled: false,
            frame_base_published: false,
        }
    }

    #[test]
    fn emit_machine_code_bails_on_unbound_branch_label() {
        // `B <label 7>` where label 7 is never bound. The old patch loop left
        // the displacement-0 placeholder in place — a branch to itself, i.e.
        // an infinite loop inside successfully-"compiled" code.
        let result = result_from_instructions(vec![
            Arm64Instruction::B { label: 7 },
            Arm64Instruction::Ret,
        ]);
        assert!(
            emit_machine_code(&result).is_none(),
            "an unbound branch label must bail the method, not emit `B .`"
        );
    }

    #[test]
    fn emit_machine_code_bails_on_unbound_conditional_branch_label() {
        let result = result_from_instructions(vec![
            Arm64Instruction::Cbz {
                rt: Arm64Register::X0,
                label: 3,
            },
            Arm64Instruction::Ret,
        ]);
        assert!(
            emit_machine_code(&result).is_none(),
            "an unbound CBZ label must bail the method"
        );
    }

    #[test]
    fn emit_machine_code_bails_on_unbound_ldr_literal_label() {
        // An unpatched LDR (literal) reads at `pc + 0` — the instruction
        // itself — rather than a constant-pool entry.
        let result = result_from_instructions(vec![
            Arm64Instruction::LdrLiteral {
                rt: Arm64Register::X0,
                label: 11,
            },
            Arm64Instruction::Ret,
        ]);
        assert!(
            emit_machine_code(&result).is_none(),
            "an unbound LDR-literal label must bail the method"
        );
    }

    #[test]
    fn emit_machine_code_accepts_bound_labels() {
        // Companion to the three bail tests: a correctly bound forward branch
        // must still encode, so the new checks cannot be satisfied by a
        // blanket refusal.
        let result = result_from_instructions(vec![
            Arm64Instruction::B { label: 1 },
            Arm64Instruction::Nop,
            Arm64Instruction::Label(1),
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("bound labels must encode");
        // 3 real instructions (Label emits nothing) × 4 bytes.
        assert_eq!(bytes.len(), 3 * 4);
        // The `B` at offset 0 targets offset 8 → imm26 == 2.
        let b = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(b >> 26, 0b000101, "opcode must be unconditional B");
        assert_eq!(b & 0x03ff_ffff, 2, "B displacement must be +2 instructions");
    }

    /// `B.cond`/`CBZ` carry a signed imm19 → ±1 MiB. A conditional branch
    /// over more than 2^18 instructions cannot be encoded.
    ///
    /// `Aarch64Emitter::mark_branch_overflow` does two things: trips a
    /// `debug_assert!` (active in debug/test builds) AND sets the sticky
    /// `overflowed` flag (always). `debug_assert!` is compiled out under
    /// `--release`, so the assertion this test can make differs per profile —
    /// same split as `aarch64::tests::assert_branch_overflow_detected`.
    #[test]
    fn emit_machine_code_bails_on_out_of_range_conditional_branch() {
        const SPAN: usize = (1 << 18) + 4; // > 2^18 instructions ⇒ > ±1 MiB

        let build = || {
            let mut insts = Vec::with_capacity(SPAN + 3);
            insts.push(Arm64Instruction::Cbz {
                rt: Arm64Register::X0,
                label: 1,
            });
            insts.resize(SPAN + 1, Arm64Instruction::Nop);
            insts.push(Arm64Instruction::Label(1));
            insts.push(Arm64Instruction::Ret);
            result_from_instructions(insts)
        };

        if cfg!(debug_assertions) {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                emit_machine_code(&build())
            }));
            std::panic::set_hook(prev);
            assert!(
                outcome.is_err(),
                "an out-of-range CBZ patch must trip the debug_assert! in debug builds"
            );
        } else {
            assert!(
                emit_machine_code(&build()).is_none(),
                "an out-of-range CBZ patch must set the sticky overflow flag and bail"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Division / remainder are refused while the throw path is UNWIRED (no
    // `throw_arithmetic` helper, or no word that the exception table is
    // empty) — see the module header and the `r9w11_*` tests for the wired
    // lowering. `make_backend_with_method` wires neither.
    // -----------------------------------------------------------------------

    #[test]
    fn backend_idiv_bails_to_interpreter() {
        // iload_0; iload_1; idiv; ireturn
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x6c, 0xac]);
        assert!(
            !result.success,
            "idiv must bail with no ArithmeticException path wired (the pre-round-7 \
             guard was BRK #1, i.e. SIGTRAP)"
        );
        assert!(emit_machine_code(&result).is_none());
    }

    #[test]
    fn backend_ldiv_bails_to_interpreter() {
        // lload_0; lload_1; ldiv; lreturn
        let result = make_backend_with_method(2, 2, &[0x1e, 0x1f, 0x6d, 0xad]);
        assert!(
            !result.success,
            "ldiv must bail for the same reason as idiv"
        );
    }

    #[test]
    fn backend_irem_and_lrem_bail_to_interpreter() {
        // AArch64 SDIV by zero yields 0 (it does not trap), so the old
        // `a - (a/b)*b` lowering returned `a` for `a % 0` instead of throwing.
        for &(op, name) in &[(0x70u8, "irem"), (0x71u8, "lrem")] {
            let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, op, 0xac]);
            assert!(
                !result.success,
                "{name} must bail: SDIV-by-zero silently yields 0 on AArch64"
            );
        }
    }

    /// The refusal must be specific to div/rem — the surrounding integer
    /// arithmetic still compiles.
    #[test]
    fn backend_imul_still_compiles_after_div_refusal() {
        // iload_0; iload_1; imul; ireturn
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x68, 0xac]);
        assert!(
            result.success,
            "imul must be unaffected by the div/rem bail"
        );
        assert!(emit_machine_code(&result).is_some());
    }

    /// Coverage guard for the module header's census: every opcode of the
    /// object model must refuse the method WHEN NOTHING IS WIRED FOR IT.
    ///
    /// What this asserts has narrowed as the backend grew, and it is worth
    /// being precise about what is left. Most of these opcodes now HAVE a
    /// lowering; `make_backend_with_method` wires no helper, no site table and
    /// no throw path, so each refuses in its own arm. The test still earns its
    /// place: a lowering that quietly stopped requiring what it needs -- a
    /// resolved site, an exact exception path, an empty exception table --
    /// would compile here, and this is what says so. The header's table of
    /// CONDITIONAL rows is the other half of the same rule.
    #[test]
    fn object_model_opcodes_are_all_unsupported() {
        // One representative per documented unsupported area, plus every
        // field/dispatch/allocation opcode (the ones whose absence defines
        // this backend's scope).
        let unsupported: &[u8] = &[
            0x2e, 0x32, 0x35, // iaload / aaload / saload
            0x4f, 0x53, 0x56, // iastore / aastore / sastore
            0xb2, 0xb3, 0xb4, 0xb5, // getstatic / putstatic / getfield / putfield
            0xb6, 0xb7, 0xb9, 0xba, // invokevirtual/special/interface/dynamic
            0xbb, 0xbc, 0xbd, 0xbe, // new / newarray / anewarray / arraylength
            0xbf, // athrow
            0xc0, 0xc1, // checkcast / instanceof
            // `monitorenter`/`monitorexit` (0xc2/0xc3) left this list at h23:
            // they have a lowering now, and it refuses only for an unwired
            // helper -- which is the `every_shared_memory_opcode_...` test's
            // business, not this one's.
            // `multianewarray` (0xc5) left this list at h23c: it has a
            // lowering now, and refuses only for an unresolved site or an
            // unwired helper -- which is the
            // `every_shared_memory_opcode_...` test's business.
            0xc4, 0xc8, // wide / goto_w
        ];
        for &op in unsupported {
            // Two operand bytes cover the widest of these; trailing bytes are
            // irrelevant because the arm bails before consuming them.
            let result = make_backend_with_method(2, 2, &[op, 0x00, 0x01, 0xb1]);
            assert!(
                !result.success,
                "opcode 0x{op:02x} unexpectedly compiled — update the coverage \
                 table in this module's header before landing that"
            );
        }
    }

    /// Every opcode the shared-memory predicate names must be refused **by the
    /// gate**, not by the `_` arm -- unless it is in
    /// [`opcode_has_ordered_lowering`], in which case its OWN arm must refuse
    /// it while nothing is wired for it, and must say what is missing.
    ///
    /// The distinction is the whole point. Before the gate, these opcodes were
    /// refused because nobody had written a lowering for them -- an emergent
    /// property, invisible to the next person to write one. The assertion on
    /// the emitted comment is what makes it a rule: adding a `getfield` arm
    /// without the ordering instructions leaves the method refused and this
    /// test passing; removing the gate fails it.
    ///
    /// The `expected` table is the second half of the rule, and the reason
    /// this test grew rather than being deleted when the blanket
    /// `ARM64_CAN_ORDER_MEMORY` went away (round 9 wave 14): an opcode cannot
    /// join `opcode_has_ordered_lowering` without someone stating, here, what
    /// its unwired refusal says.
    #[test]
    fn every_shared_memory_opcode_without_an_ordered_lowering_is_refused() {
        for op in 0u8..=0xff {
            if !opcode_touches_shared_memory(op) {
                continue;
            }
            // Three operand bytes cover the widest of these (multianewarray);
            // trailing bytes are irrelevant because the gate bails before
            // consuming any of them.
            let result = make_backend_with_method(2, 2, &[op, 0x00, 0x01, 0x01, 0xb1]);
            assert!(
                !result.success,
                "opcode 0x{op:02x} touches shared memory and must not compile \
                 with nothing wired"
            );
            if opcode_has_ordered_lowering(op) {
                let expected = match op {
                    0xb2 | 0xb3 => "no resolved static field",
                    0xbb..=0xbd => "no resolved class, an unwired helper",
                    0xbf => "an unwired helper, or a non-empty exception table",
                    0xb4 | 0xb5 => "no resolved instance field",
                    // Round 9 wave 22. `invokedynamic` shares the arm and the
                    // comment, though its refusal is unconditional: the
                    // dispatch helper has no `invoke_kind` for a call site
                    // that is not a class.
                    0xb6..=0xba => "unresolved site, unwired dispatch helper",
                    0xbe => "no exact NullPointerException path",
                    // Round 9 wave 23.
                    0xc0 | 0xc1 => "unresolved target class, an unwired helper",
                    // h23, 2026-09-22.
                    0xc2 | 0xc3 => "an unwired helper, or no route for a pending exception",
                    // h23c, 2026-09-22.
                    0xc5 => "no resolved site, an unwired helper, an unsupported arity",
                    0x2e..=0x35 | 0x4f..=0x56 => "no exact array exception path",
                    _ => panic!(
                        "opcode 0x{op:02x} gained an ordered lowering without telling this \
                         test what its unwired refusal says"
                    ),
                };
                let refused_by_arm = result.instructions.iter().any(
                    |inst| matches!(inst, Arm64Instruction::Comment(c) if c.contains(expected)),
                );
                assert!(
                    refused_by_arm,
                    "opcode 0x{op:02x} has an ordered lowering and must be refused by its \
                     own arm when nothing is wired for it"
                );
                continue;
            }
            let gated = result.instructions.iter().any(|inst| {
                matches!(inst, Arm64Instruction::Comment(c) if c.contains("touches shared memory"))
            });
            assert!(
                gated,
                "opcode 0x{op:02x} was refused, but by the fallthrough arm rather than by \
                 the shared-memory gate — the refusal must be the rule, not an accident"
            );
        }
    }

    /// h23 (2026-09-22): `monitorenter`/`monitorexit` lower, and a whole
    /// `synchronized` body compiles -- the item that stood on five pages.
    #[test]
    fn h23_monitors_lower_through_the_helpers() {
        const MON_ENTER: usize = 0x7FFF_0000_C200;
        const MON_EXIT: usize = 0x7FFF_0000_C300;
        const SET_THROW_BCI: usize = 0x7FFF_0000_9100;

        fn compile_monitors(
            code: &[u8],
            enter: usize,
            exit: usize,
            table_empty: bool,
        ) -> Arm64CompileResult {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.monitor_enter = enter;
            h.monitor_exit = exit;
            h.set_throw_bci = SET_THROW_BCI;
            b.set_helpers(h);
            b.set_method_descriptor("(Ljava/lang/Object;)V", true);
            b.set_exception_table_empty(table_empty);
            b.compile_method(2, 1, 4, code)
        }

        // aload_0; monitorenter; aload_0; monitorexit; return -- the shape
        // javac's `synchronized (o) {}` compiles to, minus the handler (which
        // is exception-table data, not bytecode).
        let code = [0x2a, 0xc2, 0x2a, 0xc3, 0xb1];

        // A NON-EMPTY table, because every real `synchronized` block has one.
        let r = compile_monitors(&code, MON_ENTER, MON_EXIT, false);
        assert!(
            r.success,
            "a synchronized body must compile with a non-empty exception table"
        );
        for (addr, what) in [(MON_ENTER, "monitor_enter"), (MON_EXIT, "monitor_exit")] {
            let at = r
                .instructions
                .iter()
                .position(
                    |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == addr as i64),
                )
                .unwrap_or_else(|| panic!("{what} must be materialized"));
            assert!(
                matches!(
                    r.instructions.get(at + 1),
                    Some(Arm64Instruction::Blr { .. })
                ),
                "{what} must be CALLED, not merely loaded"
            );
        }
        // Both sentinel edges stamp their own bci: pc 1 and pc 3.
        for bci in [1i64, 3] {
            let stamps = r
                .instructions
                .windows(3)
                .filter(|w| {
                    matches!(w[0], Arm64Instruction::MovImm { imm, .. } if imm == bci)
                        && matches!(w[1], Arm64Instruction::MovImm { imm, .. } if imm == SET_THROW_BCI as i64)
                        && matches!(w[2], Arm64Instruction::Blr { .. })
                })
                .count();
            assert_eq!(stamps, 1, "the monitor op at bci {bci} stamps its own bci");
        }
        // The frame carries the context word: both helpers take the VM
        // pointer, and without it `context_slot_offset` refuses.
        assert!(
            r.needs_context,
            "a monitor op needs the VM context in the frame"
        );

        // Each helper is required on its own: an unwired one refuses, by its
        // own arm, and says so.
        for (enter, exit) in [(0, MON_EXIT), (MON_ENTER, 0)] {
            let r = compile_monitors(&code, enter, exit, false);
            assert!(!r.success, "an unwired monitor helper must refuse");
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Comment(c) if c.contains("monitor at pc=")
                )),
                "the refusal must name the monitor arm"
            );
        }

        // With an EMPTY table no stamp is emitted, so such a body stays
        // byte-identical to what it would have been.
        let empty = compile_monitors(&code, MON_ENTER, MON_EXIT, true);
        assert!(empty.success);
        assert!(
            !empty.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == SET_THROW_BCI as i64
            )),
            "an empty table emits no stamp"
        );
    }

    /// h23c (2026-09-22): `multianewarray` lowers at ANY arity, through
    /// `multianewarray_n`, with the operand area as its dimension buffer.
    #[test]
    fn h23c_multianewarray_lowers_at_any_arity() {
        const MNA_N: usize = 0x7FFF_0000_C500;
        const SET_THROW_BCI: usize = 0x7FFF_0000_9100;

        fn compile_mna(ndims: u8, site: Option<i64>) -> Arm64CompileResult {
            // iconst_1 x ndims; multianewarray #7, ndims; areturn
            let mut code: Vec<u8> = vec![0x04; usize::from(ndims)];
            code.extend_from_slice(&[0xc5, 0x00, 0x07, ndims, 0xb0]);
            let mna_pc = usize::from(ndims);
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.multianewarray_n = MNA_N;
            h.set_throw_bci = SET_THROW_BCI;
            b.set_helpers(h);
            b.set_method_descriptor("()[[I", true);
            // NON-EMPTY: a real allocation site sits inside try/catch often
            // enough that this is the case worth pinning.
            b.set_exception_table_empty(false);
            if let Some(site) = site {
                b.set_multianewarray_site_info([(mna_pc, site)].into_iter().collect());
            }
            b.compile_method(0, 0, usize::from(ndims) + 2, &code)
        }

        // Every arity the scanner admits, not just 2 and 3.
        for ndims in 1..=crate::x64::MAX_JIT_MULTIANEWARRAY_DIMS {
            // Cast: MAX_JIT_MULTIANEWARRAY_DIMS is single-digit.
            let r = compile_mna(ndims as u8, Some(0x1234_5678));
            assert!(r.success, "ndims={ndims} must compile");
            let at = r
                .instructions
                .iter()
                .position(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { imm, .. } if *imm == MNA_N as i64
                ))
                .unwrap_or_else(|| panic!("ndims={ndims}: the helper must be materialized"));
            assert!(
                matches!(
                    r.instructions.get(at + 1),
                    Some(Arm64Instruction::Blr { .. })
                ),
                "ndims={ndims}: the helper must be CALLED"
            );
            // The arity travels as an immediate, so a site that lowered the
            // wrong count would be visible here rather than at run time.
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { imm, .. } if *imm == ndims as i64
                )),
                "ndims={ndims}: the arity must be passed"
            );
            // The dimension buffer is an address off FP -- one SUB, not a
            // copy loop.
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::SubImm { rn: Arm64Register::FP, .. }
                )),
                "ndims={ndims}: the buffer address comes off FP"
            );
            // The frame carries the context word and a safepoint id, because
            // the helper takes the VM pointer and can collect.
            assert!(r.needs_context, "ndims={ndims}: needs the VM context");
            assert_ne!(r.sp_id_slot_off, 0, "ndims={ndims}: needs a safepoint id");
            // The 0-sentinel edge stamps its own bci.
            let bci = ndims as i64;
            assert!(
                r.instructions.windows(3).any(|w| {
                    matches!(w[0], Arm64Instruction::MovImm { imm, .. } if imm == bci)
                        && matches!(w[1], Arm64Instruction::MovImm { imm, .. } if imm == SET_THROW_BCI as i64)
                        && matches!(w[2], Arm64Instruction::Blr { .. })
                }),
                "ndims={ndims}: the sentinel edge stamps the site's own bci"
            );
        }

        // An unresolved site refuses, by this arm, with a named reason.
        let r = compile_mna(3, None);
        assert!(!r.success, "an unresolved site must refuse");
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Comment(c) if c.contains("multianewarray at pc=")
            )),
            "the refusal must name the multianewarray arm"
        );

        // Wider than the scanner admits: refused rather than mis-consumed.
        let too_wide = compile_mna(
            // Cast: single-digit + 1.
            (crate::x64::MAX_JIT_MULTIANEWARRAY_DIMS + 1) as u8,
            Some(0x1234_5678),
        );
        assert!(!too_wide.success, "an arity past the cap must refuse");
    }

    /// **The gate is INERT as of h23c (2026-09-22)**, and this records the day
    /// and what would wake it.
    ///
    /// This test used to be `the_shared_memory_gate_still_has_something_to_refuse`
    /// and asserted the opposite: that some opcode in
    /// [`opcode_touches_shared_memory`] still lacked an ordered lowering. Its
    /// doc said "the day every one of them does, the gate can never fire and
    /// should be deleted rather than left as a dead branch a reader has to
    /// reason about. This test fails on that day, which is the only way anyone
    /// would notice." `multianewarray` was the last one, and it fired.
    ///
    /// # Why the gate was NOT deleted, against that note
    ///
    /// A deliberate deviation, recorded so the next person can overrule it.
    /// The two sets coinciding is a property of TODAY, not a theorem: the
    /// gate's own doc calls [`opcode_touches_shared_memory`] "an ALLOW-LIST OF
    /// THE HAZARD", so the state it defends against -- an opcode classified as
    /// touching shared memory before anyone has written its ordered lowering
    /// -- is exactly what a future wave produces on its way in. Deleting the
    /// branch buys one less thing to read and sells the interlock that makes
    /// the refusal a RULE rather than an accident, which is the distinction
    /// the sibling test
    /// (`every_shared_memory_opcode_without_an_ordered_lowering_is_refused`)
    /// exists to preserve. It costs one predicate call per opcode.
    ///
    /// So the gate stays and the assertion is inverted: the inert state is now
    /// pinned, and any opcode that joins the hazard list without a lowering
    /// fails HERE, naming itself, instead of silently re-arming a branch
    /// everyone had stopped expecting to fire.
    #[test]
    fn the_shared_memory_gate_is_inert_and_every_hazard_opcode_is_lowered() {
        let ungated: Vec<u8> = (0u8..=0xff)
            .filter(|&op| opcode_touches_shared_memory(op) && !opcode_has_ordered_lowering(op))
            .collect();
        assert!(
            ungated.is_empty(),
            "opcode(s) {ungated:02x?} are classified as touching shared memory but have \
             no ordered lowering, so the `compile_pass` gate can fire again. That is not \
             a failure in itself -- it is the state the gate exists for -- but it must be \
             a DELIBERATE one: give each of them an arm that refuses by name with its \
             ordering stated, add it to `opcode_has_ordered_lowering`, and this goes green."
        );
    }

    /// The thread-local opcodes this backend exists to compile must NOT be
    /// caught by the gate. A predicate that refused everything would pass the
    /// test above and compile nothing.
    #[test]
    fn the_shared_memory_gate_does_not_catch_thread_local_work() {
        // Constants, local load/store, arithmetic, conversions, compares,
        // branches, the stack shuffles, `iinc`, the switches and the returns.
        for op in [
            0x00u8, 0x01, 0x03, 0x10, 0x11, 0x15, 0x1a, 0x36, 0x3b, 0x57, 0x59, 0x5f, 0x60, 0x64,
            0x68, 0x78, 0x7a, 0x7e, 0x84, 0x85, 0x91, 0x94, 0x99, 0x9f, 0xa7, 0xaa, 0xab, 0xac,
            0xb1,
        ] {
            assert!(
                !opcode_touches_shared_memory(op),
                "opcode 0x{op:02x} is thread-local work; gating it would compile nothing"
            );
        }
    }

    /// The two ordering constants are claims about what this file's PRODUCTION
    /// code emits, so check them against the file.
    ///
    /// Round 9 wave 9 added the ENCODERS (`ldar`/`stlr` and friends,
    /// `ldaxr`/`stlxr`, `casal`, pinned by
    /// `aarch64::tests::test_acquire_release_and_exclusive_encodings`). An
    /// encoder is not a lowering: what these constants claim is that something
    /// above this test module CALLS them.
    ///
    /// Until round 9 wave 14 there was one constant for both halves,
    /// `ARM64_CAN_ORDER_MEMORY`, and the gate read it as a blanket permission.
    /// Splitting it is what let the acquire/release half tell the truth (it
    /// has had call sites since wave 10) without waiting on an exclusive half
    /// that cannot arrive -- see [`ARM64_LOWERS_ACQUIRE_RELEASE`].
    #[test]
    fn the_ordering_constants_agree_with_the_code() {
        let src = include_str!("aarch64.rs");
        let has = |name: &str| src.contains(&format!("pub fn {name}("));
        // A `volatile` field access needs acquire-load and release-store.
        let acquire_release = has("ldar") && has("stlr");
        // `monitorenter` needs a compare-and-swap on the lock word: either the
        // load/store-exclusive pair or the LSE single instruction.
        let exclusive = (has("ldaxr") && has("stlxr")) || has("casal");
        assert!(
            acquire_release && exclusive,
            "the acquire/release ({acquire_release}) and exclusive-access \
             ({exclusive}) encoders were added in round 9 wave 9 and must not \
             silently disappear"
        );

        // The production half of this file: everything before its test module.
        let backend = include_str!("aarch64_backend.rs");
        let decl = format!("\nmod {}", "tests {");
        let production = backend.find(&decl).map_or(backend, |at| &backend[..at]);
        let calls = |name: &str| production.contains(&format!("emitter.{name}("));
        assert_eq!(
            acquire_release && calls("ldar") && calls("stlr"),
            ARM64_LOWERS_ACQUIRE_RELEASE,
            "ARM64_LOWERS_ACQUIRE_RELEASE disagrees with the code: encoders present = \
             {acquire_release}, production calls ldar = {}, stlr = {}",
            calls("ldar"),
            calls("stlr")
        );
        assert_eq!(
            exclusive && ((calls("ldaxr") && calls("stlxr")) || calls("casal")),
            ARM64_LOWERS_EXCLUSIVE_ACCESS,
            "ARM64_LOWERS_EXCLUSIVE_ACCESS disagrees with the code: encoders present = \
             {exclusive}, production calls ldaxr = {}, stlxr = {}, casal = {}",
            calls("ldaxr"),
            calls("stlxr"),
            calls("casal")
        );
    }

    /// The gate has no blanket escape hatch any more, and this is what keeps
    /// it that way: `compile_pass`'s gate must read
    /// `opcode_has_ordered_lowering` and NOTHING else.
    ///
    /// A `bool` in front of the per-opcode list is exactly what a later change
    /// flips to unblock itself, and flipping it un-gates every heap opcode at
    /// once. The list cannot be flipped -- each entry has to be written, and
    /// `every_shared_memory_opcode_without_an_ordered_lowering_is_refused`
    /// makes the author state the arm's own refusal when they add one.
    #[test]
    fn the_gate_consults_only_the_per_opcode_list() {
        let backend = include_str!("aarch64_backend.rs");
        let decl = format!("\nmod {}", "tests {");
        let production = backend.find(&decl).map_or(backend, |at| &backend[..at]);
        assert!(
            production.contains(
                "if opcode_touches_shared_memory(opcode) && !opcode_has_ordered_lowering(opcode) {"
            ),
            "the shared-memory gate is not the exact two-term condition this test pins; \
             if it grew a third term, that term is a blanket permission and the reason \
             `ARM64_CAN_ORDER_MEMORY` was removed applies to it"
        );
    }

    /// `invokestatic` has a match arm but `emit_invoke` always fails, so no
    /// method containing a call of any kind compiles on this backend.
    #[test]
    fn invokestatic_arm_exists_but_always_bails() {
        let result = make_backend_with_method(1, 1, &[0xb8, 0x00, 0x01, 0xb1]);
        assert!(
            !result.success,
            "there is no call-target resolution on this backend; invokestatic must bail"
        );
    }

    /// A loop back-edge must resolve to a real target, not the `B .`
    /// self-branch the lazy label discovery used to leave behind.
    ///
    /// ```text
    /// 0: iconst_0        0x03        // sum = 0
    /// 1: istore_1        0x3c
    /// 2: iload_1         0x1b   <-- back-edge target
    /// 3: iconst_1        0x04
    /// 4: iadd            0x60
    /// 5: istore_1        0x3c
    /// 6: goto -4         0xa7 ff fc  // back to pc 2
    /// ```
    ///
    /// (The `goto` is unconditional, so the loop never exits.)
    ///
    /// **This method no longer compiles at all** — see
    /// `loop_method_bails_no_safepoint_poll` and [`Arm64Backend::label_for_pc`]:
    /// a back-edge is refused because there is no safepoint poll to place on it.
    /// The original assertion ("a pure-arithmetic loop must compile") is
    /// therefore inverted here.
    ///
    /// The *encoder-level* guard the original test existed for — that a
    /// backward branch resolves to a real target rather than being left as the
    /// displacement-0 `B .` placeholder — is preserved below by driving
    /// `emit_machine_code` with a hand-built pseudo-op sequence containing a
    /// bound backward branch. That is a strictly stronger check on the piece
    /// that can still regress (the patch loop), and it survives the front-end
    /// policy change.
    #[test]
    fn backward_branch_resolves_to_its_target_not_itself() {
        // Front end: the loop is now refused outright.
        let code = [0x03, 0x3c, 0x1b, 0x04, 0x60, 0x3c, 0xa7, 0xff, 0xfc];
        let result = make_backend_with_method(2, 1, &code);
        assert!(
            !result.success,
            "a loop must bail — no safepoint poll exists for its back-edge"
        );

        // Encoder: a bound backward branch must still patch to a real negative
        // displacement, never to `B .`.
        let looped = result_from_instructions(vec![
            Arm64Instruction::Label(1),
            Arm64Instruction::Nop,
            Arm64Instruction::Nop,
            Arm64Instruction::B { label: 1 },
        ]);
        let bytes = emit_machine_code(&looped).expect("bound back-edge must encode");
        assert_eq!(bytes.len(), 3 * 4, "Label emits nothing; 3 real words");

        let mut last_b: Option<(usize, u32)> = None;
        for (i, w) in bytes.chunks_exact(4).enumerate() {
            let word = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
            if word >> 26 == 0b000101 {
                last_b = Some((i, word));
            }
        }
        let (b_index, b_word) = last_b.expect("an unconditional B must be present");
        let imm26 = b_word & 0x03ff_ffff;
        assert_ne!(
            imm26, 0,
            "displacement 0 is `B .` — an infinite self-branch, the bug this guards"
        );
        // Sign-extend imm26 and confirm it points backwards, at a real
        // instruction inside the buffer.
        let signed = ((imm26 << 6) as i32) >> 6;
        assert_eq!(
            signed, -2,
            "B at word 2 targeting word 0 is a -2 displacement"
        );
        let target = (b_index as i64 + signed as i64) * 4;
        assert_eq!(target, 0, "back-edge target must be the bound label");
        assert!(
            (target as usize) < bytes.len(),
            "back-edge target must land inside the emitted buffer"
        );
    }

    /// A compiled method still carries NO oop maps -- and the reason is no
    /// longer the writer.
    ///
    /// `emit_oop_map_for_safepoint` is correct now (see
    /// `the_oop_map_pc_is_the_encoders_byte_offset`), but it still has no
    /// production call site, because this backend lowers no allocation, no call
    /// and no monitor and refuses back edges -- so a compiled method contains no
    /// GC-capable point to record a map AT. If this fires, a safepoint was
    /// added: that is the good outcome, and the module header's
    /// "Safety-critical gaps" section needs updating with it.
    #[test]
    fn compiled_methods_carry_no_oop_maps() {
        // aload_0; areturn — the reference path that *does* call
        // `mark_top_operand_as_oop`.
        let result = make_backend_with_method(1, 1, &[0x2a, 0xb0]);
        assert!(result.success);
        assert!(
            result.pending_oop_maps.is_empty(),
            "no safepoint is emitted on this backend, so nothing calls the map              writer; if this fires, a safepoint landed -- update the header"
        );
        // ...and the resolved side agrees, which is the half a GC would read.
        let (_code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
        assert!(maps.is_empty());
    }

    // -----------------------------------------------------------------------
    // aarch64 parity audit (2026-08-01) — fail-closed gates
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Safepoint polls (opt-in, `CRATONVM_JIT_ARM64_SAFEPOINTS`)
    // -----------------------------------------------------------------------

    /// Build a backend with polls on and plausible helper addresses.
    fn poll_backend() -> Arm64Backend {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(true);
        // SAFETY: plain struct of `usize` addresses.
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.safepoint_flag_addr = 0x1234_5678_9AB0;
        h.safepoint_slow_path = 0x7FFF_0000_1000;
        // The frame-base publisher, wired because a real VM wires it: precise
        // maps are default-ON, so `build_helpers` fills this slot on every
        // ordinary start. Round 9 wave 21 made it a term of
        // `fully_oop_covered`, so a poll fixture without it would be a fixture
        // that can no longer make the claim it exists to test.
        h.frame_record = 0x7FFF_0000_2000;
        b.set_helpers(h);
        b
    }

    /// THE WIDTH OF THE FLAG READ, asserted as an exact instruction word.
    ///
    /// The safepoint flag is a Rust `AtomicBool` -- ONE byte -- and the
    /// `GcBarrier` fields that follow it (`gc_generation: AtomicU64`, ...) are
    /// not zero. Reading it with the 64-bit `ldr_imm` would fold those bytes
    /// into the `CBZ` and make the poll fire on every iteration once the
    /// generation counter is nonzero. A host that cannot execute aarch64 has to
    /// catch that by ENCODING, so this pins the literal word rather than
    /// merely asserting "an Ldrb was emitted".
    #[test]
    fn the_poll_reads_one_byte_and_the_encoding_says_so() {
        use crate::aarch64::{Aarch64Emitter, Reg};
        let mut e = Aarch64Emitter::new();
        assert!(e.ldrb_imm(Reg::X17, Reg::X16, 0));
        let word = u32::from_le_bytes(e.code()[0..4].try_into().unwrap());
        // LDRB Wt, [Xn, #0] = 0x39400000 | (Rn << 5) | Rt
        assert_eq!(
            word, 0x3940_0211,
            "LDRB W17, [X16] must encode as 0x39400211; got {word:#010x}"
        );
        // The 64-bit form is a DIFFERENT instruction -- the control that makes
        // the assertion above mean something.
        let mut e64 = Aarch64Emitter::new();
        assert!(e64.ldr_imm(Reg::X17, Reg::X16, 0));
        let w64 = u32::from_le_bytes(e64.code()[0..4].try_into().unwrap());
        assert_ne!(word, w64, "byte and doubleword loads must differ");
    }

    /// The entry poll's shape, read off the pseudo-op stream.
    #[test]
    fn the_entry_poll_has_the_expected_shape() {
        let mut b = poll_backend();
        // `return void` -- no operand stack, so the poll spills nothing.
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success, "the method must still compile");
        let ops = &result.instructions;
        let ldrb = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
            .expect("the poll must read the flag with a BYTE load");
        assert!(
            matches!(ops[ldrb - 1], Arm64Instruction::MovImm { .. }),
            "the flag address must be materialized right before the load"
        );
        assert!(
            matches!(ops[ldrb + 1], Arm64Instruction::Cbz { .. }),
            "a clear flag must branch PAST the call, not into it"
        );
        assert!(
            ops[ldrb..]
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Blr { .. })),
            "the poll must call the slow path"
        );
        // The CBZ target must be bound AFTER the BLR, or the poll skips
        // nothing -- or worse, branches backwards.
        let Arm64Instruction::Cbz { label, .. } = ops[ldrb + 1] else {
            unreachable!()
        };
        let blr = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .unwrap();
        let bound = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == label))
            .expect("the skip label must be bound");
        assert!(
            bound > blr,
            "the skip target must be past the call ({bound} vs {blr})"
        );
    }

    /// OFF is byte-identical to before, and still refuses loops.
    ///
    /// The negative control for every assertion above: without it they would
    /// pass just as well if the poll were emitted unconditionally.
    #[test]
    fn safepoints_off_emits_no_poll_and_still_refuses_loops() {
        let mut off = Arm64Backend::new();
        off.set_safepoints_enabled(false);
        let a = off.compile_method(0, 0, 4, &[0xb1]);
        assert!(a.success);
        assert!(
            !a.instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Ldrb { .. })),
            "no poll may be emitted with the switch off"
        );
        // The SAME loop `a_loop_compiles_with_a_poll_at_its_header` compiles
        // with the switch on, so this is a true A/B on one input: with polls
        // off the pre-existing safety gate still refuses it, because a compiled
        // loop containing no poll is a region a stop-the-world request can
        // never interrupt.
        //
        // (An earlier draft of this used a bare `goto -3` at pc 0. That target
        // is NEGATIVE, wraps when cast to `usize`, and so never looked like a
        // back edge at all -- the test passed for the wrong reason until the
        // loop above was written to compare against.)
        let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
        let mut off2 = Arm64Backend::new();
        off2.set_safepoints_enabled(false);
        let b = off2.compile_method(1, 0, 4, &code);
        assert!(
            !b.success,
            "with polls off, a backward branch must still refuse the method"
        );
    }

    /// A helper table with no flag address emits nothing, switch or no switch.
    ///
    /// The same optional-helper contract x64 has: an unwired build must not
    /// call through a null pointer, and must be byte-identical to before.
    #[test]
    fn an_unwired_helper_table_emits_no_poll() {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(true); // switch ON, helpers absent
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert!(
            !result
                .instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Ldrb { .. })),
            "no flag address means no poll code at all"
        );
    }

    /// With polls on, a LOOP compiles -- and gets a poll at its header.
    ///
    /// This is the capability the refusal was trading away: `label_for_pc`
    /// refused every backward branch precisely because a compiled loop with no
    /// poll in it is a region a stop-the-world request can never interrupt.
    #[test]
    fn a_loop_compiles_with_a_poll_at_its_header() {
        // 0: iconst_0   1: istore_0   2: iinc 0,1   5: goto 2
        let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
        let mut b = poll_backend();
        let result = b.compile_method(1, 0, 4, &code);
        assert!(
            result.success,
            "a loop must compile once its header can carry a poll"
        );
        let polls = result
            .instructions
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
            .count();
        assert_eq!(polls, 2, "expected an entry poll and a loop-header poll");
        // And it encodes: the backward branch patches to a NEGATIVE
        // displacement, which nothing on this backend had exercised before.
        assert!(
            emit_machine_code(&result).is_some(),
            "the loop must survive encoding, back edge and all"
        );
    }

    /// The poll leaves the compile-time operand model exactly as it found it.
    ///
    /// Its stores and reloads sit INSIDE the `CBZ`-skipped block, so the model
    /// must say the same thing on both paths: every operand in the register it
    /// was in. Asserting the model is unchanged is how a non-executing host
    /// checks that.
    #[test]
    fn the_poll_restores_the_operand_model() {
        let mut b = poll_backend();
        b.install_frame(
            0,
            Arm64SpillArea {
                operands: 8,
                ..Arm64SpillArea::default()
            },
            &[],
        );
        b.operand_stack = vec![
            Operand::in_reg(OperandKind::Ref, Arm64Register::X9),
            Operand::in_reg(OperandKind::I32, Arm64Register::X10),
            Operand::in_reg(OperandKind::F64, Arm64Register::V0),
        ];
        let before = b.operand_stack.clone();

        b.emit_safepoint_poll(false);

        assert!(!b.failed, "the poll must not refuse this frame");
        assert_eq!(
            b.operand_stack, before,
            "the poll must leave the model as it found it"
        );
        // The oop map DID record the reference operand's slot: the store is
        // what makes it nameable, and the map is taken at the call.
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert_eq!(
            b.pending_oop_maps[0].frame_slot_offsets.len(),
            1,
            "only the reference operand belongs in the map"
        );
    }

    /// FLOATING-POINT OPERANDS SURVIVE THE POLL'S CALL.
    ///
    /// V0-V7 are as caller-saved as X9-X15, and the poll used to store only the
    /// GPR operand stack: a float or double live across a taken poll was
    /// whatever the slow path left in its register.
    #[test]
    fn the_poll_stores_and_reloads_floating_point_operands() {
        let mut b = poll_backend();
        b.install_frame(
            0,
            Arm64SpillArea {
                operands: 8,
                ..Arm64SpillArea::default()
            },
            &[],
        );
        b.operand_stack = vec![
            Operand::in_reg(OperandKind::F32, Arm64Register::V1),
            Operand::in_reg(OperandKind::F64, Arm64Register::V2),
        ];
        b.emit_safepoint_poll(false);
        assert!(!b.failed);

        let ops = b.buffer.instructions();
        let blr = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .expect("the poll calls the slow path");
        let stored = |vt: Arm64Register, double: bool| {
            ops[..blr].iter().any(|i| {
                matches!(i, Arm64Instruction::FpStr { vt: v, rn, is_double, .. }
                         if *v == vt && *rn == Arm64Register::FP && *is_double == double)
            })
        };
        let reloaded = |vt: Arm64Register, double: bool| {
            ops[blr..].iter().any(|i| {
                matches!(i, Arm64Instruction::FpLdr { vt: v, rn, is_double, .. }
                         if *v == vt && *rn == Arm64Register::FP && *is_double == double)
            })
        };
        assert!(
            stored(Arm64Register::V1, false),
            "the float is stored at its own width"
        );
        assert!(stored(Arm64Register::V2, true), "the double is stored");
        assert!(
            reloaded(Arm64Register::V1, false),
            "the float is reloaded after the call"
        );
        assert!(
            reloaded(Arm64Register::V2, true),
            "the double is reloaded after the call"
        );
    }

    /// A poll whose store has no reserved slot refuses the method.
    ///
    /// The alternative is a live value sitting in a caller-saved register
    /// across a CALL, which is the exact hazard the store exists for.
    #[test]
    fn a_poll_that_cannot_place_its_spill_refuses() {
        let mut b = poll_backend();
        b.install_frame(0, Arm64SpillArea::default(), &[]);
        b.operand_stack = vec![Operand::in_reg(OperandKind::Ref, Arm64Register::X9)];
        b.emit_safepoint_poll(false);
        assert!(
            b.failed,
            "a spill with nowhere to go must fail the method closed"
        );
    }

    /// The operand spill area and the frame-homed locals must NOT overlap.
    ///
    /// FIXED 2026-09-03; this failed as written.
    ///
    /// `spill_index_for` numbers a frame-homed local by how many locals BEFORE
    /// it also lack a register, so those locals occupy spill indices
    /// `0..gpr_spills`. `alloc_scratch` numbers an operand slot
    /// `frame.num_reg_locals + depth` -- and `num_reg_locals` is how many
    /// locals got a REGISTER, which is the complement of that count, not the
    /// end of it.
    ///
    /// `num_spills = gpr_spills + max_stack` is the tell: the frame is sized as
    /// if operands began at `gpr_spills`, which is exactly what makes the last
    /// operand slot the last reserved word. Basing them at `num_reg_locals`
    /// instead either ALIASES a local (fewer locals got registers) or runs PAST
    /// the reserved area into the callee-save slots (more did).
    #[test]
    fn operand_spill_slots_do_not_alias_frame_homed_locals() {
        let mut b = Arm64Backend::new();
        // Four locals: local 0 register-homed, locals 1..3 frame-homed.
        b.local_regs = vec![Some(Arm64Register::X19), None, None, None];
        b.float_local_regs = vec![None, None, None, None];
        let gpr_spills = 3usize;
        let max_stack = 4usize;
        b.frame = Some(Arm64FrameLayout::compute(
            4,
            gpr_spills + max_stack,
            &[Arm64Register::X19],
        ));

        let local_slots: Vec<usize> = (1..4).map(|i| b.spill_index_for(i)).collect();
        assert_eq!(
            local_slots,
            vec![0, 1, 2],
            "frame-homed locals occupy 0..gpr_spills"
        );

        assert_eq!(b.frame.as_ref().unwrap().num_reg_locals, 1);
        // The base both spillers now share.
        let operand_slot_0 = b.local_spill_count();
        assert_eq!(
            operand_slot_0, gpr_spills,
            "the operand area must begin where the locals end"
        );

        assert!(
            !local_slots.contains(&operand_slot_0),
            "operand depth 0 landed on spill slot {operand_slot_0}, which is \
             also a frame-homed local's slot -- the operand stack and the \
             locals share frame words"
        );
        // The OTHER direction: with more locals register-homed than not, the
        // old base ran past the reserved area into the callee-save slots,
        // overwriting a saved register the epilogue restores to the caller.
        let mut b2 = Arm64Backend::new();
        b2.local_regs = vec![
            Some(Arm64Register::X19),
            Some(Arm64Register::X20),
            Some(Arm64Register::X21),
            None,
        ];
        b2.float_local_regs = vec![None; 4];
        let saved = [Arm64Register::X19, Arm64Register::X20, Arm64Register::X21];
        let num_spills = 1 + max_stack; // gpr_spills(1) + max_stack
        b2.frame = Some(Arm64FrameLayout::compute(4, num_spills, &saved));
        let last = b2.local_spill_count() + (max_stack - 1);
        let old_last = b2.frame.as_ref().unwrap().num_reg_locals + (max_stack - 1);
        assert!(
            last < num_spills,
            "the deepest operand slot ({last}) must stay inside the reserved              area ({num_spills}); the old base put it at {old_last}"
        );
        assert!(old_last >= num_spills, "the old base really did overrun");
    }

    /// The four spill regions tile the area exactly, and each accessor refuses
    /// outside its OWN region rather than outside the grand total.
    ///
    /// That distinction is the whole point of the type (finding A11). Every one
    /// of the four accessors used to bound-check against `frame.num_spills`, so
    /// an operand depth past `max_stack` did not refuse — it silently answered
    /// the word belonging to a safepoint home, and a home index past the homes
    /// answered the safepoint-id word. A refusal at the TOTAL's edge can never
    /// catch a region mix-up, because a mix-up stays inside the total by
    /// construction.
    #[test]
    fn each_spill_region_refuses_outside_itself_not_merely_outside_the_frame() {
        let area = Arm64SpillArea {
            locals: 3,
            operands: 4,
            safepoint_homes: 2,
            sp_id: 1,
            context: 0,
        };
        assert_eq!(area.total(), 10, "the total is derived, not restated");

        // The regions tile `0..total` with no gap and no overlap: collect every
        // word each accessor can produce and check the result is exactly
        // `0..total`. A shared word shows up as a duplicate and a gap as a
        // missing index — neither is visible from a total-sized bound.
        let mut words: Vec<usize> = Vec::new();
        words.extend((0..area.locals).filter_map(|i| area.local_word(i)));
        words.extend((0..area.operands).filter_map(|d| area.operand_word(d)));
        words.extend((0..area.safepoint_homes).filter_map(|k| area.safepoint_home_word(k)));
        words.extend(area.sp_id_word());
        words.sort_unstable();
        assert_eq!(words, (0..area.total()).collect::<Vec<_>>());

        // One past each region refuses, even though every one of these indices
        // is inside the frame and would have passed the old check.
        assert_eq!(area.local_word(area.locals), None);
        assert_eq!(area.operand_word(area.operands), None);
        assert_eq!(area.safepoint_home_word(area.safepoint_homes), None);

        // A compilation that does not poll reserves neither homes nor an id
        // word, and asking for either must answer "there is none" rather than
        // the first word past the operands.
        let no_poll = Arm64SpillArea {
            locals: 1,
            operands: 2,
            safepoint_homes: 0,
            sp_id: 0,
            context: 0,
        };
        assert_eq!(no_poll.total(), 3);
        assert_eq!(no_poll.safepoint_home_word(0), None);
        assert_eq!(no_poll.sp_id_word(), None);

        // The all-zero partition is what a backend holds before its prologue
        // runs: every accessor refuses, so an offset can never be computed
        // against a frame that does not exist yet.
        let unsized_area = Arm64SpillArea::default();
        assert_eq!(unsized_area.total(), 0);
        assert_eq!(unsized_area.local_word(0), None);
        assert_eq!(unsized_area.operand_word(0), None);
    }

    /// The id word is found by NAME, not by "the last one".
    ///
    /// `sp_id_slot_off` used to be computed as `num_spills - 1`, which is the
    /// right answer only while the id word happens to be laid out last. It IS
    /// laid out last today; the point is that nothing said so, and the next
    /// region appended to the area would have silently taken the id word's
    /// offset with it.
    #[test]
    fn the_safepoint_id_word_is_named_rather_than_assumed_to_be_last() {
        let area = Arm64SpillArea {
            locals: 2,
            operands: 5,
            safepoint_homes: 3,
            sp_id: 1,
            context: 0,
        };
        assert_eq!(area.sp_id_word(), Some(10));
        assert_eq!(
            area.sp_id_word(),
            Some(area.total() - 1),
            "today it IS last — this pins the agreement rather than the guess"
        );
    }

    /// A frame for `n` locals of which `reg_homed` have registers.
    fn locals_frame(b: &mut Arm64Backend, reg: &[Option<Arm64Register>], max_stack: usize) {
        b.local_regs = reg.to_vec();
        b.float_local_regs = vec![None; reg.len()];
        let saved: Vec<Arm64Register> = reg.iter().flatten().copied().collect();
        let gpr_spills = reg.iter().filter(|r| r.is_none()).count();
        // The partition production would build for this shape: the frame-homed
        // locals, the operand area, and one safepoint home per register-homed
        // local. No safepoint-id word — these fixtures drive the poll directly
        // rather than through `compile_pass`, which is what reserves it.
        b.install_frame(
            reg.len(),
            Arm64SpillArea {
                locals: gpr_spills,
                operands: max_stack,
                safepoint_homes: saved.len(),
                sp_id: 0,
                context: 0,
            },
            &saved,
        );
    }

    /// THE POINT OF THIS WHOLE CHANGE: a REGISTER-HOMED reference local is
    /// stored to a frame home, named in the map, and reloaded after the call.
    ///
    /// X19-X28 are callee-saved, so the value survives the call on its own --
    /// but it survives inside the CALLEE's saved-register area, where only the
    /// conservative walk can see it, and a conservative walk marks without
    /// being able to REWRITE. A relocating collector therefore could not move
    /// an object whose only root was a register local. The store makes it a
    /// nameable, rewritable root; the reload is what carries a moved object's
    /// new address back into the register.
    #[test]
    fn a_register_homed_reference_local_is_spilled_named_and_reloaded() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        // The dataflow says local 0 holds a reference at this pc.
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0b1];
        b.local_oop_reached = vec![true];

        let home = b
            .safepoint_home_for_reg_local(0)
            .expect("a home must be reserved for a register-homed local");
        b.emit_safepoint_poll(false);
        assert!(!b.failed);

        let stored = b.buffer.instructions().iter().any(|i| {
            matches!(i, Arm64Instruction::Str { rt, rn, offset }
                     if *rt == Arm64Register::X19
                     && *rn == Arm64Register::FP
                     && *offset == home)
        });
        assert!(stored, "the register local must be stored to its home");

        assert_eq!(b.pending_oop_maps.len(), 1);
        let named: Vec<i16> = b.pending_oop_maps[0].frame_slot_offsets.clone();
        assert!(
            named.contains(&(home as i16)),
            "the map must name the home ({home}); named {named:?}"
        );

        let reloaded = b.buffer.instructions().iter().any(|i| {
            matches!(i, Arm64Instruction::Ldr { rt, rn, offset }
                     if *rt == Arm64Register::X19
                     && *rn == Arm64Register::FP
                     && *offset == home)
        });
        assert!(
            reloaded,
            "the register must be reloaded, or a relocated object's new address \
             never reaches the running code"
        );
    }

    /// A FRAME-HOMED reference local is named where it already lives -- no
    /// store, because there is nothing to move.
    #[test]
    fn a_frame_homed_reference_local_is_named_without_a_spill() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[None], 2);
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0b1];
        b.local_oop_reached = vec![true];

        let frame_off = b.frame.as_ref().unwrap().spill_offset; // slot 0
        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert!(
            b.pending_oop_maps[0]
                .frame_slot_offsets
                .contains(&(frame_off as i16)),
            "a frame-homed local must be named at its own slot"
        );
        assert!(
            !b.buffer.instructions().iter().any(|i| {
                matches!(i, Arm64Instruction::Str { offset, .. } if *offset == frame_off)
            }),
            "a frame-homed local is already where the GC reads it; storing it \
             again would be pure cost"
        );
    }

    /// A local the dataflow does NOT call a reference is not named.
    ///
    /// The precision control. Naming a primitive would hand a relocating
    /// collector a word to rewrite that is not a pointer -- and an `int` can
    /// coincidentally hold a value `is_object_address` accepts, which is
    /// exactly why this uses the flow-sensitive "must be oop" dataflow rather
    /// than the whole-method `find_reference_locals` approximation.
    #[test]
    fn a_non_reference_local_is_not_named() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0]; // reached, and NOT an oop
        b.local_oop_reached = vec![true];
        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        // EVERY safepoint publishes an entry now, so its id resolves; a site
        // with nothing live publishes one that NAMES nothing.
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert!(
            b.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "a primitive local must not be named"
        );
        assert_eq!(
            b.incomplete_oop_maps, 0,
            "a site with nothing live is COVERED, not incomplete"
        );
        assert!(
            !b.buffer.instructions().iter().any(|i| {
                matches!(i, Arm64Instruction::Str { rt, .. } if *rt == Arm64Register::X19)
            }),
            "a primitive local must not be spilled either"
        );
    }

    /// An UNREACHED pc makes no claim, rather than claiming "no oops".
    ///
    /// The dataflow does not reach pcs only an exception edge arrives at, and
    /// it is empty above 64 locals. Reading either as "nothing live" is how a
    /// collector loses a root, so both fall back to naming nothing and leaving
    /// the frame to the conservative scan.
    #[test]
    fn an_unreached_pc_names_nothing_rather_than_claiming_emptiness() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0b1];
        b.local_oop_reached = vec![false]; // never reached
        assert!(b.oop_locals_at_current_pc(false).is_none());
        b.emit_safepoint_poll(false);
        assert!(!b.failed, "no claim is not a refusal of the method");
        assert_eq!(b.pending_oop_maps.len(), 1, "the id must still resolve");
        assert!(b.pending_oop_maps[0].frame_slot_offsets.is_empty());
        // ...but the METHOD may not claim coverage: "could not answer" is not
        // "nothing was live", and `fully_oop_covered` switches off the
        // conservative scan that is currently covering for it.
        assert_eq!(
            b.incomplete_oop_maps, 1,
            "an unanswerable site must sink the method's coverage claim"
        );
    }

    /// The safepoint homes sit past both the locals and the operand area.
    ///
    /// Three regions share one spill area, and an overlap would have the GC
    /// read a word two of them write. The frame is extended by exactly the
    /// number of register-homed locals when polls are on, so a home is always
    /// inside the reservation and never on a local's or an operand's slot.
    #[test]
    fn safepoint_homes_do_not_collide_with_locals_or_operands() {
        let mut b = poll_backend();
        let regs = [
            Some(Arm64Register::X19),
            None,
            Some(Arm64Register::X20),
            None,
        ];
        let max_stack = 3usize;
        locals_frame(&mut b, &regs, max_stack);
        let frame_words = b.frame.as_ref().unwrap().num_spills;

        let local_slots: Vec<i32> = (0..regs.len())
            .filter(|&i| regs[i].is_none())
            .map(|i| b.spill_index_for(i) as i32)
            .collect();
        let operand_slots: Vec<i32> = (0..max_stack)
            .map(|d| (b.local_spill_count() + d) as i32)
            .collect();
        let home_slots: Vec<i32> = (0..regs.len())
            .filter(|&i| regs[i].is_some())
            .map(|i| {
                let off = b.safepoint_home_for_reg_local(i).unwrap();
                (off - b.frame.as_ref().unwrap().spill_offset) / 8
            })
            .collect();

        for h in &home_slots {
            assert!(
                !local_slots.contains(h) && !operand_slots.contains(h),
                "home slot {h} collides (locals {local_slots:?}, operands \
                 {operand_slots:?})"
            );
            assert!(
                (*h as usize) < frame_words,
                "home slot {h} is outside the reserved area ({frame_words})"
            );
        }
        assert_eq!(home_slots.len(), 2, "one home per register-homed local");
    }

    /// The ENTRY poll names the reference PARAMETERS.
    ///
    /// The prologue runs before the walk, so there is no bci to look up; the
    /// live oops there are exactly the reference parameters. Without the
    /// descriptor seed this mask is 0 and a reference parameter that is never
    /// `astore`d is never named -- covered by the conservative scan, but not
    /// precisely, which is the difference a relocating collector cares about.
    #[test]
    fn the_entry_poll_names_the_reference_parameters() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19), None], 2);
        // static (Ljava/lang/Object;I)V -> local 0 is a reference parameter.
        b.set_method_descriptor("(Ljava/lang/Object;I)V", true);
        assert_eq!(b.param_oop_mask & 1, 1, "param 0 is a reference");

        let home = b.safepoint_home_for_reg_local(0).unwrap();
        b.emit_safepoint_poll(true); // entry
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert!(
            b.pending_oop_maps[0]
                .frame_slot_offsets
                .contains(&(home as i16)),
            "the entry poll must name the reference parameter"
        );

        // The control: with no descriptor seeded, it names nothing.
        let mut b2 = poll_backend();
        locals_frame(&mut b2, &[Some(Arm64Register::X19), None], 2);
        b2.emit_safepoint_poll(true);
        assert_eq!(b2.pending_oop_maps.len(), 1, "the id still resolves");
        assert!(
            b2.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "with no descriptor seeded the parameter is not named"
        );
    }

    /// THE ENTRY POLL MUST NOT RUN WHILE THE ARGUMENTS ARE STILL IN X0-X7.
    ///
    /// `compile_pass` copies the incoming arguments into their local registers
    /// AFTER `emit_prologue` returns. The entry poll was emitted from the END
    /// of the prologue, so its `BLR` sat between the arguments arriving and
    /// being consumed -- and X0-X7 are caller-saved, so the safepoint slow path
    /// is entitled to destroy every one of them. Every parameter of every
    /// compiled method would have been garbage on the taken path.
    ///
    /// Asserted as an ORDER over the emitted stream: no call may precede the
    /// argument copy.
    #[test]
    fn the_entry_poll_runs_after_the_argument_copy() {
        let mut b = poll_backend();
        // static (Ljava/lang/Object;)I { aload_0; areturn } -- one reference
        // parameter, so there is an argument copy to be clobbered.
        b.set_method_descriptor("(Ljava/lang/Object;)Ljava/lang/Object;", true);
        let result = b.compile_method(1, 1, 4, &[0x2a, 0xb0]);
        assert!(result.success, "the method must compile");

        let ops = result.instructions;
        let first_call = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }));
        let arg_copy = ops.iter().position(|i| {
            matches!(i, Arm64Instruction::Mov { rm, .. }
                     if *rm == Arm64EntryConvention::INT_ARG_REGS[0])
        });

        if let (Some(call), Some(copy)) = (first_call, arg_copy) {
            assert!(
                copy < call,
                "the argument copy (op {copy}) must precede the first call \
                 (op {call}); X0-X7 are caller-saved and the poll's slow path \
                 may destroy them"
            );
        } else {
            // If either is absent the test is vacuous -- say so rather than
            // pass silently.
            panic!(
                "expected both an argument copy and a poll call; got copy={arg_copy:?} \
                 call={first_call:?}"
            );
        }
    }

    /// The prologue STAMPS "not yet at a safepoint" into the id slot.
    ///
    /// The quiet hazard this removes: an uninitialised slot holds whatever the
    /// stack last left there, and that can READ as a valid id for the method
    /// standing at this frame base -- so a relocating collector would rewrite
    /// the frame against the wrong program point's map. `SP_ID_UNSET_BC_PC`
    /// (`u32::MAX - 1`) matches no map, so the proof fails CLOSED. It cannot be
    /// 0, because bci 0 is a legal and very common safepoint.
    #[test]
    fn the_prologue_stamps_the_id_slot_unset() {
        let mut b = poll_backend();
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert_ne!(result.sp_id_slot_off, 0, "a slot must be reserved");
        let off = -result.sp_id_slot_off;

        let ops = &result.instructions;
        let stamp = ops.iter().position(|i| {
            matches!(i, Arm64Instruction::MovImm { imm, .. }
                     if *imm == crate::x64::safepoint::SP_ID_UNSET_BC_PC as i64)
        });
        let stamp = stamp.expect("the prologue must stamp the unset sentinel");
        assert!(
            matches!(ops[stamp + 1], Arm64Instruction::Str { rn, offset, .. }
                     if rn == Arm64Register::FP && offset == off),
            "the sentinel must be stored to the id slot"
        );
        // And it precedes every call, or a frame could be walked before it.
        if let Some(call) = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
        {
            assert!(stamp < call, "the stamp must precede any call");
        }
        assert_ne!(
            crate::x64::safepoint::SP_ID_UNSET_BC_PC,
            0,
            "0 is a legal bci and must never be the sentinel"
        );
    }

    /// Each safepoint stores ITS OWN id, and the map carries the same value.
    ///
    /// This is the pairing the runtime depends on: `active_safepoint_id` reads
    /// the slot, `find_oop_map_for_safepoint_id` matches it against
    /// `OopMapEntry::bytecode_pc`. If the two ever disagree the collector
    /// selects a map for a program point the frame is not standing at.
    #[test]
    fn the_stored_id_and_the_maps_id_are_the_same_value() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.sp_id_slot_off = 64;
        b.cur_bytecode_pc = 41;
        b.local_oop_masks = vec![0; 64];
        b.local_oop_reached = vec![true; 64];
        b.local_oop_masks[41] = 0b1;

        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert_eq!(
            b.pending_oop_maps[0].safepoint_id, 41,
            "the map must be keyed by the site's bci"
        );
        let stored = b
            .buffer
            .instructions()
            .iter()
            .any(|i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == 41));
        assert!(stored, "the site must store its own bci into the slot");
    }

    /// The ENTRY poll uses the synthetic pc, not 0.
    #[test]
    fn the_entry_poll_uses_the_synthetic_id() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.sp_id_slot_off = 64;
        b.set_method_descriptor("(Ljava/lang/Object;)V", true);
        b.emit_safepoint_poll(true);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert_eq!(
            b.pending_oop_maps[0].safepoint_id,
            crate::x64::safepoint::ENTRY_POLL_BC_PC as u32,
            "the entry poll must not reuse bci 0, which is a legal safepoint"
        );
    }

    /// The published artifact carries the slot, and the runtime's OWN reader
    /// selects the right map through it.
    ///
    /// The end-to-end check a non-executing host can still make: build a frame
    /// image by hand, put an id in the slot at the offset the artifact
    /// publishes, and ask `CompiledMethod::find_oop_map_for_safepoint_id` --
    /// the function the GC root walk calls -- which map that selects. Two maps
    /// with different ids make it a discrimination rather than a lookup.
    #[test]
    fn the_runtime_selects_a_map_through_the_published_slot() {
        let mut result = result_from_instructions(vec![
            Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234,
            },
            Arm64Instruction::Ret,
        ]);
        result.sp_id_slot_off = 24;
        result.pending_oop_maps = vec![
            Arm64PendingOopMap {
                pseudo_index: 1,
                frame_slot_offsets: vec![-16],
                safepoint_id: 41,
            },
            Arm64PendingOopMap {
                pseudo_index: 1,
                frame_slot_offsets: vec![-32],
                safepoint_id: 77,
            },
        ];

        let cm = publish_compiled_method(&result).expect("publishes");
        assert_eq!(
            cm.sp_id_slot_off, 24,
            "the artifact must carry the slot the maps are keyed through"
        );

        // A frame image: 8 words, with the id written where the artifact says.
        let mut frame = [0usize; 8];
        let base = frame.as_mut_ptr() as usize + frame.len() * 8;
        // SAFETY: writing inside our own array, at the published offset.
        unsafe {
            *((base - cm.sp_id_slot_off as usize) as *mut usize) = 77;
        }

        let selected: Vec<i16> = cm
            .oop_maps
            .iter()
            .filter(|m| m.bytecode_pc == 77)
            .flat_map(|m| m.frame_slot_offsets.clone())
            .collect();
        assert_eq!(
            selected,
            vec![-32],
            "the id in the slot must select the map for THAT site"
        );

        // The control: the other id selects the other map, so this is a
        // discrimination and not a single-map lookup that would pass anyway.
        let other: Vec<i16> = cm
            .oop_maps
            .iter()
            .filter(|m| m.bytecode_pc == 41)
            .flat_map(|m| m.frame_slot_offsets.clone())
            .collect();
        assert_eq!(other, vec![-16]);
        // And the unset sentinel selects NOTHING -- fail closed.
        assert!(cm
            .oop_maps
            .iter()
            .all(|m| m.bytecode_pc != crate::x64::safepoint::SP_ID_UNSET_BC_PC as u32));
    }

    /// The frame base is published, or the id is a number in a frame nothing
    /// can locate.
    #[test]
    fn the_frame_base_is_published_before_the_first_poll() {
        let mut b = poll_backend();
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.safepoint_flag_addr = 0x1234_5678_9AB0;
        h.safepoint_slow_path = 0x7FFF_0000_1000;
        h.frame_record = 0x7FFF_0000_2000;
        b.set_helpers(h);

        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        let ops = &result.instructions;
        let record = ops
            .iter()
            .position(
                |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == 0x7FFF_0000_2000),
            )
            .expect("the frame-record address must be materialized");
        let poll = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
            .expect("the entry poll must be emitted");
        assert!(
            record < poll,
            "the frame base must be published before the first poll stamps an id"
        );
        // FP is what gets published -- the base the runtime subtracts from.
        assert!(
            ops[..record].iter().any(|i| {
                matches!(i, Arm64Instruction::Mov { rd, rm }
                         if *rd == Arm64Register::X0 && *rm == Arm64Register::FP)
            }),
            "arg0 must be FP"
        );
    }

    /// An unwired frame-record helper emits no call, like every other optional
    /// helper here -- and, since round 9 wave 21, makes no coverage claim.
    #[test]
    fn an_unwired_frame_record_emits_nothing() {
        let mut b = poll_backend();
        // ...with the slot cleared again: `poll_backend` wires it, because a
        // real VM does and because the claim is a term of it now.
        let mut h = b.helpers;
        h.frame_record = 0;
        b.set_helpers(h);
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert!(!result.instructions.iter().any(|i| {
            matches!(i, Arm64Instruction::Mov { rd, rm }
                     if *rd == Arm64Register::X0 && *rm == Arm64Register::FP)
        }));
        assert!(!result.frame_base_published);
        assert!(
            !publish_compiled_method(&result)
                .expect("publishes")
                .fully_oop_covered,
            "maps nothing can locate are not coverage"
        );
    }

    /// An operand's oop mark belongs to the VALUE, not to its stack index.
    ///
    /// The marks once lived in a vector beside the stack that neither push nor
    /// pop maintained, so a mark outlived its value and was re-read for the
    /// next one: a primitive named as a reference (a relocating collector
    /// rewrites a non-pointer) or a reference lost. The mark is a field of the
    /// entry now, so it cannot drift; this pins that it does not.
    #[test]
    fn operand_oop_marks_track_the_value_not_the_index() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[], 4);
        b.sp_id_slot_off = 64;

        // A reference at depth 0...
        b.push_reg(OperandKind::Ref, Arm64Register::X9);
        assert!(b.operand_stack[0].oop);

        // ...consumed...
        let _ = b.pop_operand();
        b.held.clear();
        // ...and an INT pushed into the same slot.
        b.push_reg(OperandKind::I32, Arm64Register::X10);

        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1, "the id must still resolve");
        assert!(
            b.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "an int at depth 0 was named as a reference: the mark from the \
             popped value survived and was re-read for the new one. map={:?}",
            b.pending_oop_maps[0]
        );
    }

    /// `fully_oop_covered` is COMPUTED, and every term can sink it.
    ///
    /// This is the claim that lets the collector suppress its conservative scan
    /// of these frames, so the test that matters is not "it can be true" but
    /// "each thing that should make it false does".
    #[test]
    fn fully_oop_covered_is_computed_from_terms_that_can_each_sink_it() {
        // A clean method: polls on, helpers wired, no locals, one entry poll.
        let mut b = poll_backend();
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert_ne!(result.sp_id_slot_off, 0);
        assert_eq!(result.incomplete_oop_maps, 0);
        assert!(result.safepoint_count > 0, "the entry poll is a safepoint");
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            cm.fully_oop_covered,
            "a method whose every safepoint is described must be able to say so"
        );

        // (1) No id slot -> no map can be selected at all.
        let mut r1 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r1.sp_id_slot_off = 0;
        r1.safepoint_count = 1;
        r1.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 0,
            frame_slot_offsets: vec![],
            safepoint_id: 5,
        }];
        assert!(!publish_compiled_method(&r1).unwrap().fully_oop_covered);

        // (2) A safepoint that published no map -- its id cannot resolve, and
        //     "no map for this id" is indistinguishable from "not covered".
        let mut r2 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r2.sp_id_slot_off = 24;
        r2.safepoint_count = 2; // two safepoints...
        r2.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 0,
            frame_slot_offsets: vec![],
            safepoint_id: 5,
        }]; // ...one map
        assert!(!publish_compiled_method(&r2).unwrap().fully_oop_covered);

        // (3) A safepoint that could not describe what was live at it.
        let mut r3 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r3.sp_id_slot_off = 24;
        r3.safepoint_count = 1;
        r3.incomplete_oop_maps = 1;
        r3.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 0,
            frame_slot_offsets: vec![],
            safepoint_id: 5,
        }];
        assert!(!publish_compiled_method(&r3).unwrap().fully_oop_covered);

        // (4) No safepoint at all is NOT coverage -- it is a frame nothing ever
        //     observed. A vacuous true here would be the worst of the four.
        let mut r4 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r4.sp_id_slot_off = 24;
        r4.safepoint_count = 0;
        assert!(!publish_compiled_method(&r4).unwrap().fully_oop_covered);

        // (5) No published frame base (round 9 wave 21). Every term above is
        //     read at `[frame_base - something]`, and the runtime learns
        //     `frame_base` only from `emit_frame_record`. Without it the maps
        //     are unreadable, so the claim describes nothing -- which is the
        //     vacuous-proof failure mode
        //     `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`
        //     is about, arrived at from the other end.
        let mut r5 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r5.sp_id_slot_off = 24;
        r5.safepoint_count = 1;
        r5.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 0,
            frame_slot_offsets: vec![],
            safepoint_id: 5,
        }];
        r5.frame_base_published = false;
        assert!(!publish_compiled_method(&r5).unwrap().fully_oop_covered);
        // ...and the same result with the base published DOES claim it, so
        // this case is testing the one term and not some other refusal.
        r5.frame_base_published = true;
        assert!(publish_compiled_method(&r5).unwrap().fully_oop_covered);
    }

    /// With polls OFF the claim is never made, so a default build is unchanged.
    #[test]
    fn safepoints_off_never_claims_coverage() {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert_eq!(result.sp_id_slot_off, 0, "no slot is reserved");
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            !cm.fully_oop_covered,
            "the default build must keep its conservative scan"
        );
        assert!(!cm.has_precise_oop_maps());
    }

    /// A method whose dataflow cannot answer keeps the conservative scan.
    ///
    /// The end-to-end version of the `incomplete_oop_maps` term: a real
    /// compile of a method with MORE THAN 64 LOCALS, where
    /// `compute_local_oop_masks` returns nothing at all.
    #[test]
    fn a_method_the_dataflow_cannot_describe_does_not_claim_coverage() {
        let mut b = poll_backend();
        // 70 locals -- past the 64-slot mask, so `compute_local_oop_masks`
        // returns nothing at all -- AND a loop, so there is a non-entry
        // safepoint that has to consult it. The entry poll alone would not do:
        // it answers from `param_oop_mask` without touching the dataflow, and
        // for a method with no reference parameters that answer is complete.
        let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
        let result = b.compile_method(70, 0, 4, &code);
        assert!(result.success);
        assert!(
            result.incomplete_oop_maps > 0,
            "a site the dataflow cannot answer for must be counted incomplete"
        );
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            !cm.fully_oop_covered,
            "and the method must not claim coverage it cannot prove"
        );
    }

    /// The published frame layout separates the caller's saved registers from
    /// this frame's own words.
    ///
    /// A ZERO layout tells the band verifier that nothing is a register image,
    /// so the prologue's saved FP/LR pair and the caller's saved X19-X28 would
    /// count as in-band words of THIS frame. They hold the CALLER's live
    /// references, which this frame's maps have no business naming -- the
    /// oracle would report them `never_mapped` and refute the coverage claim on
    /// noise. An oracle that cries wolf is worse than one that is off.
    #[test]
    fn the_published_frame_layout_excludes_the_callers_saved_registers() {
        let mut b = poll_backend();
        let result = b.compile_method(3, 1, 4, &[0x2a, 0xb0]); // aload_0; areturn
        assert!(result.success);
        let layout = arm64_frame_layout(&result.frame);

        assert!(
            layout.callee_saved_shallow,
            "aarch64 puts the save area next to the frame pointer; saying so is \
             what stops the verifier's x86-64 half-line from swallowing the \
             whole spill area"
        );
        // The FP/LR pair is the frame record AT FP, not a word of this frame's
        // band: nothing at or above FP may be claimed.
        assert!(
            !layout.is_register_image(0),
            "[FP] is the caller's FP, outside the band"
        );
        assert!(
            !layout.is_register_image(-8),
            "[FP+8] is the return address"
        );
        // Every saved GPR is a register image.
        for (i, _) in result.frame.saved_regs.iter().enumerate() {
            let off = -(result.frame.callee_save_offset + (i as i32) * 8);
            assert!(
                layout.is_register_image(off),
                "saved register {i} at [FP-{off}] must be a register image"
            );
        }
        // The spill area is NOT a register image -- it is this frame's own
        // words, and it is where the oop maps point.
        if result.frame.num_spills > 0 {
            let deepest = -result.frame.spill_offset;
            assert!(
                !layout.is_register_image(deepest),
                "the spill area must stay visible to the verifier"
            );
            assert!(
                layout.spill_hi > layout.spill_lo,
                "the spill range must be published, or the verifier cannot tell \
                 a dead slot from a missed root"
            );
            assert!(
                deepest >= layout.spill_lo && deepest < layout.spill_hi,
                "slot 0 ({deepest}) must fall inside the published spill range \
                 [{}, {})",
                layout.spill_lo,
                layout.spill_hi
            );
        }
        // The two regions must not overlap, or a word belongs to both.
        assert!(
            layout.spill_lo >= layout.callee_saved_hi,
            "spill [{}, {}) overlaps the register images [{}, {})",
            layout.spill_lo,
            layout.spill_hi,
            layout.callee_saved_lo,
            layout.callee_saved_hi
        );
    }

    /// A published artifact carries that layout, not the all-zero default.
    #[test]
    fn a_published_artifact_carries_its_frame_layout() {
        let mut b = poll_backend();
        // aload_0; areturn -- local 0 is live, so it gets a callee-saved home.
        let result = b.compile_method(1, 1, 4, &[0x2a, 0xb0]);
        assert!(result.success);
        assert!(
            !result.frame.saved_regs.is_empty(),
            "test precondition: a saved GPR"
        );
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            cm.frame_layout.callee_saved_shallow,
            "the artifact must carry the aarch64 geometry"
        );
        let deepest_save = -result.frame.callee_save_offset;
        assert!(
            cm.frame_layout.is_register_image(deepest_save),
            "the caller's saved X19 at [FP-{deepest_save}] must be excluded from this frame's band"
        );
        assert_ne!(
            cm.frame_layout,
            cratonvm_jit_frame_layout_default(),
            "a zero layout would make the oracle report the caller's registers \
             as this frame's missed roots"
        );
    }

    fn cratonvm_jit_frame_layout_default() -> crate::FrameLayout {
        crate::FrameLayout::default()
    }
    /// A published artifact carries its resolved oop maps.
    ///
    /// The publication path used to build its `CompiledMethod` with
    /// `CompiledMethod::new(buf)` and never transfer the backend's maps -- so
    /// even a correct writer would have produced nothing a GC could read. That
    /// path sits behind `#[cfg(target_arch = "aarch64")]` and is therefore not
    /// compiled on an x86-64 host at all, which is how it stayed that way; the
    /// logic now lives in `publish_compiled_method`, which this exercises here.
    ///
    /// The buffer is allocated and finalized but never CALLED: these are aarch64
    /// bytes and the test host is x86-64. This asserts the metadata plumbing,
    /// which is the half that was broken.
    #[test]
    fn a_published_artifact_carries_its_resolved_oop_maps() {
        let mut result = result_from_instructions(vec![
            Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234_5678_9ABC,
            },
            Arm64Instruction::Ret,
        ]);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 1,
            frame_slot_offsets: vec![32],
            safepoint_id: 11,
        }];

        let expected_pc =
            emit_machine_code(&result_from_instructions(vec![Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234_5678_9ABC,
            }]))
            .expect("the prefix encodes")
            .len() as u32;

        let mut cm = publish_compiled_method(&result).expect("the artifact publishes");
        assert!(
            cm.has_precise_oop_maps(),
            "the published artifact must carry the maps -- this is the transfer              that was missing"
        );
        assert_eq!(cm.oop_maps.len(), 1);
        assert_eq!(cm.oop_maps[0].native_pc_offset, expected_pc);
        assert_eq!(cm.oop_maps[0].frame_slot_offsets, vec![32]);
        // The claim that would be false: no shadow stack, no relocation
        // support, and register oops covered only conservatively.
        assert!(!cm.oop_maps[0].moving_young_coverage_complete);
        assert!(
            !cm.fully_oop_covered,
            "`fully_oop_covered` licenses SUPPRESSING the conservative scan, and              nothing here can execute aarch64 to earn that -- the slot exists              now, the evidence does not"
        );
        // And `find_oop_map_for_pc` -- the reader that applies here, since there
        // is no safepoint-id to select by -- finds it at that PC.
        assert!(cm.find_oop_map_for_pc(expected_pc).is_some());
    }

    /// THE BUG THIS FIXES, pinned as a difference.
    ///
    /// `emit_oop_map_for_safepoint` used to key its map as
    /// `instruction_count * 4`, and the 2026-08-01 parity audit made it fail
    /// the method closed rather than let a caller inherit that, noting the fix
    /// was "to key oop maps off the *encoder's* byte offset ... then delete
    /// this guard".
    ///
    /// The stream below is built from exactly the pseudo-ops that break the old
    /// arithmetic: a `Comment` and a `Label` that emit NOTHING, a wide `MovImm`
    /// that expands to several words, and a `ConstantPoolEntry` that emits 8
    /// bytes. The map's PC must be where the encoder actually put the following
    /// instruction -- and must NOT be `index * 4`.
    #[test]
    fn the_oop_map_pc_is_the_encoders_byte_offset() {
        let prefix = vec![
            Arm64Instruction::Comment("emits nothing".to_string()),
            Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234_5678_9ABC,
            },
            Arm64Instruction::Label(1),
            Arm64Instruction::ConstantPoolEntry {
                label: 2,
                value: 0xDEAD_BEEF,
            },
        ];
        // The safepoint sits before the instruction at index 4.
        let sp_index = prefix.len() as u32;
        let mut instructions = prefix.clone();
        instructions.push(Arm64Instruction::Ret);

        // The expected byte offset, computed by ENCODING THE PREFIX rather than
        // by hardcoding any instruction's width -- so this test cannot drift
        // with the encoder.
        let expected = emit_machine_code(&result_from_instructions(prefix))
            .expect("the prefix encodes")
            .len() as u32;

        let mut result = result_from_instructions(instructions);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: sp_index,
            frame_slot_offsets: vec![16, 24],
            safepoint_id: 7,
        }];

        let (_code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
        assert_eq!(maps.len(), 1);
        assert_eq!(
            maps[0].native_pc_offset, expected,
            "the map must be keyed by the encoder's byte offset"
        );
        assert_eq!(maps[0].frame_slot_offsets, vec![16, 24]);

        // And the old arithmetic really would have been wrong here -- without
        // this the test would pass on a stream where the two happen to agree.
        assert_ne!(
            expected,
            sp_index * 4,
            "this stream must actually distinguish the encoder offset from the              pseudo-op count; pick different pseudo-ops if it stops doing so"
        );
    }

    /// The writer records a PENDING map, and refuses rather than truncate.
    #[test]
    fn the_oop_map_writer_records_a_pending_map() {
        let mut backend = Arm64Backend::new();
        assert!(!backend.failed);
        // Nothing marked as an oop yet: an empty map is not recorded at all.
        backend.emit_oop_map_for_safepoint(0);
        assert!(
            !backend.failed,
            "the writer no longer fails the method closed"
        );
        assert_eq!(
            backend.pending_oop_maps.len(),
            1,
            "every safepoint publishes an entry, so its id resolves"
        );
        assert!(
            backend.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "...and a site with no live reference names nothing"
        );
    }

    /// Fail closed on a pending map the encoder cannot place.
    ///
    /// A `pseudo_index` past the end of the stream cannot be resolved to a byte
    /// offset, and a GUESSED PC is the failure mode this whole two-phase
    /// arrangement exists to prevent -- so the method is discarded.
    #[test]
    fn an_unplaceable_oop_map_discards_the_method() {
        let mut result = result_from_instructions(vec![Arm64Instruction::Ret]);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 99,
            frame_slot_offsets: vec![8],
            safepoint_id: 3,
        }];
        assert!(
            emit_machine_code_with_oop_maps(&result).is_none(),
            "an oop map that cannot be placed must discard the method"
        );
        // The plain encoder is unaffected: it publishes no map, so an
        // unresolvable one cannot mislead anything through that path.
        assert!(emit_machine_code(&result).is_some());
    }

    /// A safepoint at the very END of the stream still resolves, via the
    /// trailing sentinel in `pseudo_offsets`.
    #[test]
    fn a_safepoint_at_the_end_of_the_stream_resolves_to_the_code_length() {
        let instructions = vec![Arm64Instruction::Ret];
        let mut result = result_from_instructions(instructions);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 1,
            frame_slot_offsets: vec![8],
            safepoint_id: 3,
        }];
        let (code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].native_pc_offset as usize, code.len());
    }

    /// A frame larger than a guard page compiles, and touches every page it
    /// crosses before SP moves.
    ///
    /// Formerly `oversized_frame_bails_no_stack_bang`: with no bang, `SUB SP,
    /// SP, #frame` could step clean past the guard page, so such frames were
    /// refused outright.
    #[test]
    fn a_large_frame_bangs_every_page_before_moving_sp() {
        // max_stack = 600 -> 600 operand words -> a 4816-byte frame.
        let mut big = Arm64Backend::new();
        big.set_safepoints_enabled(false);
        let result = big.compile_method(0, 0, 600, &[0xb1]);
        assert!(result.frame.frame_size >= 4096, "test precondition");
        assert!(
            result.success,
            "a large frame must compile once it is banged"
        );

        let below = result.frame.frame_size - 16;
        let ops = &result.instructions;
        let probes: Vec<i32> = ops
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::SubImm { rd, rn, imm }
                    if *rd == Arm64Register::X16 && *rn == Arm64Register::SP =>
                {
                    Some(*imm)
                }
                _ => None,
            })
            .collect();
        assert_eq!(Some(probes.clone()), stack_bang_probe_offsets(below));
        assert_eq!(
            probes.last().copied(),
            Some(below),
            "the exact frame bottom is probed"
        );

        let alloc = ops
            .iter()
            .position(|i| {
                matches!(i, Arm64Instruction::SubImm { rd, rn, .. }
                         if *rd == Arm64Register::SP && *rn == Arm64Register::SP)
            })
            .expect("the frame is allocated");
        let last_touch = ops
            .iter()
            .rposition(|i| {
                matches!(i, Arm64Instruction::Str { rt, rn, offset: 0 }
                         if *rt == Arm64Register::XZR && *rn == Arm64Register::X16)
            })
            .expect("each probe stores through X16");
        assert!(last_touch < alloc, "every page is touched BEFORE SP moves");
        assert!(
            emit_machine_code(&result).is_some(),
            "the 4800-byte SUB SP needs the extended-register form, which now exists"
        );

        // A frame under a page cannot skip the guard, and gets no probe.
        let mut small = Arm64Backend::new();
        small.set_safepoints_enabled(false);
        let ok = small.compile_method(0, 0, 4, &[0xb1]);
        assert!(ok.success);
        assert!(!ok.instructions.iter().any(|i| {
            matches!(i, Arm64Instruction::Str { rt, rn, .. }
                     if *rt == Arm64Register::XZR && *rn == Arm64Register::X16)
        }));
    }

    #[test]
    fn stack_bang_probe_offsets_cover_every_page_crossed() {
        assert_eq!(stack_bang_probe_offsets(0), Some(vec![]));
        assert_eq!(stack_bang_probe_offsets(4095), Some(vec![]));
        assert_eq!(stack_bang_probe_offsets(4096), Some(vec![4096]));
        assert_eq!(stack_bang_probe_offsets(8200), Some(vec![4096, 8192, 8200]));
        assert_eq!(stack_bang_probe_offsets(-1), None);
        assert_eq!(
            stack_bang_probe_offsets(4096 * (MAX_STACK_BANG_PROBES as i32 + 1)),
            None,
            "past the probe cap the method is refused"
        );
    }

    /// A wide SP adjustment lowers through the extended-register form.
    ///
    /// Formerly `addsub_imm_safe_refuses_unencodable_sp_adjustment`: the only
    /// register fallback was the shifted-register form, where 31 is XZR.
    #[test]
    fn addsub_imm_safe_lowers_a_wide_sp_adjustment() {
        use crate::aarch64::{Aarch64Emitter, Reg};

        let mut e = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(
            &mut e,
            crate::aarch64::SP,
            crate::aarch64::SP,
            5000,
            true
        ));
        let n = e.code().len();
        assert_eq!(n, 8, "MOVZ X16, #5000; SUB SP, SP, X16, UXTX");
        let last = u32::from_le_bytes(e.code()[n - 4..].try_into().unwrap());
        assert_eq!(last, 0xCB30_63FF, "sub sp, sp, x16 -- SP on both sides");

        let mut e2 = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(
            &mut e2,
            crate::aarch64::SP,
            crate::aarch64::SP,
            8192,
            true
        ));
        assert_eq!(
            e2.code().len(),
            4,
            "the shifted-12 immediate is still one word"
        );

        // The one refusal left: an operand that is IP0 itself.
        let mut e3 = Aarch64Emitter::new();
        assert!(!emit_addsub_imm_safe(
            &mut e3,
            Reg::X9,
            Reg::X16,
            5000,
            false
        ));
    }

    /// A far SP-relative access adds SP, not XZR, to the offset.
    #[test]
    fn a_far_sp_relative_access_materializes_an_sp_address() {
        let result = result_from_instructions(vec![
            Arm64Instruction::Ldr {
                rt: Arm64Register::X9,
                rn: Arm64Register::SP,
                offset: 100_000,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("encodes");
        let n = bytes.len();
        let add = u32::from_le_bytes(bytes[n - 12..n - 8].try_into().unwrap());
        assert_eq!(add, 0x8B30_63F0, "add x16, sp, x16 in the extended form");
    }

    /// `emit_machine_code` encodes a wide SP adjustment instead of refusing.
    #[test]
    fn emit_machine_code_encodes_a_wide_sp_immediate() {
        let result = result_from_instructions(vec![
            Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: 5000,
            },
            Arm64Instruction::Ret,
        ]);
        assert_eq!(
            emit_machine_code(&result).map(|b| b.len()),
            Some(12),
            "MOVZ, SUB (extended), RET"
        );

        // The shifted-immediate form still produces code too.
        let ok = result_from_instructions(vec![
            Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: 8192,
            },
            Arm64Instruction::Ret,
        ]);
        assert!(emit_machine_code(&ok).is_some());
    }
    // =====================================================================
    // The category-dependent stack shuffles.
    //
    // This backend keeps TWO simulated operand stacks — `operand_stack` for
    // int/long/reference and `float_operand_stack` for float/double — and its
    // shuffle arms popped a FIXED number of entries from the first one. Both
    // assumptions are wrong in general, and the x64 `dup2_x2` page
    // (`dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend`) raised
    // exactly this as a question it did not answer: whether that backend's
    // unconditional four-pop was live here too.
    //
    // It was, and so were four more arms. These tests read the simulated
    // stack directly after the walk, because entry COUNT is what the defect
    // was about: a category-2 value is one entry here and two JVM slots, so a
    // fixed pop either leaves the shuffle short or reaches past it into
    // entries the shuffle must not touch.
    // =====================================================================

    /// `[int, int, long, long] dup2_x2` is JVMS FORM 4 — two entries, not
    /// four. The old unconditional four-pop swallowed the two `int`s beneath
    /// and pushed a six-entry stack in the wrong order; the `int`s below the
    /// shuffle must come through untouched.
    #[test]
    fn dup2_x2_form4_leaves_the_entries_beneath_it_alone() {
        // iconst_0, iconst_1, lconst_0, lconst_1
        let prefix = [0x03u8, 0x04, 0x09, 0x0a];
        let mut control = Arm64Backend::new();
        assert!(control.compile_method(4, 0, 8, &prefix).success);
        let before = control.operand_stack.clone();
        assert_eq!(before.len(), 4, "control: four values, four entries");

        let mut backend = Arm64Backend::new();
        let mut code = prefix.to_vec();
        code.push(0x5e); // dup2_x2
        assert!(
            backend.compile_method(4, 0, 8, &code).success,
            "FORM 4 dup2_x2 must lower"
        );
        let after = &backend.operand_stack;
        assert_eq!(after.len(), 5, "[v2, v1] -> [v1, v2, v1] over two ints");
        assert_eq!(
            &after[..2],
            &before[..2],
            "the two ints below the shuffle must not move"
        );
        assert_eq!(after[3], before[2], "v2 stays in place");
        assert_eq!(after[4], before[3], "v1 stays on top");
        assert!(
            after[2] != after[4],
            "the inserted copy is a fresh register, not the original"
        );
    }

    /// `[long, int, int] dup2_x2` is FORM 3 — the copy goes three entries
    /// down, not four. The old arm popped a fourth entry that did not exist.
    #[test]
    fn dup2_x2_form3_duplicates_two_entries_over_one() {
        let prefix = [0x09u8, 0x03, 0x04]; // lconst_0, iconst_0, iconst_1
        let mut control = Arm64Backend::new();
        assert!(control.compile_method(4, 0, 8, &prefix).success);
        let before = control.operand_stack.clone();

        let mut backend = Arm64Backend::new();
        let mut code = prefix.to_vec();
        code.push(0x5e);
        assert!(
            backend.compile_method(4, 0, 8, &code).success,
            "FORM 3 dup2_x2 must lower"
        );
        let after = &backend.operand_stack;
        assert_eq!(after.len(), 5, "[v3, v2, v1] -> [v2, v1, v3, v2, v1]");
        assert_eq!(after[2], before[0], "the long stays where it was");
        assert_eq!(after[3], before[1]);
        assert_eq!(after[4], before[2]);
    }

    /// FORM 1, the only form the old arm handled: four category-1 entries.
    #[test]
    fn dup2_x2_form1_still_lowers() {
        let prefix = [0x03u8, 0x04, 0x05, 0x06]; // iconst_0..3
        let mut backend = Arm64Backend::new();
        let mut code = prefix.to_vec();
        code.push(0x5e);
        assert!(backend.compile_method(4, 0, 8, &code).success);
        assert_eq!(backend.operand_stack.len(), 6);
    }

    /// Floating-point operands shuffle like any other, and the forms the JVMS
    /// does not define still refuse.
    ///
    /// Formerly `a_floating_point_operand_refuses_the_shuffle_rather_than_moving_the_wrong_stack`.
    /// Floats lived on a second stack the shuffles never touched, so every
    /// shuffle involving one had to refuse. There is one typed stack now, and
    /// each shuffle resolves its JVMS form from the entries' categories.
    #[test]
    fn floating_point_operands_shuffle_on_the_unified_stack() {
        for (name, code, depth) in [
            (
                "dup of a float over two ints",
                vec![0x03u8, 0x04, 0x0b, 0x59],
                4,
            ),
            (
                "pop of a float over two ints",
                vec![0x03, 0x04, 0x0b, 0x57],
                2,
            ),
            ("dup2 of a double", vec![0x0e, 0x5c], 2),
            ("dup2_x2 of two doubles", vec![0x0e, 0x0f, 0x5e], 3),
            ("dup_x1 with a float below", vec![0x0b, 0x03, 0x5a], 3),
            ("swap with a float below", vec![0x0b, 0x03, 0x5f], 2),
            ("pop of a float", vec![0x0b, 0x57], 0),
        ] {
            let mut backend = Arm64Backend::new();
            assert!(
                backend.compile_method(4, 0, 8, &code).success,
                "{name} must lower"
            );
            assert_eq!(backend.operand_stack.len(), depth, "{name}");
        }

        // `swap` really exchanges the values, kinds and all.
        let mut s = Arm64Backend::new();
        assert!(s.compile_method(4, 0, 8, &[0x0b, 0x03, 0x5f]).success);
        let kinds: Vec<OperandKind> = s.operand_stack.iter().map(|o| o.kind).collect();
        assert_eq!(kinds, vec![OperandKind::I32, OperandKind::F32]);

        // `dup` of a float copies at the float's width.
        let mut d = Arm64Backend::new();
        let dup = d.compile_method(4, 0, 8, &[0x0b, 0x59]);
        assert!(dup
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovFpSingle { .. })));

        // Forms the JVMS does not define.
        for (name, code) in [
            ("dup of a double", vec![0x0eu8, 0x59]),
            ("dup_x1 over a long", vec![0x09, 0x03, 0x5a]),
            ("swap of a double", vec![0x03, 0x0e, 0x5f]),
            ("pop of a long", vec![0x09, 0x57]),
        ] {
            let mut backend = Arm64Backend::new();
            assert!(
                !backend.compile_method(4, 0, 8, &code).success,
                "{name} must refuse"
            );
        }
    }

    /// `pop2` over a single category-2 value pops ONE entry here. The old arm
    /// popped two, discarding whatever the `long` was sitting on.
    #[test]
    fn pop2_over_a_long_discards_one_entry_not_two() {
        // iconst_0, lconst_0, pop2 -> the int must survive.
        let mut backend = Arm64Backend::new();
        assert!(backend.compile_method(4, 0, 8, &[0x03, 0x09, 0x58]).success);
        assert_eq!(
            backend.operand_stack.len(),
            1,
            "pop2 of a category-2 value leaves the int beneath it"
        );
    }

    /// `dup2` over a single category-2 value duplicates ONE entry. The old arm
    /// duplicated the unrelated value beneath the `long` as well — the same
    /// miscompile x64 was fixed for.
    #[test]
    fn dup2_over_a_long_duplicates_one_entry_not_two() {
        let mut backend = Arm64Backend::new();
        assert!(backend.compile_method(4, 0, 8, &[0x03, 0x09, 0x5c]).success);
        assert_eq!(
            backend.operand_stack.len(),
            3,
            "[int, long] -> [int, long, long]"
        );
    }

    /// When the width analysis cannot type the operands, the method is
    /// refused rather than shuffled on a guess. `dup2_x2` at pc 0 has no
    /// operands at all.
    #[test]
    fn an_untypeable_shuffle_refuses_the_method() {
        let mut backend = Arm64Backend::new();
        assert!(!backend.compile_method(4, 0, 8, &[0x5e]).success);
    }

    // =====================================================================
    // 2026-09-12 review fixes. `eval_int_method` runs the integer pseudo-ops a
    // straight-line method lowers to, so VALUES are checked on a host that
    // cannot execute AArch64; the other tests pin the emitted shapes.
    // =====================================================================

    /// Run a compiled method's integer pseudo-ops from its prologue to `RET`
    /// and answer X0. Panics on any pseudo-op it does not model, so a test
    /// cannot pass by skipping one.
    fn eval_int_method(result: &Arm64CompileResult, args: &[i64]) -> i64 {
        let ops = &result.instructions;
        let mut reg = [0u64; 64];
        let mut mem: HashMap<u64, u64> = HashMap::new();
        reg[31] = 0x7FFF_0000; // SP
        for (i, a) in args.iter().enumerate() {
            reg[i] = *a as u64;
        }
        let labels: HashMap<u32, usize> = ops
            .iter()
            .enumerate()
            .filter_map(|(i, op)| match op {
                Arm64Instruction::Label(l) => Some((*l, i)),
                _ => None,
            })
            .collect();
        let w = |v: u64| v as u32;
        let at = |base: u64, off: i32| base.wrapping_add(off as i64 as u64);
        let mut pc = 0usize;
        for _ in 0..100_000 {
            let Some(op) = ops.get(pc) else {
                panic!("fell off the end of the pseudo-op stream");
            };
            let mut next = pc + 1;
            match op {
                Arm64Instruction::Label(_)
                | Arm64Instruction::Comment(_)
                | Arm64Instruction::Nop => {}
                Arm64Instruction::MovImm { rd, imm } => reg[rd.0 as usize] = *imm as u64,
                Arm64Instruction::Mov { rd, rm } => reg[rd.0 as usize] = reg[rm.0 as usize],
                Arm64Instruction::AddImm { rd, rn, imm } => {
                    reg[rd.0 as usize] = at(reg[rn.0 as usize], *imm)
                }
                Arm64Instruction::SubImm { rd, rn, imm } => {
                    reg[rd.0 as usize] = at(reg[rn.0 as usize], imm.wrapping_neg())
                }
                Arm64Instruction::AddW { rd, rn, rm } => {
                    reg[rd.0 as usize] =
                        u64::from(w(reg[rn.0 as usize]).wrapping_add(w(reg[rm.0 as usize])))
                }
                Arm64Instruction::SubW { rd, rn, rm } => {
                    reg[rd.0 as usize] =
                        u64::from(w(reg[rn.0 as usize]).wrapping_sub(w(reg[rm.0 as usize])))
                }
                Arm64Instruction::LslW { rd, rn, rm } => {
                    reg[rd.0 as usize] =
                        u64::from(w(reg[rn.0 as usize]).wrapping_shl(w(reg[rm.0 as usize])))
                }
                Arm64Instruction::AddImmW { rd, rn, imm } => {
                    reg[rd.0 as usize] = u64::from(w(reg[rn.0 as usize]).wrapping_add(*imm as u32))
                }
                Arm64Instruction::SubImmW { rd, rn, imm } => {
                    reg[rd.0 as usize] = u64::from(w(reg[rn.0 as usize]).wrapping_sub(*imm as u32))
                }
                Arm64Instruction::Sxtw { rd, rn } => {
                    reg[rd.0 as usize] = w(reg[rn.0 as usize]) as i32 as i64 as u64
                }
                Arm64Instruction::CbzW { rt, label } => {
                    if w(reg[rt.0 as usize]) == 0 {
                        next = labels[label];
                    }
                }
                Arm64Instruction::CbnzW { rt, label } => {
                    if w(reg[rt.0 as usize]) != 0 {
                        next = labels[label];
                    }
                }
                Arm64Instruction::Str { rt, rn, offset } => {
                    // Register 31 as a store's Rt is XZR.
                    let v = if rt.0 == 31 { 0 } else { reg[rt.0 as usize] };
                    mem.insert(at(reg[rn.0 as usize], *offset), v);
                }
                Arm64Instruction::Ldr { rt, rn, offset } => {
                    let addr = at(reg[rn.0 as usize], *offset);
                    reg[rt.0 as usize] = *mem
                        .get(&addr)
                        .unwrap_or_else(|| panic!("load of an unwritten word at {addr:#x}"));
                }
                Arm64Instruction::StpPre {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = at(reg[rn.0 as usize], *offset);
                    mem.insert(base, reg[rt1.0 as usize]);
                    mem.insert(base + 8, reg[rt2.0 as usize]);
                    reg[rn.0 as usize] = base;
                }
                Arm64Instruction::Stp {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = at(reg[rn.0 as usize], *offset);
                    mem.insert(base, reg[rt1.0 as usize]);
                    mem.insert(base + 8, reg[rt2.0 as usize]);
                }
                Arm64Instruction::Ldp {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = at(reg[rn.0 as usize], *offset);
                    reg[rt1.0 as usize] = mem[&base];
                    reg[rt2.0 as usize] = mem[&(base + 8)];
                }
                Arm64Instruction::LdpPost {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = reg[rn.0 as usize];
                    reg[rt1.0 as usize] = mem[&base];
                    reg[rt2.0 as usize] = mem[&(base + 8)];
                    reg[rn.0 as usize] = at(base, *offset);
                }
                Arm64Instruction::B { label } => next = labels[label],
                Arm64Instruction::Ret => return reg[0] as i64,
                other => panic!("eval_int_method does not model {other:?}"),
            }
            pc = next;
        }
        panic!("100000 steps without a RET");
    }

    /// Compile a static method with `descriptor`, polls off, and require it to
    /// succeed.
    fn compile_with(
        descriptor: &str,
        locals: usize,
        stack: usize,
        code: &[u8],
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_method_descriptor(descriptor, true);
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        let result = b.compile_method(locals, slots, stack, code);
        assert!(result.success, "{descriptor} {code:02x?} must compile");
        result
    }

    /// THE REPORTED MISCOMPILE. `static int f(int a, int b) { return a -
    /// (b+1+2+3+4); }` returned -4 for `f(100, 5)`: the round-robin scratch
    /// allocator wrapped onto a live register, spilled it, recorded the spill
    /// against the REGISTER, pushed the same register for the new value, and
    /// popping the new value reloaded the old one.
    #[test]
    fn the_expression_that_returned_minus_four_evaluates_to_85() {
        let code = [
            0x1a, 0x1b, 0x04, 0x60, 0x05, 0x60, 0x06, 0x60, 0x07, 0x60, 0x64, 0xac,
        ];
        let result = compile_with("(II)I", 2, 3, &code);
        assert_eq!(eval_int_method(&result, &[100, 5]), 85);
        assert_eq!(eval_int_method(&result, &[0, -15]), 5); // 0 - (-15 + 10)
    }

    /// A stack deeper than the seven scratch registers keeps every operand, in
    /// order -- checked with a subtraction chain, which is order-sensitive.
    #[test]
    fn a_stack_deeper_than_the_scratch_pool_keeps_every_value() {
        let ten_values = || {
            let mut code = vec![0x04u8, 0x05, 0x06, 0x07, 0x08]; // iconst_1..5
            for v in 6..=10u8 {
                code.extend_from_slice(&[0x10, v]); // bipush
            }
            code
        };
        let mut sum = ten_values();
        sum.extend(std::iter::repeat(0x60).take(9));
        sum.push(0xac);
        assert_eq!(eval_int_method(&compile_with("()I", 0, 10, &sum), &[]), 55);

        // 1 - (2 - (3 - (4 - (5 - (6 - (7 - (8 - (9 - 10))))))))
        let mut sub = ten_values();
        sub.extend(std::iter::repeat(0x64).take(9));
        sub.push(0xac);
        assert_eq!(eval_int_method(&compile_with("()I", 0, 10, &sub), &[]), -5);
    }

    /// A value on the operand stack across a branch reaches the merge from
    /// both paths: `a == 0 ? 2 : 1`. The model used to follow the walk, so the
    /// two arms left the value in different registers.
    #[test]
    fn a_ternary_value_survives_the_merge() {
        // iload_0; ifeq 8; iconst_1; goto 9; 8: iconst_2; 9: ireturn
        let code = [0x1a, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x05, 0xac];
        let result = compile_with("(I)I", 1, 1, &code);
        assert_eq!(eval_int_method(&result, &[0]), 2);
        assert_eq!(eval_int_method(&result, &[5]), 1);
    }

    /// `int` results wrap at 32 bits and stay sign-extended through `i2l`,
    /// `l2i` sign-extends, `ishl` masks, and `iinc` wraps.
    #[test]
    fn int_arithmetic_wraps_and_stays_sign_extended() {
        let min = i64::from(i32::MIN);
        let max = i64::from(i32::MAX);
        let add = compile_with("(II)I", 2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
        assert_eq!(eval_int_method(&add, &[max, 1]), min);
        // iload_0; iload_1; iadd; i2l; lreturn
        let widen = compile_with("(II)J", 2, 2, &[0x1a, 0x1b, 0x60, 0x85, 0xad]);
        assert_eq!(
            eval_int_method(&widen, &[max, 1]),
            min,
            "i2l of a wrapped int"
        );
        // lload_0; l2i; ireturn
        let narrow = compile_with("(J)I", 2, 2, &[0x1e, 0x88, 0xac]);
        assert_eq!(
            eval_int_method(&narrow, &[0xFFFF_FFFF]),
            -1,
            "(int) 0xFFFFFFFFL"
        );
        assert_eq!(eval_int_method(&narrow, &[0x1_0000_0005]), 5);
        // iload_0; iload_1; ishl; ireturn
        let shl = compile_with("(II)I", 2, 2, &[0x1a, 0x1b, 0x78, 0xac]);
        assert_eq!(
            eval_int_method(&shl, &[1, 32]),
            1,
            "the distance is masked to 5 bits"
        );
        assert_eq!(eval_int_method(&shl, &[1, 31]), min);
        // iinc 0, 1; iload_0; ireturn
        let inc = compile_with("(I)I", 1, 1, &[0x84, 0x00, 0x01, 0x1a, 0xac]);
        assert_eq!(eval_int_method(&inc, &[max]), min);
    }

    /// Every argument lands in its JVM local, category 2 included.
    ///
    /// The prologue moved argument register `i` into local `i`, which is wrong
    /// for every argument after a `long` or `double`.
    #[test]
    fn parameters_after_a_long_are_homed_by_jvm_slot() {
        // static int f(long a, long b, int c) { return c; } -- iload 4; ireturn
        let r1 = compile_with("(JJI)I", 5, 1, &[0x15, 0x04, 0xac]);
        assert_eq!(
            eval_int_method(&r1, &[11, 22, 33]),
            33,
            "c is argument 2 and local 4"
        );
        // static long f(int a, long b) { return b; } -- lload_1; lreturn
        let r2 = compile_with("(IJ)J", 3, 2, &[0x1f, 0xad]);
        assert_eq!(eval_int_method(&r2, &[7, -9]), -9);
    }

    /// A frame-homed parameter -- which every `float`/`double` one is -- is
    /// stored by the prologue and read back from that word. It used to be
    /// stored nowhere.
    #[test]
    fn floating_point_parameters_are_stored_to_their_frame_homes() {
        // static double f(double a, double b) { return b; }
        // dload_0; pop2; dload_2; dreturn
        let result = compile_with("(DD)D", 4, 2, &[0x26, 0x58, 0x28, 0xaf]);
        let ops = &result.instructions;
        let home_of = |arg: Arm64Register| {
            ops.iter().find_map(|i| match i {
                Arm64Instruction::Str { rt, rn, offset }
                    if *rt == arg && *rn == Arm64Register::FP =>
                {
                    Some(*offset)
                }
                _ => None,
            })
        };
        let a = home_of(Arm64Register::X0).expect("argument 0 is stored to its frame home");
        let b = home_of(Arm64Register::X1).expect("argument 1 is stored to its frame home");
        assert_ne!(a, b);
        let loads: Vec<i32> = ops
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::FpLdr {
                    rn,
                    offset,
                    is_double: true,
                    ..
                } if *rn == Arm64Register::FP => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(
            loads,
            vec![a, b],
            "each dload reads the word its argument was stored to"
        );
    }

    /// `freturn`/`dreturn` move the bits into X0 -- where the VM reads every
    /// result -- and leave through the shared epilogue. They used to discard
    /// the value and emit a bare `RET`, skipping the frame teardown.
    #[test]
    fn fp_returns_move_the_bits_to_x0_and_take_the_epilogue() {
        for (code, single) in [(&[0x0cu8, 0xae][..], true), (&[0x0f, 0xaf][..], false)] {
            let result = make_backend_with_method(0, 0, code);
            assert!(result.success);
            let ops = &result.instructions;
            let mv = ops
                .iter()
                .position(|i| {
                    if single {
                        matches!(i, Arm64Instruction::FmovFromFpSingle { rd, .. } if *rd == Arm64Register::X0)
                    } else {
                        matches!(i, Arm64Instruction::FmovFromFp { rd, .. } if *rd == Arm64Register::X0)
                    }
                })
                .expect("the value is moved into X0");
            assert!(
                matches!(ops[mv + 1], Arm64Instruction::B { .. }),
                "then to the epilogue"
            );
            let rets = ops
                .iter()
                .filter(|i| matches!(i, Arm64Instruction::Ret))
                .count();
            assert_eq!(rets, 1, "the only RET is the epilogue's");
            assert!(matches!(ops.last(), Some(Arm64Instruction::Ret)));
        }
    }

    /// `*cmpg` negates on MI and `*cmpl` on LT, and the NZCV truth table after
    /// `FCMP` says that puts NaN where the JVMS does. `fcmpg` used to take
    /// `B.LT`, which is TRUE on unordered, so NaN produced -1.
    #[test]
    fn fp_compares_put_nan_on_the_jvms_side() {
        type Nzcv = (bool, bool, bool, bool);
        const LESS: Nzcv = (true, false, false, false);
        const EQUAL: Nzcv = (false, true, true, false);
        const GREATER: Nzcv = (false, false, true, false);
        const UNORDERED: Nzcv = (false, false, true, true);
        fn holds(cond: Arm64Condition, (n, z, _c, v): Nzcv) -> bool {
            match cond {
                Arm64Condition::Ne => !z,
                Arm64Condition::Mi => n,
                Arm64Condition::Lt => n != v,
                other => panic!("unmodelled condition {other:?}"),
            }
        }
        // CSET ne; CNEG <cond>
        fn value(cond: Arm64Condition, flags: Nzcv) -> i32 {
            let v = i32::from(holds(Arm64Condition::Ne, flags));
            if holds(cond, flags) {
                -v
            } else {
                v
            }
        }
        for (code, nan) in [
            ([0x0bu8, 0x0c, 0x95, 0xac], -1), // fcmpl
            ([0x0b, 0x0c, 0x96, 0xac], 1),    // fcmpg
            ([0x0e, 0x0f, 0x97, 0xac], -1),   // dcmpl
            ([0x0e, 0x0f, 0x98, 0xac], 1),    // dcmpg
        ] {
            let r = make_backend_with_method(0, 0, &code);
            assert!(r.success);
            assert!(r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Cset {
                    cond: Arm64Condition::Ne,
                    ..
                }
            )));
            assert!(
                !r.instructions
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::BCond { .. })),
                "branch-free"
            );
            let cond = r
                .instructions
                .iter()
                .find_map(|i| match i {
                    Arm64Instruction::Cneg { cond, .. } => Some(*cond),
                    _ => None,
                })
                .expect("CNEG");
            assert_eq!(value(cond, UNORDERED), nan, "0x{:02x} on NaN", code[2]);
            assert_eq!(value(cond, LESS), -1);
            assert_eq!(value(cond, EQUAL), 0);
            assert_eq!(value(cond, GREATER), 1);
        }
    }

    /// `f2i`/`d2i` use the 32-bit saturating `FCVTZS W` and then sign-extend;
    /// `l2i` sign-extends instead of masking.
    #[test]
    fn fp_to_int_conversions_saturate_at_32_bits() {
        for (code, name) in [
            (&[0x0cu8, 0x8b, 0xac][..], "f2i"),
            (&[0x0f, 0x8e, 0xac][..], "d2i"),
        ] {
            let r = make_backend_with_method(0, 0, code);
            assert!(r.success);
            let ops = &r.instructions;
            let at = ops
                .iter()
                .position(|i| {
                    matches!(
                        i,
                        Arm64Instruction::FcvtzsSingle { .. } | Arm64Instruction::FcvtzsIntW { .. }
                    )
                })
                .unwrap_or_else(|| panic!("{name} must use FCVTZS W"));
            let rd = match ops[at] {
                Arm64Instruction::FcvtzsSingle { rd, .. }
                | Arm64Instruction::FcvtzsIntW { rd, .. } => rd,
                _ => unreachable!(),
            };
            assert!(
                matches!(ops[at + 1], Arm64Instruction::Sxtw { rd: d, rn: s } if d == rd && s == rd),
                "{name} then SXTW"
            );
            assert!(
                !ops.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::FcvtzsInt { .. } | Arm64Instruction::FcvtzsSingleX { .. }
                )),
                "{name} must not saturate at the 64-bit bounds"
            );
        }
        let l2i = make_backend_with_method(0, 0, &[0x0a, 0x88, 0xac]);
        assert!(l2i
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Sxtw { .. })));
        assert!(
            !l2i.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::And { .. } | Arm64Instruction::AndImm { .. }
            )),
            "l2i must not zero-extend"
        );
    }

    /// Every 32-bit int op is its W form followed by `SXTW` of its result, and
    /// int compares and zero tests read the W register.
    #[test]
    fn int_ops_lower_to_w_forms_followed_by_sxtw() {
        for op in [0x60u8, 0x64, 0x68, 0x7e, 0x80, 0x82, 0x78, 0x7a, 0x7c] {
            let r = make_backend_with_method(2, 2, &[0x1a, 0x1b, op, 0xac]);
            assert!(r.success, "0x{op:02x}");
            let ops = &r.instructions;
            let rd = ops
                .iter()
                .find_map(|i| match i {
                    Arm64Instruction::AddW { rd, .. }
                    | Arm64Instruction::SubW { rd, .. }
                    | Arm64Instruction::MulW { rd, .. }
                    | Arm64Instruction::AndW { rd, .. }
                    | Arm64Instruction::OrrW { rd, .. }
                    | Arm64Instruction::EorW { rd, .. }
                    | Arm64Instruction::LslW { rd, .. }
                    | Arm64Instruction::AsrW { rd, .. }
                    | Arm64Instruction::LsrW { rd, .. } => Some(*rd),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("0x{op:02x} has no W form"));
            assert!(
                ops.windows(2).any(|pair| matches!(
                    (&pair[0], &pair[1]),
                    (_, Arm64Instruction::Sxtw { rd: d, rn: s }) if *d == rd && *s == rd
                )),
                "0x{op:02x}: the result is sign-extended"
            );
            assert!(
                !ops.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Add { .. }
                        | Arm64Instruction::Sub { .. }
                        | Arm64Instruction::Mul { .. }
                        | Arm64Instruction::And { .. }
                        | Arm64Instruction::Orr { .. }
                        | Arm64Instruction::Eor { .. }
                        | Arm64Instruction::Lsl { .. }
                        | Arm64Instruction::Asr { .. }
                        | Arm64Instruction::Lsr { .. }
                )),
                "0x{op:02x} leaves no 64-bit op behind"
            );
        }
        // iload_0; iload_1; if_icmpeq 6; return; 6: return
        let icmp = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x9f, 0x00, 0x04, 0xb1, 0xb1]);
        assert!(icmp
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::CmpW { .. })));
        // iload_0; ifeq 5; return; 5: return
        let ifeq = make_backend_with_method(1, 1, &[0x1a, 0x99, 0x00, 0x04, 0xb1, 0xb1]);
        assert!(ifeq
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::CbzW { .. })));
    }

    /// A switch's key is never compared with itself, even with every scratch
    /// register live, and a dense `tableswitch` is a real jump table.
    ///
    /// The old lowering popped the key and then took each case constant from
    /// the scratch allocator, which could hand back the key's own register:
    /// `CMP R, R`, always equal.
    #[test]
    fn a_switch_never_compares_its_key_with_itself() {
        // seven live ints (every scratch register), then the key
        let mut prefix = vec![0x03u8; 7];
        prefix.push(0x1a);
        let mut look = prefix.clone();
        look.extend_from_slice(&[0xab, 0, 0, 0]);
        for word in [28i32, 2, 1, 28, 2, 28] {
            look.extend_from_slice(&word.to_be_bytes());
        }
        look.push(0xac);
        let mut table = prefix;
        table.extend_from_slice(&[0xaa, 0, 0, 0]);
        for word in [28i32, 0, 2, 28, 28, 28] {
            table.extend_from_slice(&word.to_be_bytes());
        }
        table.push(0xac);

        for (name, code) in [("lookupswitch", look), ("tableswitch", table)] {
            assert_eq!(code.len(), 37, "{name}: every case targets pc 36");
            let mut b = Arm64Backend::new();
            let result = b.compile_method(1, 1, 8, &code);
            assert!(result.success, "{name} must compile");
            let bytes = emit_machine_code(&result).unwrap_or_else(|| panic!("{name} must encode"));
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            for &word in &words {
                // CMP (SUBS ZR, Rn, Rm) with no shift, either width.
                if word & 0x7F20_FC1F == 0x6B00_001F {
                    assert_ne!(
                        (word >> 5) & 0x1F,
                        (word >> 16) & 0x1F,
                        "{name}: {word:#010x} compares a register with itself"
                    );
                }
            }
            if name == "tableswitch" {
                assert!(
                    words.contains(&0xB8B1_5A11),
                    "LDRSW X17, [X16, W17, UXTW #2]"
                );
                assert!(words.contains(&0xD61F_0200), "BR X16");
            }
        }
    }

    /// The prologue builds the standard AAPCS64 frame record and the epilogue
    /// tears it down, neither through `MOV` (which reads register 31 as XZR).
    #[test]
    fn the_prologue_builds_the_standard_frame_record() {
        let result = make_backend_with_method(1, 1, &[0x1a, 0xac]);
        assert!(result.success);
        let ops = &result.instructions;
        assert!(
            matches!(ops[0], Arm64Instruction::StpPre { rt1, rt2, rn, offset: -16 }
                         if rt1 == Arm64Register::FP && rt2 == Arm64Register::LR && rn == Arm64Register::SP)
        );
        assert!(
            matches!(ops[1], Arm64Instruction::AddImm { rd, rn, imm: 0 }
                     if rd == Arm64Register::FP && rn == Arm64Register::SP),
            "FP = SP right after the push: [FP] is the caller's FP and [FP+8] the LR"
        );
        assert!(!ops.iter().any(|i| matches!(i, Arm64Instruction::Mov { rd, rm }
                                             if *rd == Arm64Register::FP || *rd == Arm64Register::SP || *rm == Arm64Register::SP)));
        let ldp = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::LdpPost { .. }))
            .expect("epilogue");
        assert!(
            matches!(ops[ldp - 1], Arm64Instruction::AddImm { rd, rn, imm: 0 }
                         if rd == Arm64Register::SP && rn == Arm64Register::FP)
        );

        let bytes = emit_machine_code(&result).expect("encodes");
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(word(0), 0xA9BF_7BFD, "stp x29, x30, [sp, #-16]!");
        assert_eq!(word(1), 0x9100_03FD, "mov x29, sp (ADD form)");

        assert_eq!(
            Arm64FrameLayout::compute(0, 0, &[Arm64Register::X19]).callee_save_offset,
            -16,
            "the first callee-save pair sits directly below FP"
        );
    }

    /// `estimated_size` is an upper bound, and a label or comment costs nothing.
    #[test]
    fn code_buffer_estimated_size_is_an_upper_bound() {
        // sipush 32767; istore_0; iinc 0, 1; iload_0; ireturn
        let result = make_backend_with_method(
            1,
            0,
            &[0x11, 0x7F, 0xFF, 0x3b, 0x84, 0x00, 0x01, 0x1a, 0xac],
        );
        assert!(result.success);
        let mut buf = Arm64CodeBuffer::new();
        for inst in &result.instructions {
            buf.emit(inst.clone());
        }
        let encoded = emit_machine_code(&result).expect("encodes").len();
        assert!(
            buf.estimated_size() >= encoded,
            "{} < {encoded}",
            buf.estimated_size()
        );

        let mut labels_only = Arm64CodeBuffer::new();
        let l = labels_only.new_label();
        labels_only.bind_label(l);
        labels_only.emit(Arm64Instruction::Comment("nothing".into()));
        assert_eq!(labels_only.estimated_size(), 0);
    }

    /// More float operands than V0-V7 spill by depth, at their own width, and
    /// the method still compiles. The float allocator used to round-robin with
    /// no liveness check and no spill.
    #[test]
    fn a_float_stack_deeper_than_v0_to_v7_spills_by_depth() {
        let mut code = vec![0x0cu8; 9]; // nine fconst_1
        code.extend(std::iter::repeat(0x62).take(8)); // eight fadd
        code.push(0xae); // freturn
        let mut b = Arm64Backend::new();
        let result = b.compile_method(0, 0, 9, &code);
        assert!(
            result.success,
            "nine float operands must not exhaust the pool"
        );
        assert!(result.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::FpStr { is_double: false, rn, .. } if *rn == Arm64Register::FP
        )));
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 15: the mid-method helper call, and `putfield` on it.
    // -----------------------------------------------------------------------

    const R9W15_PUTFIELD: usize = 0x7FFF_0000_5000;

    /// Compile `code` with the NPE path and the four `jit_putfield_*` slots
    /// wired, and `field` resolved at every `getfield`/`putfield` site.
    fn compile_field(
        descriptor: &str,
        code: &[u8],
        field: Arm64InstanceField,
        npe: usize,
        putfield: usize,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.jit_npe_with_action = npe;
        h.putfield_int = putfield;
        h.putfield_long = putfield;
        h.putfield_float = putfield;
        h.putfield_double = putfield;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(true);
        let mut map = HashMap::new();
        for (pc, _op, _cp) in instance_field_sites(code) {
            map.insert(pc, field);
        }
        b.set_instance_field_info(map);
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        b.compile_method(slots, slots, 4, code)
    }

    /// A spill area with room for `operands` stack entries and nothing else.
    fn test_area(operands: usize) -> Arm64SpillArea {
        Arm64SpillArea {
            locals: 0,
            operands,
            safepoint_homes: 0,
            sp_id: 0,
            context: 0,
        }
    }

    fn int_field(index: usize, tag: u8, is_volatile: bool) -> Arm64InstanceField {
        Arm64InstanceField {
            field_index: index,
            type_tag: tag,
            is_volatile,
        }
    }

    /// The site walk is an INSTRUCTION walk: a `0xb4` byte that is some other
    /// instruction's operand is not a field access, and resolving it would
    /// hand the backend a constant-pool index that site never names.
    #[test]
    fn r9w15_instance_field_sites_walks_instructions_not_bytes() {
        // sipush 0xb400; putfield #7; return  -- the `0xb4` inside the
        // `sipush` operand must not be reported.
        let code = [0x11, 0xb4, 0x00, 0xb5, 0x00, 0x07, 0xb1];
        assert_eq!(
            instance_field_sites(&code),
            vec![(3usize, 0xb5u8, 7u16)],
            "only the real putfield is a site"
        );
    }

    /// The general call sequence: the operand stack goes to its depth slots,
    /// the arguments go to X0.., the call is through X16, and every stored
    /// operand comes back into the register the model says it is in.
    #[test]
    fn r9w15_a_helper_call_spills_the_operand_stack_and_reloads_it() {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // A frame with room for four operands. `compile_method` TAKES its
        // frame on the way out, so a test that drives the emitters directly
        // installs its own rather than compiling a throwaway method first.
        b.install_frame(0, test_area(4), &[]);
        // Two live operands, then a call.
        b.emit_iconst(11);
        b.emit_iconst(22);
        let live: Vec<Arm64Register> = b
            .operand_stack
            .iter()
            .map(|o| match o.loc {
                OperandLoc::Reg(r) => r,
                OperandLoc::Slot(_) => panic!("both operands start in registers"),
            })
            .collect();
        // The dispatch loop clears `held` at the start of every bytecode; these
        // tests drive the emitter directly, so they do it themselves.
        b.held.clear();
        let before = b.buffer.instructions().len();
        // THE CALLER allocates the result register, and allocates it BEFORE
        // the call -- see `emit_helper_call`.
        let result = b.alloc_reg(false);
        assert!(
            b.emit_helper_call(
                R9W15_PUTFIELD,
                &[Arm64HelperArg::Imm(7), Arm64HelperArg::Reg(live[0])],
                Some(result),
            ),
            "the call must be accepted"
        );
        assert!(!b.failed);
        let emitted = &b.buffer.instructions()[before..];

        let call_at = emitted
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { rn } if *rn == Arm64Register::X16))
            .expect("the call goes through X16");

        // BEFORE the call: one store per live operand, to its own depth slot.
        for (depth, reg) in live.iter().enumerate() {
            let off = b
                .spill_offset_for_depth(depth)
                .expect("every legal depth has a slot");
            assert!(
                emitted[..call_at].iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Str { rt, rn, offset }
                        if rt == reg && *rn == Arm64Register::FP && *offset == off
                )),
                "operand at depth {depth} must be stored before the call"
            );
            // AFTER it: the reload, into the SAME register.
            assert!(
                emitted[call_at..].iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Ldr { rt, rn, offset }
                        if rt == reg && *rn == Arm64Register::FP && *offset == off
                )),
                "operand at depth {depth} must be reloaded after the call"
            );
        }

        // The arguments, in X0.. order.
        assert!(
            emitted[..call_at].iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X0 && *imm == 7
            )),
            "the constant argument is materialized into X0"
        );
        assert!(
            emitted[..call_at].iter().any(|i| matches!(
                i,
                Arm64Instruction::Mov { rd, rm }
                    if *rd == Arm64Register::X1 && *rm == live[0]
            )),
            "the register argument is moved into X1"
        );
        // The result leaves X0 before the reloads can touch anything.
        let move_out = emitted[call_at..]
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::Mov { rd, rm }
                        if *rd == result && *rm == Arm64Register::X0
                )
            })
            .expect("the result is moved out of X0");
        let first_reload = emitted[call_at..]
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Ldr { .. }))
            .expect("there is a reload");
        assert!(
            move_out < first_reload,
            "the result must leave X0 before the reloads run"
        );
    }

    /// THE ORDERING RULE THE DOC CALLS OUT: the result register is allocated
    /// BEFORE the spill loop, so it can never be one of the registers the
    /// reload writes. Allocating it afterwards is the bug this pins — the
    /// reload would overwrite the helper's result with an operand, and both
    /// are legitimate values of the same width, so nothing downstream could
    /// tell.
    #[test]
    fn r9w15_a_helper_call_never_returns_a_register_the_reload_clobbers() {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.install_frame(0, test_area(8), &[]);
        // Fill the scratch pool, so the result register HAS to come from an
        // occupied one.
        for i in 0..SCRATCH_REGS.len() {
            b.emit_iconst(i32::try_from(i).expect("seven fits"));
        }
        b.held.clear();
        let before = b.buffer.instructions().len();
        // Every scratch register holds an operand, so this allocation MUST
        // spill one -- which is exactly what takes it out of the reload set.
        let result = b.alloc_reg(false);
        assert!(b.emit_helper_call(R9W15_PUTFIELD, &[], Some(result)));
        assert!(!b.failed);
        let emitted = &b.buffer.instructions()[before..];
        let call_at = emitted
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .expect("the call is emitted");
        assert!(
            !emitted[call_at..]
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Ldr { rt, .. } if *rt == result)),
            "no reload may target the result register"
        );
    }

    /// An unwired helper is a call through a null pointer, and a source
    /// register outside the scratch pool could alias an argument register.
    /// Both refuse rather than emit.
    #[test]
    fn r9w15_a_helper_call_refuses_an_unwired_address_or_a_non_scratch_source() {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.install_frame(0, test_area(4), &[]);
        assert!(!b.emit_helper_call(0, &[], None));
        assert!(b.failed, "address 0 must refuse the method");

        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.install_frame(0, test_area(4), &[]);
        assert!(!b.emit_helper_call(
            R9W15_PUTFIELD,
            &[Arm64HelperArg::Reg(Arm64Register::X0)],
            None
        ));
        assert!(b.failed, "an argument register as a SOURCE must refuse");

        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.install_frame(0, test_area(4), &[]);
        let nine = vec![Arm64HelperArg::Imm(0); 9];
        assert!(!b.emit_helper_call(R9W15_PUTFIELD, &nine, None));
        assert!(b.failed, "a ninth argument would go on the stack");

        // A destination the caller did NOT allocate -- one that still holds a
        // live operand -- is the bug the `into` contract exists to prevent:
        // the reload would overwrite the helper's result with that operand.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.install_frame(0, test_area(4), &[]);
        b.emit_iconst(1);
        let OperandLoc::Reg(live) = b.operand_stack[0].loc else {
            panic!("the constant starts in a register")
        };
        b.held.clear();
        assert!(!b.emit_helper_call(R9W15_PUTFIELD, &[], Some(live)));
        assert!(b.failed, "a destination holding a live operand must refuse");
    }

    /// `putfield` of every primitive tag: the receiver is null-checked with
    /// `npe_action::NONE`, the value is narrowed to the field's declared type,
    /// a float or double travels to the helper in a GPR, and the three
    /// arguments arrive in X0/X1/X2.
    #[test]
    fn r9w15_putfield_null_checks_narrows_and_calls_the_right_helper() {
        for (tag, descriptor, load, narrow) in [
            (b'I', "(LX;I)V", 0x1bu8, None),
            (b'B', "(LX;I)V", 0x1b, Some("sxtb")),
            (b'S', "(LX;I)V", 0x1b, Some("sxth")),
            (b'C', "(LX;I)V", 0x1b, Some("and 0xffff")),
            (b'Z', "(LX;I)V", 0x1b, Some("and 1")),
            (b'J', "(LX;J)V", 0x1f, None),
            (b'F', "(LX;F)V", 0x23, Some("fmov")),
            (b'D', "(LX;D)V", 0x27, Some("fmov")),
        ] {
            // aload_0; <load_1>; putfield #3; return
            let code = [0x2a, load, 0xb5, 0x00, 0x03, 0xb1];
            let r = compile_field(
                descriptor,
                &code,
                int_field(5, tag, false),
                R9W12_NPE,
                R9W15_PUTFIELD,
            );
            assert!(r.success, "putfield of '{}' must compile", char::from(tag));

            let cbz = r
                .instructions
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Cbz { .. }))
                .expect("the receiver is null-checked");
            let Arm64Instruction::Cbz {
                rt: obj,
                label: npe,
            } = r.instructions[cbz]
            else {
                unreachable!()
            };
            let stub = r
                .instructions
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == npe))
                .expect("the NPE label is bound");
            assert!(
                matches!(
                    r.instructions[stub + 1],
                    Arm64Instruction::MovImm { imm, .. } if imm == i64::from(npe_action::NONE)
                ),
                "'{}': JEP 358 has no field action, so the code is NONE",
                char::from(tag)
            );

            let call = r
                .instructions
                .iter()
                .position(
                    |i| matches!(i, Arm64Instruction::Blr { rn } if *rn == Arm64Register::X16),
                )
                .expect("the helper is called");
            assert!(
                cbz < call,
                "'{}': the null check precedes the call, or a null receiver \
                 silently drops the store",
                char::from(tag)
            );
            assert!(
                r.instructions[..call].iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W15_PUTFIELD as i64
                )),
                "'{}': the call target is the wired helper",
                char::from(tag)
            );
            assert!(
                r.instructions[..call].iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Mov { rd, rm } if *rd == Arm64Register::X0 && *rm == obj
                )),
                "'{}': the receiver is the first argument",
                char::from(tag)
            );
            assert!(
                r.instructions[..call].iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm } if *rd == Arm64Register::X1 && *imm == 5
                )),
                "'{}': the field index is the second argument",
                char::from(tag)
            );

            // The narrowing, and the FP bit move, between the check and the call.
            let window = &r.instructions[cbz..call];
            let found = match narrow {
                None => true,
                Some("sxtb") => window
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Sxtb { .. })),
                Some("sxth") => window
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Sxth { .. })),
                Some("and 0xffff") => window
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 0xFFFF)),
                Some("and 1") => window
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 1)),
                Some("fmov") => window.iter().any(|i| {
                    matches!(
                        i,
                        Arm64Instruction::FmovFromFp { .. }
                            | Arm64Instruction::FmovFromFpSingle { .. }
                    )
                }),
                Some(other) => panic!("unknown expectation {other}"),
            };
            assert!(
                found,
                "'{}': expected {narrow:?} between the null check and the call",
                char::from(tag)
            );
            assert!(
                emit_machine_code(&r).is_some(),
                "'{}': the body must encode",
                char::from(tag)
            );
        }
    }

    /// A `volatile` `putfield` fences on BOTH sides, and a plain one on
    /// neither. The leading fence is the half x64 does not owe: there the
    /// store is a plain `MOV` that TSO already makes a release, while here it
    /// happens inside a helper as a relaxed store and a `BLR` orders nothing.
    #[test]
    fn r9w15_a_volatile_putfield_fences_on_both_sides() {
        let code = [0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1];
        for is_volatile in [false, true] {
            let r = compile_field(
                "(LX;I)V",
                &code,
                int_field(1, b'I', is_volatile),
                R9W12_NPE,
                R9W15_PUTFIELD,
            );
            assert!(r.success);
            let call = r
                .instructions
                .iter()
                .position(
                    |i| matches!(i, Arm64Instruction::Blr { rn } if *rn == Arm64Register::X16),
                )
                .expect("the helper is called");
            let before = r.instructions[..call]
                .iter()
                .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
                .count();
            let after = r.instructions[call..]
                .iter()
                .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
                .count();
            if is_volatile {
                assert_eq!(before, 1, "the release edge is a leading DMB ISH");
                assert_eq!(after, 1, "the StoreLoad edge is a trailing DMB ISH");
            } else {
                assert_eq!(
                    before + after,
                    0,
                    "a plain field owes no fence -- the JMM says nothing about it"
                );
            }
        }
    }

    /// A REFERENCE field refuses the method. Its store owes the SATB
    /// pre-barrier and the card mark that only `jit_putfield_object` runs, and
    /// neither is emitted here; compiling it through `jit_putfield_int` would
    /// write a `Value::Int` over a live reference.
    #[test]
    fn r9w15_putfield_of_a_reference_refuses_the_method() {
        for &tag in b"L[" {
            let code = [0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1];
            let r = compile_field(
                "(LX;LY;)V",
                &code,
                int_field(1, tag, false),
                R9W12_NPE,
                R9W15_PUTFIELD,
            );
            assert!(
                !r.success,
                "a '{}' field must refuse: no SATB pre-barrier, no card mark",
                char::from(tag)
            );
        }
    }

    /// Every "not wired" refusal. An unresolved site, no NPE path, and an
    /// unwired `jit_putfield_*` each keep the pre-wave behaviour.
    #[test]
    fn r9w15_putfield_refuses_every_unwired_shape() {
        let code = [0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1];
        // No resolved site: the map is empty.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.jit_npe_with_action = R9W12_NPE;
        h.putfield_int = R9W15_PUTFIELD;
        b.set_helpers(h);
        b.set_method_descriptor("(LX;I)V", true);
        b.set_exception_table_empty(true);
        let r = b.compile_method(2, 2, 4, &code);
        assert!(!r.success, "an unresolved site must refuse");

        // No exact NPE path (a non-empty exception table).
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_helpers(h);
        b.set_method_descriptor("(LX;I)V", true);
        b.set_exception_table_empty(false);
        let mut map = HashMap::new();
        map.insert(2usize, int_field(1, b'I', false));
        b.set_instance_field_info(map.clone());
        let r = b.compile_method(2, 2, 4, &code);
        assert!(
            !r.success,
            "without an exact NPE path the store would silently vanish on null"
        );

        // No helper.
        let r = compile_field("(LX;I)V", &code, int_field(1, b'I', false), R9W12_NPE, 0);
        assert!(!r.success, "an unwired jit_putfield_* must refuse");
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 16: the context ABI, and `getfield` on it.
    // -----------------------------------------------------------------------

    const R9W16_GETFIELD: usize = 0x7FFF_0000_6000;
    const R9W16_DISPATCH_THREW: usize = 0x7FFF_0000_7000;

    /// Compile `code` with the NPE path, `jit_getfield` and `dispatch_threw`
    /// wired, and `field` resolved at every instance-field site.
    fn compile_getfield(
        descriptor: &str,
        code: &[u8],
        field: Arm64InstanceField,
        getfield: usize,
        dispatch_threw: usize,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.jit_npe_with_action = R9W12_NPE;
        h.getfield = getfield;
        h.dispatch_threw = dispatch_threw;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(true);
        let mut map = HashMap::new();
        for (pc, _op, _cp) in instance_field_sites(code) {
            map.insert(pc, field);
        }
        b.set_instance_field_info(map);
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        b.compile_method(slots.max(2), slots, 4, code)
    }

    /// THE CONTEXT OCCUPIES X0. A body that takes one homes it to its frame
    /// word in the prologue and reads its first Java argument from X1; a body
    /// that does not is byte-identical to before this wave.
    #[test]
    fn r9w16_the_context_is_homed_and_shifts_every_argument() {
        // aload_0; getfield #3; ireturn  -- needs a context.
        let with = compile_getfield(
            "(LX;)I",
            &[0x2a, 0xb4, 0x00, 0x03, 0xac],
            int_field(1, b'I', false),
            R9W16_GETFIELD,
            R9W16_DISPATCH_THREW,
        );
        assert!(with.success, "a primitive getfield must compile");
        assert!(
            with.needs_context,
            "the artifact must declare the ABI its prologue was built for"
        );
        // The context store is the FIRST thing to touch X0.
        let ctx_store = with
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::Str { rt, rn, .. }
                        if *rt == Arm64Register::X0 && *rn == Arm64Register::FP
                )
            })
            .expect("the context is homed");
        // ...and the receiver arrives in X1, not X0.
        let homed = with.instructions.iter().position(|i| {
            matches!(
                i,
                Arm64Instruction::Mov { rm, .. } | Arm64Instruction::Str { rt: rm, .. }
                    if *rm == Arm64Register::X1
            )
        });
        assert!(
            homed.is_some_and(|at| at > ctx_store),
            "argument 0 homes out of X1, after the context store: {:?}",
            with.instructions
        );
        assert!(
            !with.instructions[ctx_store + 1..]
                .iter()
                .take_while(|i| !matches!(i, Arm64Instruction::Blr { .. }))
                .any(|i| matches!(
                    i,
                    Arm64Instruction::Mov { rm, .. } if *rm == Arm64Register::X0
                )),
            "nothing may home a Java argument out of X0 when a context is there"
        );

        // A method with no `getfield` takes no context and homes out of X0.
        let without = compile_getfield(
            "(I)I",
            &[0x1a, 0xac],
            int_field(1, b'I', false),
            R9W16_GETFIELD,
            R9W16_DISPATCH_THREW,
        );
        assert!(without.success);
        assert!(
            !without.needs_context,
            "a method with no getfield must keep the plain entry"
        );
        assert!(
            !without.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Str { rt, rn, .. }
                    if *rt == Arm64Register::X0 && *rn == Arm64Register::FP
            )),
            "and reserve no context word"
        );
    }

    /// The frame word: `Arm64SpillArea` puts the context LAST, so adding it
    /// moved nothing, and its accessor refuses when none is reserved.
    #[test]
    fn r9w16_the_context_word_is_last_and_absent_when_unused() {
        let none = Arm64SpillArea {
            locals: 2,
            operands: 3,
            safepoint_homes: 1,
            sp_id: 1,
            context: 0,
        };
        let some = Arm64SpillArea { context: 1, ..none };
        assert_eq!(none.context_word(), None);
        assert_eq!(some.context_word(), Some(2 + 3 + 1 + 1));
        assert_eq!(none.total() + 1, some.total());
        // Every other region is unmoved.
        for a in [none, some] {
            assert_eq!(a.local_word(1), Some(1));
            assert_eq!(a.operand_word(0), Some(2));
            assert_eq!(a.safepoint_home_word(0), Some(5));
            assert_eq!(a.sp_id_word(), Some(6));
        }
    }

    /// `getfield` of every primitive tag: the context comes out of its frame
    /// word, the three arguments arrive in X0/X1/X2, and the result is put in
    /// this backend's canonical form for its kind.
    #[test]
    fn r9w16_getfield_loads_the_context_and_calls_the_helper() {
        for (tag, ret, canonical) in [
            (b'I', 0xacu8, "sxtw"),
            (b'B', 0xac, "sxtw"),
            (b'C', 0xac, "sxtw"),
            (b'J', 0xad, "none"),
            (b'F', 0xae, "fmov"),
            (b'D', 0xaf, "fmov"),
        ] {
            // aload_0; getfield #3; <return>
            let code = [0x2a, 0xb4, 0x00, 0x03, ret];
            let r = compile_getfield(
                "(LX;)V",
                &code,
                int_field(6, tag, false),
                R9W16_GETFIELD,
                R9W16_DISPATCH_THREW,
            );
            assert!(r.success, "getfield of '{}' must compile", char::from(tag));

            let call = r
                .instructions
                .iter()
                .position(|i| {
                    matches!(
                        i,
                        Arm64Instruction::MovImm { rd, imm }
                            if *rd == Arm64Register::X16 && *imm == R9W16_GETFIELD as i64
                    )
                })
                .expect("the getfield helper is called");
            let before = &r.instructions[..call];
            assert!(
                before
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Cbz { .. })),
                "'{}': the receiver is null-checked first",
                char::from(tag)
            );
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
                )),
                "'{}': the context is read out of its frame word",
                char::from(tag)
            );
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X2 && *imm == 6
                )),
                "'{}': the field index is the third argument",
                char::from(tag)
            );

            // The sentinel check, on every tag.
            let after = &r.instructions[call..];
            assert!(
                after.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { imm, .. } if *imm == i64::MIN
                )) && after
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
                "'{}': the i64::MIN sentinel must be tested",
                char::from(tag)
            );

            let canonicalized = match canonical {
                "sxtw" => after
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Sxtw { .. })),
                "fmov" => after.iter().any(|i| {
                    matches!(
                        i,
                        Arm64Instruction::FmovToFp { .. } | Arm64Instruction::FmovToFpSingle { .. }
                    )
                }),
                _ => true,
            };
            assert!(
                canonicalized,
                "'{}': the result must reach this backend's canonical form",
                char::from(tag)
            );
            assert!(
                emit_machine_code(&r).is_some(),
                "'{}': the body must encode",
                char::from(tag)
            );
        }
    }

    /// ONLY `J` and `D` pay for the sentinel ambiguity. A `long` field holding
    /// `Long.MIN_VALUE` and a `double` field holding `-0.0` both return
    /// exactly `i64::MIN`, so those two must ask `dispatch_threw` whether the
    /// signal is real; for every other primitive tag `i64::MIN` cannot be a
    /// value, so the second call would be pure cost.
    #[test]
    fn r9w16_only_long_and_double_ask_dispatch_threw() {
        for (tag, ret, wants) in [
            (b'I', 0xacu8, false),
            (b'F', 0xae, false),
            (b'S', 0xac, false),
            (b'J', 0xad, true),
            (b'D', 0xaf, true),
        ] {
            let code = [0x2a, 0xb4, 0x00, 0x03, ret];
            let r = compile_getfield(
                "(LX;)V",
                &code,
                int_field(0, tag, false),
                R9W16_GETFIELD,
                R9W16_DISPATCH_THREW,
            );
            assert!(r.success);
            let asks = r.instructions.iter().any(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W16_DISPATCH_THREW as i64
                )
            });
            assert_eq!(
                asks,
                wants,
                "'{}': dispatch_threw is owed exactly when i64::MIN is a legal value",
                char::from(tag)
            );
            // ...and when it IS owed, an unwired helper refuses the method
            // rather than reading Long.MIN_VALUE as a pending exception.
            if wants {
                let unwired =
                    compile_getfield("(LX;)V", &code, int_field(0, tag, false), R9W16_GETFIELD, 0);
                assert!(
                    !unwired.success,
                    "'{}': without dispatch_threw the ambiguity cannot be resolved",
                    char::from(tag)
                );
            }
        }
    }

    /// Every "not wired" refusal for `getfield`.
    ///
    /// h23 (2026-09-22): a REFERENCE field is no longer one of them -- see
    /// `h23_getfield_lowers_a_reference_field_as_an_oop`. An unrecognised
    /// type tag still refuses through the same arm.
    #[test]
    fn r9w16_getfield_refuses_every_unwired_shape() {
        let code = [0x2a, 0xb4, 0x00, 0x03, 0xac];
        // A tag no descriptor produces.
        let r = compile_getfield(
            "(LX;)I",
            &code,
            int_field(0, b'Q', false),
            R9W16_GETFIELD,
            R9W16_DISPATCH_THREW,
        );
        assert!(!r.success, "an unrecognised type tag must refuse");
        // An unwired `jit_getfield`.
        let r = compile_getfield(
            "(LX;)I",
            &code,
            int_field(0, b'I', false),
            0,
            R9W16_DISPATCH_THREW,
        );
        assert!(!r.success, "an unwired jit_getfield must refuse");

        // No exact NPE path.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.getfield = R9W16_GETFIELD;
        h.dispatch_threw = R9W16_DISPATCH_THREW;
        b.set_helpers(h);
        b.set_method_descriptor("(LX;)I", true);
        b.set_exception_table_empty(false);
        let mut map = HashMap::new();
        map.insert(1usize, int_field(0, b'I', false));
        b.set_instance_field_info(map);
        let r = b.compile_method(2, 1, 4, &code);
        assert!(!r.success, "without an exact NPE path getfield must refuse");
    }

    /// h23 (2026-09-22): a REFERENCE `getfield` lowers, through the helper
    /// that has always supported one, and lands on the stack as an oop.
    ///
    /// The three things this pins:
    ///
    /// * it COMPILES, for both `L` and `[`, with `dispatch_threw` UNWIRED --
    ///   a reference is never `i64::MIN`, so it owes no second call, and
    ///   requiring one would have been a refusal in disguise;
    /// * the helper is told to expect a reference
    ///   (`GETFIELD_EXPECT_REFERENCE` in its third argument, built by the one
    ///   canonical encoder). Without that bit the helper hands back the
    ///   payload of whatever `Value` variant the slot holds, and this arm
    ///   pushes it as an oop -- a wild pointer in the collector's hands;
    /// * the result is marked as an oop on the operand model.
    #[test]
    fn h23_getfield_lowers_a_reference_field_as_an_oop() {
        for &tag in b"L[" {
            let r = compile_getfield(
                "(LX;)Ljava/lang/Object;",
                &[0x2a, 0xb4, 0x00, 0x03, 0xb0],
                int_field(5, tag, false),
                R9W16_GETFIELD,
                // Deliberately UNWIRED: a reference needs no probe.
                0,
            );
            let what = format!("tag={}", char::from(tag));
            assert!(r.success, "{what}: a reference getfield must compile");
            // The third argument carries the slot index AND the flag.
            let want = cratonvm_jit_api::getfield_index_arg(5, true, false, 0) as i64;
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { imm, .. } if *imm == want
                )),
                "{what}: the encoded index {want:#x} must be materialized"
            );
            // And it is NOT the bare slot index, which is what this site
            // used to build by hand.
            assert!(
                !r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { imm: 5, .. }
                )),
                "{what}: the bare slot index must not be passed"
            );
        }

        // The KIND, proved by what the next instruction is allowed to do with
        // it: `astore_1` pops a `Ref` and fails the compile against anything
        // else, so a body that stores the result into a reference local and
        // returns it compiles only if the push was a reference.
        // aload_0; getfield #3; astore_1; aload_1; areturn
        let stored = compile_getfield(
            "(LX;)Ljava/lang/Object;",
            &[0x2a, 0xb4, 0x00, 0x03, 0x4c, 0x2b, 0xb0],
            int_field(5, b'L', false),
            R9W16_GETFIELD,
            0,
        );
        assert!(
            stored.success,
            "the result must be storable into a reference local"
        );
        // And the MARK follows from the kind by construction, which is the
        // property the arm relies on rather than re-establishes.
        assert!(
            Operand::in_reg(OperandKind::Ref, Arm64Register::X9).oop,
            "a Ref entry is an oop entry"
        );
    }

    /// h23 (2026-09-22): `getfield`'s HELPER-sentinel edge stamps its
    /// throw-site bci, like every other trapping lowering on this backend.
    ///
    /// This edge was missed when `can_throw_npe` was widened from
    /// `exception_table_empty` to `can_route_exception` earlier the same day.
    /// The inline null check leaves through the bci-keyed NPE stub and was
    /// always fine; this one -- the helper's own `i64::MIN`, for a receiver
    /// the inline check passed -- left with whatever bci a previous stamp had
    /// written. In a method with a `try`/`catch` that is the wrong-handler
    /// bug the stamp exists to prevent.
    #[test]
    fn h23_getfield_stamps_its_throw_bci_on_the_sentinel_edge() {
        const SET_THROW_BCI: usize = 0x7FFF_0000_9100;
        // aload_0; getfield #3 at pc 1; ireturn.
        let code = [0x2a, 0xb4, 0x00, 0x03, 0xac];
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.jit_npe_with_action = R9W12_NPE;
        h.getfield = R9W16_GETFIELD;
        h.set_throw_bci = SET_THROW_BCI;
        b.set_helpers(h);
        b.set_method_descriptor("(LX;)I", true);
        // A NON-EMPTY table: the whole point is that the bci now matters.
        b.set_exception_table_empty(false);
        let mut map = HashMap::new();
        map.insert(1usize, int_field(0, b'I', false));
        b.set_instance_field_info(map);
        let r = b.compile_method(2, 1, 4, &code);
        assert!(
            r.success,
            "with set_throw_bci wired a non-empty table no longer refuses"
        );
        // The stamp: the bci, then the helper address, then a BLR.
        let at = r
            .instructions
            .iter()
            .position(|i| matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == SET_THROW_BCI as i64
            ))
            .expect("the set_throw_bci address is materialized");
        assert!(
            matches!(r.instructions.get(at + 1), Some(Arm64Instruction::Blr { .. })),
            "the stamp call follows its address: {:?}",
            r.instructions.get(at + 1)
        );
        assert!(
            matches!(
                r.instructions.get(at - 1),
                Some(Arm64Instruction::MovImm { imm: 1, .. })
            ),
            "the bci stamped is the getfield's own pc: {:?}",
            r.instructions.get(at - 1)
        );
        // With an EMPTY table the stamp is not emitted at all, so an
        // exception-table-free method stays byte-identical to before h23.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_helpers(h);
        b.set_method_descriptor("(LX;)I", true);
        b.set_exception_table_empty(true);
        let mut map = HashMap::new();
        map.insert(1usize, int_field(0, b'I', false));
        b.set_instance_field_info(map);
        let empty = b.compile_method(2, 1, 4, &code);
        assert!(empty.success);
        assert!(
            !empty.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == SET_THROW_BCI as i64
            )),
            "an empty table emits no stamp"
        );
    }

    /// The operand-stack kind analysis is told what each instance field holds.
    /// Without that its `0xb4` arm answers `None` from the first `getfield`
    /// onwards, so every pc after one is undescribed and `restore_stack_at`
    /// cannot rebuild the model at a branch target past it.
    #[test]
    fn r9w16_the_kind_analysis_is_told_the_field_types() {
        // aload_0; getfield #3 (a long); lconst_0; lcmp; ifeq +5; iconst_1;
        // ireturn; iconst_0; ireturn -- a branch target AFTER the getfield.
        let code = [
            0x2a, 0xb4, 0x00, 0x03, 0x09, 0x94, 0x99, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac,
        ];
        let r = compile_getfield(
            "(LX;)I",
            &code,
            int_field(0, b'J', false),
            R9W16_GETFIELD,
            R9W16_DISPATCH_THREW,
        );
        assert!(
            r.success,
            "a branch after a getfield must still compile: {:?}",
            r.instructions
        );
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 17: `putstatic` through its helper.
    // -----------------------------------------------------------------------

    const R9W17_PUTSTATIC: usize = 0x7FFF_0000_8000;

    /// Compile `code` with the four `jit_putstatic_*` slots wired and `field`
    /// resolved at every static site.
    fn compile_putstatic(
        descriptor: &str,
        locals: usize,
        code: &[u8],
        field: Arm64StaticField,
        putstatic: usize,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.putstatic_int = putstatic;
        h.putstatic_long = putstatic;
        h.putstatic_float = putstatic;
        h.putstatic_double = putstatic;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(true);
        let mut map = HashMap::new();
        for (pc, _op, _cp) in static_field_sites(code) {
            map.insert(pc, field);
        }
        b.set_static_field_info(map);
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        b.compile_method(locals, slots, 4, code)
    }

    /// `putstatic` of every primitive tag: the value is narrowed to the
    /// declared type, the context comes out of its frame word, the four
    /// arguments arrive in X0..X3, and the `i64::MIN` sentinel is tested.
    #[test]
    fn r9w17_putstatic_narrows_loads_the_context_and_calls_the_helper() {
        for (tag, descriptor, load, narrow) in [
            (b'I', "(I)V", 0x1au8, None),
            (b'B', "(I)V", 0x1a, Some("sxtb")),
            (b'C', "(I)V", 0x1a, Some("and 0xffff")),
            (b'Z', "(I)V", 0x1a, Some("and 1")),
            (b'J', "(J)V", 0x1e, None),
            (b'F', "(F)V", 0x22, Some("fmov")),
            (b'D', "(D)V", 0x26, Some("fmov")),
        ] {
            // <load_0>; putstatic #3; return
            let code = [load, 0xb3, 0x00, 0x03, 0xb1];
            let r = compile_putstatic(
                descriptor,
                2,
                &code,
                static_field(11, 4, tag, false),
                R9W17_PUTSTATIC,
            );
            assert!(r.success, "putstatic of '{}' must compile", char::from(tag));
            assert!(
                r.needs_context,
                "'{}': jit_putstatic_* takes the VM pointer",
                char::from(tag)
            );
            assert_eq!(
                r.static_init_classes,
                vec![11],
                "'{}': the ensure-initialized obligation is recorded",
                char::from(tag)
            );

            let call = r
                .instructions
                .iter()
                .position(|i| {
                    matches!(
                        i,
                        Arm64Instruction::MovImm { rd, imm }
                            if *rd == Arm64Register::X16 && *imm == R9W17_PUTSTATIC as i64
                    )
                })
                .expect("the putstatic helper is called");
            let before = &r.instructions[..call];
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X1 && *imm == 11
                )),
                "'{}': the class id is the second argument",
                char::from(tag)
            );
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X2 && *imm == 4
                )),
                "'{}': the field index is the third",
                char::from(tag)
            );
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X3
                )),
                "'{}': the value is the fourth",
                char::from(tag)
            );
            let found = match narrow {
                None => true,
                Some("sxtb") => before
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Sxtb { .. })),
                Some("and 0xffff") => before
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 0xFFFF)),
                Some("and 1") => before
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 1)),
                Some("fmov") => before.iter().any(|i| {
                    matches!(
                        i,
                        Arm64Instruction::FmovFromFp { .. }
                            | Arm64Instruction::FmovFromFpSingle { .. }
                    )
                }),
                Some(other) => panic!("unknown expectation {other}"),
            };
            assert!(
                found,
                "'{}': expected {narrow:?} before the call",
                char::from(tag)
            );

            // The sentinel is tested against i64::MIN, not against zero: see
            // `emit_putstatic` for why a `!= 0` test would be the bug.
            let after = &r.instructions[call..];
            assert!(
                after.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { imm, .. } if *imm == i64::MIN
                )) && after
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
                "'{}': the i64::MIN sentinel must be tested",
                char::from(tag)
            );
            assert!(
                emit_machine_code(&r).is_some(),
                "'{}': the body must encode",
                char::from(tag)
            );
        }
    }

    /// A `volatile` static fences on BOTH sides, and a plain one on neither.
    #[test]
    fn r9w17_a_volatile_putstatic_fences_on_both_sides() {
        let code = [0x1au8, 0xb3, 0x00, 0x03, 0xb1];
        for is_volatile in [false, true] {
            let r = compile_putstatic(
                "(I)V",
                2,
                &code,
                static_field(1, 0, b'I', is_volatile),
                R9W17_PUTSTATIC,
            );
            assert!(r.success);
            let call = r
                .instructions
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
                .expect("the helper is called");
            let before = r.instructions[..call]
                .iter()
                .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
                .count();
            let after = r.instructions[call..]
                .iter()
                .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
                .count();
            if is_volatile {
                assert_eq!(before, 1, "the release edge is a leading DMB ISH");
                assert_eq!(after, 1, "the StoreLoad edge is a trailing DMB ISH");
            } else {
                assert_eq!(before + after, 0, "a plain static owes no fence");
            }
        }
    }

    /// Every "not wired" refusal for `putstatic`. The `base_cell` one is the
    /// load-bearing case: it is not read by this lowering at all, it is the
    /// caller's proof that the declaring class is already initialized, and
    /// without it `<clinit>` could run inside the helper -- a safepoint this
    /// call sequence records no oop map for.
    #[test]
    fn r9w17_putstatic_refuses_every_unwired_shape() {
        let code = [0x1au8, 0xb3, 0x00, 0x03, 0xb1];
        // An unwired helper.
        let r = compile_putstatic("(I)V", 2, &code, static_field(1, 0, b'I', false), 0);
        assert!(!r.success, "an unwired jit_putstatic_* must refuse");

        // No `base_cell`: the class is not known to be initialized.
        let mut uninit = static_field(1, 0, b'I', false);
        uninit.base_cell = 0;
        let r = compile_putstatic("(I)V", 2, &code, uninit, R9W17_PUTSTATIC);
        assert!(
            !r.success,
            "without the initialized-class proof, <clinit> could run inside \
             the helper -- and this call records no oop map"
        );

        // A reference static: its store owes the SATB pre-barrier.
        for &tag in b"L[" {
            let r = compile_putstatic(
                "(LX;)V",
                2,
                &[0x2au8, 0xb3, 0x00, 0x03, 0xb1],
                static_field(1, 0, tag, false),
                R9W17_PUTSTATIC,
            );
            assert!(
                !r.success,
                "a '{}' static must refuse: no SATB pre-barrier",
                char::from(tag)
            );
        }

        // An unresolved site.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.putstatic_int = R9W17_PUTSTATIC;
        b.set_helpers(h);
        b.set_method_descriptor("(I)V", true);
        let r = b.compile_method(2, 1, 4, &code);
        assert!(!r.success, "an unresolved putstatic site must refuse");
    }

    /// A `getstatic` still lowers with no context at all -- wave 17 gave
    /// `putstatic` the context ABI without dragging the read into it.
    #[test]
    fn r9w17_a_getstatic_only_method_still_takes_no_context() {
        let r = compile_with_statics(
            "()I",
            &[0xb2, 0x00, 0x03, 0xac],
            &[(0, static_field(9, 0, b'I', false))],
        );
        assert!(r.success);
        assert!(
            !r.needs_context,
            "a read needs no VM pointer; only the write does"
        );
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 18: the oop-map-recording call, and allocation on it.
    // -----------------------------------------------------------------------

    const R9W18_NEW: usize = 0x7FFF_0000_9000;
    const R9W18_NEWARRAY: usize = 0x7FFF_0000_A000;
    const R9W18_ANEWARRAY: usize = 0x7FFF_0000_B000;

    /// Compile `code` with the three allocation helpers wired and `site`
    /// resolved at every `new`/`anewarray`.
    fn compile_alloc(
        descriptor: &str,
        locals: usize,
        code: &[u8],
        site: Option<Arm64NewSite>,
        table_empty: bool,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.new_object = R9W18_NEW;
        h.newarray = R9W18_NEWARRAY;
        h.anewarray_object = R9W18_ANEWARRAY;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(table_empty);
        if let Some(site) = site {
            let mut map = HashMap::new();
            for (pc, op, _operand) in allocation_sites(code) {
                if op != 0xbc {
                    map.insert(pc, site);
                }
            }
            b.set_new_site_info(map);
        }
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        b.compile_method(locals, slots, 4, code)
    }

    fn a_new_site() -> Arm64NewSite {
        Arm64NewSite {
            class_id: 33,
            num_fields: 5,
        }
    }

    /// The site walk reports all three allocation opcodes, with the operand
    /// each one actually has -- a constant-pool index for `new`/`anewarray`,
    /// the `atype` BYTE for `newarray` -- and decodes instructions, so a
    /// `0xbb` inside another instruction's operand is not an allocation.
    #[test]
    fn r9w18_allocation_sites_report_each_opcodes_own_operand() {
        // sipush 0xbbbc; new #7; newarray T_INT(10); iconst_0; anewarray #9
        let code = [
            0x11, 0xbb, 0xbc, 0xbb, 0x00, 0x07, 0xbc, 0x0a, 0x03, 0xbd, 0x00, 0x09,
        ];
        assert_eq!(
            allocation_sites(&code),
            vec![(3usize, 0xbbu8, 7u16), (6, 0xbc, 10), (9, 0xbd, 9)],
        );
    }

    /// AN ALLOCATING METHOD GETS A SAFEPOINT FRAME EVEN WITH POLLS OFF.
    ///
    /// The homes and the id word used to be reserved only for
    /// `CRATONVM_JIT_ARM64_SAFEPOINTS`. An allocation stops this frame inside
    /// a callee that can move objects whether or not polls are on, so it needs
    /// the same frame -- otherwise the map has nowhere to name the locals from
    /// and `active_safepoint_id` cannot select it.
    #[test]
    fn r9w18_an_allocating_method_reserves_the_safepoint_frame_without_polls() {
        // new #3; areturn
        let r = compile_alloc(
            "()LX;",
            2,
            &[0xbb, 0x00, 0x03, 0xb0],
            Some(a_new_site()),
            true,
        );
        assert!(r.success, "new must compile");
        assert!(
            r.sp_id_slot_off != 0,
            "an allocating method reserves the safepoint-id word"
        );
        assert!(
            r.needs_context,
            "every allocation helper takes the VM pointer"
        );
        assert_eq!(
            r.safepoint_count, 1,
            "the allocation call is a safepoint and publishes a map"
        );
        assert_eq!(r.pending_oop_maps.len(), 1);

        // ...and a method that allocates nothing still does not.
        let plain = compile_alloc("()I", 2, &[0x03, 0xac], None, true);
        assert!(plain.success);
        assert_eq!(
            plain.sp_id_slot_off, 0,
            "a non-allocating method with polls off is unchanged"
        );
        assert!(!plain.needs_context);
    }

    /// THE POINT OF THE WAVE: every reference the call could invalidate is in
    /// the frame, NAMED, and reloaded afterwards.
    ///
    /// A callee-saved register survives the call by the AAPCS64 contract --
    /// but inside the CALLEE's save area, where only a conservative walk sees
    /// it, and a conservative walk marks without rewriting. So every store of
    /// a callee-saved register to the frame before the call must have a
    /// matching load after it; that load is what carries a moved object's new
    /// address back.
    #[test]
    fn r9w18_every_homed_reference_local_is_reloaded_after_the_call() {
        // aload_0; astore_1; new #3; pop; aload_1; areturn
        //   -- local 0 is a reference parameter and local 1 is live ACROSS
        //      the allocation.
        let r = compile_alloc(
            "(LX;)LX;",
            3,
            &[0x2a, 0x4c, 0xbb, 0x00, 0x03, 0x57, 0x2b, 0xb0],
            Some(a_new_site()),
            true,
        );
        assert!(r.success, "{:?}", r.instructions);
        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W18_NEW as i64
                )
            })
            .expect("the allocation helper is called");
        let blr = call
            + r.instructions[call..]
                .iter()
                .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
                .expect("the call is emitted");

        // The lowering begins with the context load; everything before that
        // (the prologue's own callee-saved save, the argument homing) is not
        // part of this call's sequence.
        let lowering = r.instructions[..blr]
            .iter()
            .rposition(|i| {
                matches!(
                    i,
                    Arm64Instruction::Ldr { rt, rn, .. }
                        if *rn == Arm64Register::FP && SCRATCH_REGS.contains(rt)
                )
            })
            .expect("the context is read out of its frame word");
        let homed: Vec<(Arm64Register, i32)> = r.instructions[lowering..blr]
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::Str { rt, rn, offset }
                    if *rn == Arm64Register::FP && rt.is_callee_saved() =>
                {
                    Some((*rt, *offset))
                }
                _ => None,
            })
            .collect();
        assert!(
            !homed.is_empty(),
            "a reference local must be homed before the call: {:?}",
            r.instructions
        );
        for (reg, off) in &homed {
            assert!(
                r.instructions[blr..].iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Ldr { rt, rn, offset }
                        if rt == reg && *rn == Arm64Register::FP && offset == off
                )),
                "{reg:?} homed at {off} must be RELOADED after the call -- \
                 otherwise a moved object's new address never reaches it"
            );
        }

        // ...and the map names those very offsets.
        let map = &r.pending_oop_maps[0];
        for (_, off) in &homed {
            // Cast: a frame offset, which the map carries as an i16.
            assert!(
                map.frame_slot_offsets.contains(&(*off as i16)),
                "the map must name the home at {off}, or the collector cannot \
                 rewrite it: {:?}",
                map.frame_slot_offsets
            );
        }
        assert_eq!(
            map.safepoint_id, 2,
            "the map is keyed by the allocation's own bci"
        );
    }

    /// A reference OPERAND live across the allocation is spilled to its depth
    /// slot and named in the map too -- the operand half of the same rule.
    #[test]
    fn r9w18_a_reference_operand_live_across_an_allocation_is_in_the_map() {
        // aload_0; new #3; swap; pop; areturn
        //   -- the parameter is on the STACK, under the new object.
        let r = compile_alloc(
            "(LX;)LX;",
            2,
            &[0x2a, 0xbb, 0x00, 0x03, 0x5f, 0x57, 0xb0],
            Some(a_new_site()),
            true,
        );
        assert!(r.success, "{:?}", r.instructions);
        let blr = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .expect("the allocation helper is called");
        // An OPERAND lives in the scratch pool; the prologue's callee-saved
        // save and the context store also write FP-relative words, and neither
        // is an operand.
        let spills: Vec<i32> = r.instructions[..blr]
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::Str { rt, rn, offset }
                    if *rn == Arm64Register::FP && SCRATCH_REGS.contains(rt) =>
                {
                    Some(*offset)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            spills.len(),
            1,
            "exactly one operand is live across the call: {:?}",
            r.instructions
        );
        // Cast: a frame offset, as the map carries it.
        assert!(
            r.pending_oop_maps[0]
                .frame_slot_offsets
                .contains(&(spills[0] as i16)),
            "the reference operand's slot must be in the map: {:?}",
            r.pending_oop_maps[0].frame_slot_offsets
        );
        assert!(
            r.instructions[blr..].iter().any(|i| matches!(
                i,
                Arm64Instruction::Ldr { rn, offset, .. }
                    if *rn == Arm64Register::FP && *offset == spills[0]
            )),
            "and it must be reloaded afterwards"
        );
    }

    /// The three allocation opcodes, their helpers and their arguments.
    #[test]
    fn r9w18_each_allocation_opcode_calls_its_own_helper() {
        for (name, code, locals, descriptor, helper, second) in [
            (
                "new",
                vec![0xbbu8, 0x00, 0x03, 0xb0],
                2,
                "()LX;",
                R9W18_NEW,
                33i64,
            ),
            (
                "newarray",
                vec![0x05, 0xbc, 0x0a, 0xb0],
                2,
                "()LX;",
                R9W18_NEWARRAY,
                10,
            ),
            (
                "anewarray",
                vec![0x05, 0xbd, 0x00, 0x03, 0xb0],
                2,
                "()LX;",
                R9W18_ANEWARRAY,
                33,
            ),
        ] {
            let r = compile_alloc(descriptor, locals, &code, Some(a_new_site()), true);
            assert!(r.success, "{name} must compile: {:?}", r.instructions);
            let call = r
                .instructions
                .iter()
                .position(|i| {
                    matches!(
                        i,
                        Arm64Instruction::MovImm { rd, imm }
                            if *rd == Arm64Register::X16 && *imm == helper as i64
                    )
                })
                .unwrap_or_else(|| panic!("{name}: its own helper must be called"));
            let before = &r.instructions[..call];
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0
                )),
                "{name}: the context is the first argument"
            );
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X1 && *imm == second
                )),
                "{name}: the class id (or atype) is the second argument"
            );
            // The null-result check, and the reference push.
            assert!(
                r.instructions[call..]
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Cbnz { .. })),
                "{name}: a null result must be checked"
            );
            assert!(
                emit_machine_code(&r).is_some(),
                "{name}: the body must encode"
            );
        }
    }

    /// Every "not wired" refusal for an allocation. The exception-table one is
    /// the exactness condition: the helper's OOM is drained with an unknown
    /// pc, so a handler in this frame could catch the wrong thing.
    #[test]
    fn r9w18_allocation_refuses_every_unwired_shape() {
        let code = [0xbbu8, 0x00, 0x03, 0xb0];
        // An unresolved site.
        let r = compile_alloc("()LX;", 2, &code, None, true);
        assert!(!r.success, "an unresolved new site must refuse");

        // A non-empty exception table.
        let r = compile_alloc("()LX;", 2, &code, Some(a_new_site()), false);
        assert!(
            !r.success,
            "with a handler in this frame the drain could catch the wrong thing"
        );

        // An unwired helper.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_method_descriptor("()LX;", true);
        b.set_exception_table_empty(true);
        let mut map = HashMap::new();
        map.insert(0usize, a_new_site());
        b.set_new_site_info(map);
        let r = b.compile_method(2, 0, 4, &code);
        assert!(!r.success, "an unwired jit_new_object must refuse");
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 21: the frame base an allocating method never published.
    // -----------------------------------------------------------------------

    const R9W21_FRAME_RECORD: usize = 0x7FFF_0000_C000;

    /// [`compile_alloc`] with `helpers.frame_record` wired as well, so the
    /// prologue can publish this frame's base.
    fn compile_alloc_with_frame_record(
        code: &[u8],
        site: Option<Arm64NewSite>,
        frame_record: usize,
        polls: bool,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(polls);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.new_object = R9W18_NEW;
        h.newarray = R9W18_NEWARRAY;
        h.anewarray_object = R9W18_ANEWARRAY;
        h.frame_record = frame_record;
        b.set_helpers(h);
        b.set_method_descriptor("()LX;", true);
        b.set_exception_table_empty(true);
        if let Some(site) = site {
            let mut map = HashMap::new();
            for (pc, op, _operand) in allocation_sites(code) {
                if op != 0xbc {
                    map.insert(pc, site);
                }
            }
            b.set_new_site_info(map);
        }
        b.compile_method(2, 0, 4, code)
    }

    /// How many times `code` calls `frame_record`: the three-instruction
    /// `MOV X0, FP; MOVZ/MOVK X16, #helper; BLR X16` shape, counted by the
    /// `MovImm` that names the helper.
    fn frame_record_calls(r: &Arm64CompileResult, helper: usize) -> usize {
        r.instructions
            .windows(2)
            .filter(|w| {
                matches!(
                    w[0],
                    Arm64Instruction::MovImm {
                        rd: Arm64Register::X16,
                        imm
                    } if imm == helper as i64
                ) && matches!(
                    w[1],
                    Arm64Instruction::Blr {
                        rn: Arm64Register::X16
                    }
                )
            })
            .count()
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 22: the first bytecode CALL this backend compiles.
    // -----------------------------------------------------------------------

    const R9W22_DISPATCH: usize = 0x7FFF_0000_D000;
    const R9W22_THREW: usize = 0x7FFF_0000_E000;
    const R9W22_INFO: u64 = 0x7FFF_0000_F000;

    fn an_invoke_site(num_jit_args: usize, return_type: u8) -> Arm64InvokeSite {
        Arm64InvokeSite {
            info_ptr: R9W22_INFO,
            num_jit_args,
            return_type,
        }
    }

    /// Compile `code` with `jit_invoke_dispatch` wired and every `invoke*`
    /// site resolved to `site`.
    fn compile_invoke(
        descriptor: &str,
        locals: usize,
        code: &[u8],
        site: Option<Arm64InvokeSite>,
        table_empty: bool,
        dispatch: usize,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.invoke_dispatch = dispatch;
        h.dispatch_threw = R9W22_THREW;
        h.frame_record = R9W21_FRAME_RECORD;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(table_empty);
        if let Some(site) = site {
            let mut map = HashMap::new();
            for (pc, op, _cp) in invoke_sites(code) {
                if op != 0xba {
                    map.insert(pc, site);
                }
            }
            b.set_invoke_site_info(map);
        }
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        b.compile_method(locals, slots, 4, code)
    }

    /// The site walk decodes all three instruction WIDTHS.
    ///
    /// `invokeinterface` and `invokedynamic` are five bytes, not three. A
    /// three-byte stride reads their trailing bytes as opcodes -- and
    /// `invokeinterface`'s count byte is an ARGUMENT COUNT, so for a
    /// one-argument call it reads as `0x01`, `iconst_m1`.
    #[test]
    fn r9w22_invoke_sites_decode_all_five_widths() {
        let code = [
            0x2a, // aload_0
            0xb6, 0x00, 0x07, // invokevirtual #7
            0xb7, 0x00, 0x08, // invokespecial #8
            0xb8, 0x00, 0x09, // invokestatic #9
            0xb9, 0x00, 0x0a, 0x01, 0x00, // invokeinterface #10, 1 arg
            0xba, 0x00, 0x0b, 0x00, 0x00, // invokedynamic #11
            0xb1, // return
        ];
        assert_eq!(
            invoke_sites(&code),
            vec![
                (1usize, 0xb6u8, 7u16),
                (4, 0xb7, 8),
                (7, 0xb8, 9),
                (10, 0xb9, 10),
                (15, 0xba, 11),
            ],
        );
    }

    /// Every "not wired" refusal for a call, and the one that is not about
    /// wiring at all.
    #[test]
    fn r9w22_an_invoke_refuses_every_unwired_shape() {
        // iload_0; iload_1; invokestatic #1; ireturn
        let code = [0x1au8, 0x1b, 0xb8, 0x00, 0x01, 0xac];
        let site = an_invoke_site(2, b'I');

        let ok = compile_invoke("(II)I", 2, &code, Some(site), true, R9W22_DISPATCH);
        assert!(ok.success, "the wired shape must compile");

        let r = compile_invoke("(II)I", 2, &code, None, true, R9W22_DISPATCH);
        assert!(!r.success, "an unresolved site must refuse");

        let r = compile_invoke("(II)I", 2, &code, Some(site), true, 0);
        assert!(!r.success, "an unwired jit_invoke_dispatch must refuse");

        // The exactness condition, and the one that keeps `synchronized` out:
        // the callee's exception leaves through the epilogue and the drain
        // raises it with an unknown pc, so a handler in THIS frame could catch
        // the wrong thing.
        let r = compile_invoke("(II)I", 2, &code, Some(site), false, R9W22_DISPATCH);
        assert!(
            !r.success,
            "with a handler in this frame the drain could catch the wrong thing"
        );

        // ...and `invokedynamic`, which no wiring can rescue: the helper's
        // `invoke_kind` has no value for a call site that is not a class.
        let indy = [0x1au8, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac];
        let r = compile_invoke("(I)I", 2, &indy, Some(an_invoke_site(1, b'I')), true, R9W22_DISPATCH);
        assert!(!r.success, "invokedynamic has no lowering here");
    }

    /// A method that calls takes the VM context and a SAFEPOINT FRAME, with
    /// polls off -- because a callee can be stopped inside anything at all.
    #[test]
    fn r9w22_a_calling_method_takes_the_context_and_a_safepoint_frame() {
        let code = [0x1au8, 0x1b, 0xb8, 0x00, 0x01, 0xac];
        let r = compile_invoke(
            "(II)I",
            2,
            &code,
            Some(an_invoke_site(2, b'I')),
            true,
            R9W22_DISPATCH,
        );
        assert!(r.success);
        assert!(!r.polls_enabled, "polls are off; the CALL is the safepoint");
        assert!(r.needs_context, "jit_invoke_dispatch takes the VM pointer");
        assert_ne!(r.sp_id_slot_off, 0, "the id word is reserved");
        assert!(r.frame_base_published, "and the base to read it through");
        assert_eq!(r.safepoint_count, 1, "the call is the one safepoint");
        assert_eq!(r.pending_oop_maps.len(), 1);
        let cm = publish_compiled_method(&r).expect("publishes");
        assert!(cm.needs_context());
        assert!(cm.fully_oop_covered, "one mapped safepoint, an id and a base");
    }

    /// THE ARGUMENT BUFFER IS THE OPERAND AREA, and a reference argument is
    /// therefore NAMED in the call's map.
    ///
    /// This is the property that makes the aarch64 lowering cheaper than
    /// x64's, not merely different. There, an outgoing reference argument is
    /// marshalled into a staging area no frame-slot map describes, so the site
    /// raises `pending_staged_args_unmapped` and the method loses
    /// `fully_oop_covered` for good. Here the arguments are still operand
    /// entries when the map is taken, so each reference among them is named
    /// where it already lies -- and a collector that moves one during the
    /// callee rewrites the very word the helper reads.
    #[test]
    fn r9w22_a_reference_argument_is_named_in_the_calls_map() {
        // aload_0; invokestatic #1 (Ljava/lang/Object;)I; ireturn
        let code = [0x2au8, 0xb8, 0x00, 0x01, 0xac];
        let r = compile_invoke(
            "(Ljava/lang/Object;)I",
            2,
            &code,
            Some(an_invoke_site(1, b'I')),
            true,
            R9W22_DISPATCH,
        );
        assert!(r.success, "a reference argument must compile");
        assert_eq!(r.pending_oop_maps.len(), 1);
        assert!(
            !r.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "the reference argument must be named at the call: map={:?}",
            r.pending_oop_maps[0]
        );
        // ...and there is exactly ONE argument buffer address computed, off FP.
        let bases = r
            .instructions
            .iter()
            .filter(|i| {
                matches!(i, Arm64Instruction::SubImm { rn, .. } if *rn == Arm64Register::FP)
            })
            .count();
        assert_eq!(bases, 1, "one call, one buffer address");
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 23: checkcast and instanceof.
    // -----------------------------------------------------------------------

    const R9W23_CHECKCAST: usize = 0x7FFF_0001_0000;
    const R9W23_INSTANCEOF: usize = 0x7FFF_0001_1000;
    /// A stand-in for the interned class-name bytes. Never dereferenced by
    /// anything these tests run; the fake helpers ignore it.
    const R9W23_NAME: u64 = 0x7FFF_0001_2000;

    fn compile_typecheck(
        descriptor: &str,
        code: &[u8],
        site: Option<Arm64TypecheckSite>,
        table_empty: bool,
        checkcast: usize,
        instanceof: usize,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.checkcast = checkcast;
        h.instanceof_check = instanceof;
        h.frame_record = R9W21_FRAME_RECORD;
        b.set_helpers(h);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(table_empty);
        if let Some(site) = site {
            let mut map = HashMap::new();
            for (pc, _op, _cp) in typecheck_sites(code) {
                map.insert(pc, site);
            }
            b.set_typecheck_site_info(map);
        }
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        b.compile_method(2, slots, 4, code)
    }

    fn a_typecheck_site() -> Arm64TypecheckSite {
        Arm64TypecheckSite {
            name_ptr: R9W23_NAME,
            name_len: 16,
        }
    }

    /// The site walk, and that it decodes instructions rather than bytes.
    #[test]
    fn r9w23_typecheck_sites_are_walked() {
        // sipush 0xc0c1; aload_0; checkcast #7; instanceof #9; ireturn
        let code = [
            0x11, 0xc0, 0xc1, 0x2a, 0xc0, 0x00, 0x07, 0xc1, 0x00, 0x09, 0xac,
        ];
        assert_eq!(
            typecheck_sites(&code),
            vec![(4usize, 0xc0u8, 7u16), (7, 0xc1, 9)],
        );
    }

    /// Every refusal a type check has, including the one that is not wiring.
    #[test]
    fn r9w23_a_typecheck_refuses_every_unwired_shape() {
        // aload_0; checkcast #1; areturn
        let code = [0x2au8, 0xc0, 0x00, 0x01, 0xb0];
        let d = "(Ljava/lang/Object;)Ljava/lang/Object;";

        let ok = compile_typecheck(d, &code, Some(a_typecheck_site()), true, R9W23_CHECKCAST, 0);
        assert!(ok.success, "the wired shape must compile");
        assert!(ok.needs_context, "the helper takes the VM pointer");
        assert_eq!(ok.safepoint_count, 1, "resolving the target can allocate");

        let r = compile_typecheck(d, &code, None, true, R9W23_CHECKCAST, 0);
        assert!(!r.success, "an unresolved target must refuse");

        let r = compile_typecheck(d, &code, Some(a_typecheck_site()), true, 0, 0);
        assert!(!r.success, "an unwired jit_checkcast must refuse");

        let r = compile_typecheck(d, &code, Some(a_typecheck_site()), false, R9W23_CHECKCAST, 0);
        assert!(
            !r.success,
            "a failed cast returns the sentinel with a CCE pending, which the \
             drain raises with an unknown pc"
        );

        // A resolved-looking site with a ZERO length is the trap: the helper
        // answers "not an instance" for every object, which is a wrong answer
        // rather than a missing one.
        let empty = Arm64TypecheckSite {
            name_ptr: R9W23_NAME,
            name_len: 0,
        };
        let r = compile_typecheck(d, &code, Some(empty), true, R9W23_CHECKCAST, 0);
        assert!(!r.success, "an empty target name must refuse, not be passed on");
    }

    /// The object stays on the operand stack across the call, so it is NAMED.
    ///
    /// Resolving the target class on first use allocates its `java/lang/Class`
    /// mirror, so this call can genuinely move the very object being tested.
    #[test]
    fn r9w23_the_tested_object_is_named_in_the_calls_map() {
        let code = [0x2au8, 0xc1, 0x00, 0x01, 0xac];
        let r = compile_typecheck(
            "(Ljava/lang/Object;)I",
            &code,
            Some(a_typecheck_site()),
            true,
            0,
            R9W23_INSTANCEOF,
        );
        assert!(r.success);
        assert_eq!(r.pending_oop_maps.len(), 1);
        assert!(
            !r.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "the object under test must be named at the call: map={:?}",
            r.pending_oop_maps[0]
        );
    }

    /// WAVE 18 SHIPPED WITHOUT THIS. It gave an allocating method the
    /// safepoint homes and the id word on the argument that an allocation
    /// stops the frame inside a moving callee whether or not polls are on --
    /// and left `emit_frame_record` gated on polls.
    ///
    /// The consequence is that same argument's own failure mode one level up.
    /// The runtime reads the id at `[frame_base - sp_id_slot_off]` and learns
    /// `frame_base` only from this call, so with no frame record
    /// `PreciseFrameInfo::exact_rbp` stays `0`, every precise path declines
    /// the frame, and the map the allocation had just recorded could never be
    /// selected -- exactly what reserving the id word was meant to prevent.
    #[test]
    fn r9w21_an_allocating_method_publishes_its_frame_base() {
        // nop; new #3; areturn
        let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
        let r = compile_alloc_with_frame_record(
            &code,
            Some(a_new_site()),
            R9W21_FRAME_RECORD,
            false,
        );
        assert!(r.success, "new must compile");
        assert!(!r.polls_enabled, "this compilation emits no polls");
        assert_eq!(
            frame_record_calls(&r, R9W21_FRAME_RECORD),
            1,
            "an allocating method publishes its frame base exactly once"
        );
        assert!(r.frame_base_published);

        // A method that allocates nothing still publishes nothing with polls
        // off: it can be stopped nowhere, so there is no frame to locate.
        let plain = compile_alloc_with_frame_record(
            &[0x01u8, 0xb0],
            None,
            R9W21_FRAME_RECORD,
            false,
        );
        assert!(plain.success);
        assert_eq!(frame_record_calls(&plain, R9W21_FRAME_RECORD), 0);
        assert!(!plain.frame_base_published);
    }

    /// A PUBLISHED FRAME SIZE, without which nothing walks this frame at all.
    ///
    /// The third of wave 21's "published and unreadable" family, and the one
    /// that would have made arming the oracle pointless on its own: every
    /// walker bounds a compiled frame by `[rbp - osr_frame_size, rbp)` and
    /// refuses it outright when the field is `0`, which is what this backend
    /// published for its whole existence.
    ///
    /// The value is FP-to-SP, not the whole frame: the prologue's
    /// `STP X29, X30, [SP, #-16]!` puts FP sixteen bytes below the old SP and
    /// `SUB SP, SP, #(frame_size - 16)` is the rest, so `frame_size` itself
    /// would put the band's floor sixteen bytes inside the CALLEE's frame.
    #[test]
    fn r9w21_the_frame_size_the_gc_walks_this_frame_by_is_published() {
        let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
        let r = compile_alloc_with_frame_record(
            &code,
            Some(a_new_site()),
            R9W21_FRAME_RECORD,
            false,
        );
        assert!(r.success);
        let cm = publish_compiled_method(&r).expect("publishes");
        assert_eq!(
            cm.osr_frame_size,
            r.frame.frame_size - 16,
            "the band is FP down to SP, which is what the prologue subtracts"
        );
        assert!(cm.osr_frame_size > 0, "a zero refuses the frame entirely");
        // ...and it reaches the deepest word the maps can name: slot 0 lives
        // at `[FP + spill_offset]`, so the band's floor must be at or below it.
        assert!(
            cm.osr_frame_size >= -r.frame.spill_offset,
            "the band must contain the spill area the oop maps describe:              band={} deepest_spill={}",
            cm.osr_frame_size,
            -r.frame.spill_offset
        );
    }

    /// ...and now that the base is published, the claim is worth making.
    ///
    /// Wave 18 withheld `fully_oop_covered` from an allocation-only method and
    /// said why: the strongest claim the backend makes, widened in no hurry.
    /// The argument for widening was already written there -- with polls off
    /// the frame can be stopped only inside a call, and every call this
    /// backend emits either records a map or carries a reason it cannot
    /// safepoint -- and what was missing was the frame base to read the maps
    /// through.
    #[test]
    fn r9w21_an_allocation_only_method_now_claims_full_oop_coverage() {
        let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
        let r = compile_alloc_with_frame_record(
            &code,
            Some(a_new_site()),
            R9W21_FRAME_RECORD,
            false,
        );
        assert!(r.success);
        assert!(!r.polls_enabled, "the allocation route, not the poll route");
        let cm = publish_compiled_method(&r).expect("publishes");
        assert_eq!(cm.oop_maps.len(), 1);
        assert!(
            cm.fully_oop_covered,
            "one safepoint, one map, an id word and a published base"
        );
    }

    /// The fifth term, alone. Polls ON and everything else wired, but no
    /// `frame_record` helper -- which is the VM's state under
    /// `CRATONVM_NO_PRECISE_JIT_MAPS=1` with no moving young generation.
    ///
    /// The maps are still published, because they cost nothing and the
    /// walker's precise scan is strictly additive. The CLAIM is not, because
    /// nothing can locate the frame to read them from, and a coverage bit over
    /// unreadable maps is the vacuous proof
    /// `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`
    /// is about.
    #[test]
    fn r9w21_without_a_frame_record_there_is_no_coverage_claim() {
        let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
        let r = compile_alloc_with_frame_record(&code, Some(a_new_site()), 0, true);
        assert!(r.success);
        assert!(r.polls_enabled);
        assert!(!r.frame_base_published, "the helper slot is 0");
        let cm = publish_compiled_method(&r).expect("publishes");
        assert!(
            !cm.oop_maps.is_empty(),
            "the maps are published either way -- the precise scan is additive"
        );
        assert!(
            !cm.fully_oop_covered,
            "no frame base means no way to read them, so no claim"
        );
    }

    /// WAVE 17 SHIPPED WITHOUT THIS. `putstatic`'s sentinel path leaves
    /// through the epilogue for the drain to raise, and the drain throws with
    /// an unknown pc -- so it needs an empty exception table, exactly like
    /// every other lowering here that can trap. The `<clinit>` branch the wave
    /// argues is unreachable is not the only way the helper returns the
    /// sentinel: `contain(.., OnPanic::Throw, i64::MIN, ..)` returns it for a
    /// panic as well.
    #[test]
    fn r9w18_putstatic_refuses_with_a_non_empty_exception_table() {
        let code = [0x1au8, 0xb3, 0x00, 0x03, 0xb1];
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.putstatic_int = R9W17_PUTSTATIC;
        b.set_helpers(h);
        b.set_method_descriptor("(I)V", true);
        b.set_exception_table_empty(false);
        let mut map = HashMap::new();
        map.insert(1usize, static_field(1, 0, b'I', false));
        b.set_static_field_info(map);
        let r = b.compile_method(2, 1, 4, &code);
        assert!(
            !r.success,
            "a putstatic in a method with a handler must refuse"
        );
    }

    // -----------------------------------------------------------------------
    // Round 9 wave 19: the reference stores, and the reference array element.
    // -----------------------------------------------------------------------

    const R9W19_PUTFIELD_OBJ: usize = 0x7FFF_0000_C000;
    const R9W19_PUTSTATIC_OBJ: usize = 0x7FFF_0000_D000;
    const R9W19_AALOAD: usize = 0x7FFF_0000_E000;
    const R9W19_AASTORE: usize = 0x7FFF_0000_F000;
    const R9W19_TYPE_CHECK: usize = 0x7FFF_0001_0000;

    /// Every helper this wave needs, wired at once.
    fn r9w19_helpers() -> crate::JitRuntimeHelpers {
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.jit_npe_with_action = R9W12_NPE;
        h.throw_aioobe = R9W13_AIOOBE;
        h.putfield_object = R9W19_PUTFIELD_OBJ;
        h.putstatic_object = R9W19_PUTSTATIC_OBJ;
        h.aaload = R9W19_AALOAD;
        h.aastore = R9W19_AASTORE;
        h.aastore_type_check = R9W19_TYPE_CHECK;
        h
    }

    fn compile_refs(
        descriptor: &str,
        locals: usize,
        code: &[u8],
        instance: Option<Arm64InstanceField>,
        statics: Option<Arm64StaticField>,
        helpers: crate::JitRuntimeHelpers,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_helpers(helpers);
        b.set_method_descriptor(descriptor, true);
        b.set_exception_table_empty(true);
        if let Some(f) = instance {
            let mut map = HashMap::new();
            for (pc, _op, _cp) in instance_field_sites(code) {
                map.insert(pc, f);
            }
            b.set_instance_field_info(map);
        }
        if let Some(f) = statics {
            let mut map = HashMap::new();
            for (pc, _op, _cp) in static_field_sites(code) {
                map.insert(pc, f);
            }
            b.set_static_field_info(map);
        }
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        b.compile_method(locals, slots, 4, code)
    }

    /// A REFERENCE `putfield` goes to `jit_putfield_object` with the context
    /// first, is not narrowed, and is NOT a safepoint -- the barriers enqueue
    /// and mark, they run no Java, and x64 records no map there either.
    #[test]
    fn r9w19_a_reference_putfield_calls_the_barrier_helper() {
        for tag in *b"L[" {
            // aload_0; aload_1; putfield #3; return
            let r = compile_refs(
                "(LX;LY;)V",
                2,
                &[0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1],
                Some(int_field(4, tag, false)),
                None,
                r9w19_helpers(),
            );
            assert!(
                r.success,
                "a '{}' putfield must compile: {:?}",
                char::from(tag),
                r.instructions
            );
            assert!(r.needs_context, "jit_putfield_object takes the VM pointer");
            assert!(
                r.pending_oop_maps.is_empty(),
                "the barrier helper cannot safepoint, so it records no map"
            );
            let call = r
                .instructions
                .iter()
                .position(|i| {
                    matches!(
                        i,
                        Arm64Instruction::MovImm { rd, imm }
                            if *rd == Arm64Register::X16 && *imm == R9W19_PUTFIELD_OBJ as i64
                    )
                })
                .expect("the object putfield helper is called");
            let before = &r.instructions[..call];
            assert!(
                before
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::Cbz { .. })),
                "the receiver is null-checked"
            );
            assert!(
                before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X2 && *imm == 4
                )),
                "the field index is the THIRD argument, after the context"
            );
            assert!(
                !before.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Sxtb { .. } | Arm64Instruction::AndImm { .. }
                )),
                "a reference is never narrowed"
            );
            assert!(emit_machine_code(&r).is_some());
        }
    }

    /// ...and a REFERENCE `putstatic` goes to `jit_putstatic_object`, keeping
    /// the sentinel check and the class-init record the primitive path has.
    #[test]
    fn r9w19_a_reference_putstatic_calls_the_barrier_helper() {
        // aload_0; putstatic #3; return
        let r = compile_refs(
            "(LX;)V",
            2,
            &[0x2a, 0xb3, 0x00, 0x03, 0xb1],
            None,
            Some(static_field(12, 6, b'L', false)),
            r9w19_helpers(),
        );
        assert!(r.success, "{:?}", r.instructions);
        assert_eq!(r.static_init_classes, vec![12]);
        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W19_PUTSTATIC_OBJ as i64
                )
            })
            .expect("the object putstatic helper is called");
        assert!(
            r.instructions[call..]
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
            "the sentinel is still tested"
        );
    }

    /// `aaload`: this backend's own guards, then the helper, then the result
    /// is pushed AS AN OOP -- which is what puts it in every later map.
    #[test]
    fn r9w19_aaload_guards_then_calls_and_marks_the_result_as_an_oop() {
        // aload_0; iload_1; aaload; areturn
        let r = compile_refs(
            "([LX;I)LX;",
            3,
            &[0x2a, 0x1b, 0x32, 0xb0],
            None,
            None,
            r9w19_helpers(),
        );
        assert!(r.success, "{:?}", r.instructions);
        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W19_AALOAD as i64
                )
            })
            .expect("jit_aaload is called");
        let before = &r.instructions[..call];
        assert!(
            before
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cbz { .. })),
            "the array is null-checked before the call"
        );
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::BCond {
                    cond: Arm64Condition::Cs,
                    ..
                }
            )),
            "and bounds-checked with the UNSIGNED compare"
        );
        assert_eq!(
            r.pending_oop_maps.len(),
            1,
            "the helper can safepoint, so the call records a map"
        );
        assert!(
            r.instructions[call..]
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
            "the sentinel is tested"
        );
    }

    /// `aastore` type-checks FIRST, and the array and the value survive that
    /// call in the same registers -- which is what leaving them on the operand
    /// stack buys, and what the map then covers.
    #[test]
    fn r9w19_aastore_type_checks_before_it_stores() {
        // aload_0; iload_1; aload_2; aastore; return
        let r = compile_refs(
            "([LX;ILX;)V",
            4,
            &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
            None,
            None,
            r9w19_helpers(),
        );
        assert!(r.success, "{:?}", r.instructions);
        let check = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W19_TYPE_CHECK as i64
                )
            })
            .expect("the element-type check is called");
        let store = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W19_AASTORE as i64
                )
            })
            .expect("the store helper is called");
        assert!(
            check < store,
            "the type check must come first, or an illegal store is committed \
             before anyone can refuse it"
        );
        assert_eq!(
            r.pending_oop_maps.len(),
            2,
            "both calls can safepoint, so both record a map"
        );
        // The array and the value are reference OPERANDS, so both are spilled
        // and both are named -- that is what lets a collector move them
        // during the type check and hand the store their new addresses.
        assert!(
            r.pending_oop_maps[0].frame_slot_offsets.len() >= 2,
            "the array and the value must both be in the first call's map: {:?}",
            r.pending_oop_maps[0].frame_slot_offsets
        );
        assert_eq!(
            r.pending_oop_maps[0].safepoint_id, 3,
            "keyed by the aastore's own bci"
        );
    }

    /// Every "not wired" refusal these four share.
    #[test]
    fn r9w19_the_reference_paths_refuse_every_unwired_shape() {
        // A missing `aastore_type_check` must refuse rather than fall back to
        // the bare store: its ArrayStoreException would then surface only at
        // method return, after this frame had run on.
        let mut h = r9w19_helpers();
        h.aastore_type_check = 0;
        let r = compile_refs(
            "([LX;ILX;)V",
            4,
            &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
            None,
            None,
            h,
        );
        assert!(!r.success, "no type check means no aastore");

        // A missing `aaload`.
        let mut h = r9w19_helpers();
        h.aaload = 0;
        let r = compile_refs("([LX;I)LX;", 3, &[0x2a, 0x1b, 0x32, 0xb0], None, None, h);
        assert!(!r.success, "an unwired jit_aaload must refuse");

        // A missing `putfield_object`.
        let mut h = r9w19_helpers();
        h.putfield_object = 0;
        let r = compile_refs(
            "(LX;LY;)V",
            2,
            &[0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1],
            Some(int_field(0, b'L', false)),
            None,
            h,
        );
        assert!(!r.success, "an unwired jit_putfield_object must refuse");

        // A non-empty exception table stops both array forms: their guards
        // throw, and the drain throws with an unknown pc.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_helpers(r9w19_helpers());
        b.set_method_descriptor("([LX;I)LX;", true);
        b.set_exception_table_empty(false);
        let r = b.compile_method(3, 2, 4, &[0x2a, 0x1b, 0x32, 0xb0]);
        assert!(!r.success, "aaload needs an empty exception table");
    }

    /// `athrow` hands the exception and the bci to the helper and leaves
    /// through the epilogue. No spill and no map: the operand stack is dead
    /// the moment the exception is raised.
    #[test]
    fn r9w20_athrow_calls_the_helper_and_leaves() {
        const THROW: usize = 0x7FFF_0001_1000;
        // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.throw_exception = THROW;
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_helpers(h);
        b.set_method_descriptor("(LX;)V", true);
        b.set_exception_table_empty(true);
        // aload_0; athrow
        let r = b.compile_method(2, 1, 4, &[0x2a, 0xbf]);
        assert!(r.success, "athrow must compile: {:?}", r.instructions);
        assert!(
            r.pending_oop_maps.is_empty(),
            "a frame that is leaving needs no map"
        );
        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == THROW as i64
                )
            })
            .expect("the throw helper is called");
        assert!(
            r.instructions[..call].iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X1 && *imm == 1
            )),
            "the bci is the second argument"
        );
        // The helper RETURNS the sentinel, so nothing may overwrite X0 between
        // the call and the epilogue.
        let after = &r.instructions[call..];
        let blr = after
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .expect("the call is emitted");
        let branch = after[blr..]
            .iter()
            .position(|i| matches!(i, Arm64Instruction::B { .. }))
            .expect("it leaves through the epilogue");
        assert_eq!(
            branch,
            1,
            "the branch to the epilogue follows the call directly: {:?}",
            &after[blr..]
        );

        // Unwired: refuses regardless of the exception table.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_method_descriptor("(LX;)V", true);
        b.set_exception_table_empty(true);
        assert!(!b.compile_method(2, 1, 4, &[0x2a, 0xbf]).success);

        // Wired, with a handler in this frame: `jit_throw_exception` takes the
        // bci as its own argument and stamps the same TLS cell
        // `emit_stamp_throw_bci` writes for every other trapping lowering, so
        // the drain has an exact throw site regardless of table emptiness --
        // athrow needs no `can_route_exception` check at all.
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_helpers(h);
        b.set_method_descriptor("(LX;)V", true);
        b.set_exception_table_empty(false);
        assert!(
            b.compile_method(2, 1, 4, &[0x2a, 0xbf]).success,
            "a non-empty exception table no longer refuses athrow"
        );
    }

    // ---------------------------------------------------------------------------
    // EXECUTION. Only compiled on aarch64, where the emitted bytes are native.
    // ---------------------------------------------------------------------------

    /// Tests that actually RUN the code this backend emits.
    ///
    /// Everything else in this file asserts encodings and pseudo-op structure,
    /// which is all a non-aarch64 host can prove. These are the ones that turn that
    /// construction into evidence, and they exist because nothing in this
    /// repository could execute them until an aarch64 container was stood up.
    #[cfg(target_arch = "aarch64")]
    mod arm64_execution {
        use super::*;
        use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

        /// The safepoint flag the emitted poll reads. ONE byte, like the
        /// `AtomicBool` the real `GcBarrier` exposes.
        static TEST_SP_FLAG: AtomicU8 = AtomicU8::new(0);
        /// Bumped by the slow path so a taken poll is observable.
        static SLOW_PATH_HITS: AtomicU64 = AtomicU64::new(0);

        extern "C" fn test_slow_path() {
            SLOW_PATH_HITS.fetch_add(1, Ordering::SeqCst);
        }

        /// These tests must not run concurrently, for two independent reasons.
        ///
        /// They share `TEST_SP_FLAG`, so one test's `store` decides another's
        /// control flow. And they WRITE THEN EXECUTE code: under qemu-user (the
        /// only way this file gets run at all today) a buffer being written while
        /// another thread executes from a neighbouring mapping can leave stale
        /// translation blocks, which surfaces as `SIGILL` in a test whose own
        /// codegen is fine. Serialising removes both, and costs nothing -- there
        /// are three of them.
        static EXEC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn exec_guard() -> std::sync::MutexGuard<'static, ()> {
            EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// `iload_0; iload_1; iadd; ireturn`
        const IADD: [u8; 4] = [0x1a, 0x1b, 0x60, 0xac];

        fn call2(cm: &crate::CompiledMethod, a: i64, b: i64) -> i64 {
            // SAFETY: `cm` is a finalized artifact for a static (II)I method, so
            // the entry is an `extern "C" fn(i64, i64) -> i64`.
            unsafe { cm.try_call(&[a, b]) }.expect("the compiled method is callable")
        }

        /// The emitted code runs at all.
        #[test]
        fn a_compiled_leaf_method_executes_and_returns_the_right_value() {
            let _serial = exec_guard();
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success, "iadd must compile");
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, 7, 35), 42);
            assert_eq!(call2(&cm, -1, 1), 0);
        }

        /// THE POLL SEQUENCE EXECUTES, and takes the not-taken path when the flag
        /// is clear.
        ///
        /// This is the `MOVZ/MOVK; LDRB; CBZ` sequence whose encoding is asserted
        /// by `the_poll_reads_one_byte_and_the_encoding_says_so`. Asserting the
        /// word is not the same as running it: this proves the flag is read at the
        /// right width and address and that a clear flag branches PAST the call
        /// rather than into it.
        #[test]
        fn a_clear_flag_skips_the_slow_path() {
            let _serial = exec_guard();
            TEST_SP_FLAG.store(0, Ordering::SeqCst);
            let before = SLOW_PATH_HITS.load(Ordering::SeqCst);

            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(true);
            // SAFETY: zeroed helper table, then two real addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.safepoint_flag_addr = TEST_SP_FLAG.as_ptr() as usize;
            h.safepoint_slow_path = test_slow_path as usize;
            b.set_helpers(h);

            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success);
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, 20, 22), 42, "the method still computes");
            assert_eq!(
                SLOW_PATH_HITS.load(Ordering::SeqCst),
                before,
                "a clear flag must not call the slow path"
            );
        }

        /// THE TAKEN PATH RUNS, AND THE ARGUMENTS SURVIVE IT.
        ///
        /// The poll's `BLR` clobbers X0-X7 by the AAPCS64 contract, and this
        /// method's parameters arrive there. The entry poll was originally emitted
        /// from the END of the prologue -- BEFORE `compile_pass` copies the
        /// arguments into their local registers -- so on this path every parameter
        /// would have been garbage. That was found by reading and fixed by moving
        /// the poll past the copy; this is the test that would have CAUGHT it, and
        /// it is the first thing in this backend's history that could.
        #[test]
        fn a_set_flag_calls_the_slow_path_and_the_arguments_survive() {
            let _serial = exec_guard();
            TEST_SP_FLAG.store(1, Ordering::SeqCst);
            let before = SLOW_PATH_HITS.load(Ordering::SeqCst);

            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(true);
            // SAFETY: as above.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.safepoint_flag_addr = TEST_SP_FLAG.as_ptr() as usize;
            h.safepoint_slow_path = test_slow_path as usize;
            b.set_helpers(h);

            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success);
            let cm = publish_compiled_method(&result).expect("publishes");

            let got = call2(&cm, 7, 35);
            assert!(
                SLOW_PATH_HITS.load(Ordering::SeqCst) > before,
                "a set flag must reach the slow path -- otherwise this test proves \
             nothing about the taken path"
            );
            assert_eq!(
                got, 42,
                "the arguments must survive the poll's call; X0-X7 are caller-saved"
            );

            TEST_SP_FLAG.store(0, Ordering::SeqCst);
        }

        /// Compile `code` as a static method with `descriptor`, polls off.
        fn compile_static(
            descriptor: &str,
            locals: usize,
            stack: usize,
            code: &[u8],
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            b.set_method_descriptor(descriptor, true);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, stack, code);
            assert!(result.success, "{descriptor} must compile");
            publish_compiled_method(&result).expect("publishes")
        }

        // -- Round 9 waves 12-13: the memory lowerings, EXECUTED ------------
        //
        // These are the only evidence that exists for them. Every other test
        // in this file asserts pseudo-ops or instruction words; these build a
        // real object header in memory, run the compiled body against it, and
        // check the VALUE -- including the two paths that leave through a
        // throw stub, which no encoding assertion can reach.

        /// What the fake `jit_npe_with_action` recorded, and how often.
        static NPE_CALLS: AtomicU64 = AtomicU64::new(0);
        static NPE_ACTION: AtomicU64 = AtomicU64::new(0);
        /// ...and the fake `jit_throw_aioobe`'s four arguments.
        static AIOOBE_CALLS: AtomicU64 = AtomicU64::new(0);
        static AIOOBE_ARGS: [AtomicU64; 4] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];

        extern "C" fn test_npe(code: i64) {
            NPE_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: the packed action word, kept as its bit pattern.
            NPE_ACTION.store(code as u64, Ordering::SeqCst);
        }

        /// The real helper returns the `i64::MIN` deopt sentinel, and the
        /// AIOOBE stub relies on that rather than materializing one itself.
        extern "C" fn test_aioobe(index: i64, length: i64, array: i64, bci: i64) -> i64 {
            AIOOBE_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: four i64 arguments, kept as their bit patterns.
            for (slot, v) in AIOOBE_ARGS.iter().zip([index, length, array, bci]) {
                slot.store(v as u64, Ordering::SeqCst);
            }
            i64::MIN
        }

        /// A heap object, 8-aligned, with the real header layout: `class_id`
        /// at 0, the array length (`shape`) at [`ARRAY_LENGTH_OFFSET`], the
        /// mark word (whose byte 6 is the kind/element tag) at
        /// [`MARK_WORD_OFFSET`], and the data area at [`ARRAY_DATA_OFFSET`].
        ///
        /// `Vec<u64>` for the alignment: the compiled `LDR X` of a `long[]`
        /// element is not an unaligned-access test.
        struct FakeArray {
            words: Vec<u64>,
        }

        impl FakeArray {
            fn new(length: u32, data_bytes: usize, kind_tag: u8) -> Self {
                let total = cratonvm_types::ARRAY_DATA_OFFSET + data_bytes;
                let mut me = FakeArray {
                    words: vec![0u64; total.div_ceil(8) + 1],
                };
                // SAFETY: `words` owns at least `total` bytes and every write
                // below is inside the header or the data area.
                unsafe {
                    let base = me.base();
                    std::ptr::write_unaligned(
                        base.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *mut u32,
                        length,
                    );
                    std::ptr::write_unaligned(
                        base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET),
                        kind_tag,
                    );
                }
                me
            }

            fn base(&mut self) -> *mut u8 {
                self.words.as_mut_ptr().cast()
            }

            /// The object's address, as compiled code receives it.
            fn addr(&mut self) -> i64 {
                // Cast: a real mapped address, well under i64::MAX.
                self.base() as i64
            }

            /// Write one element of `T` at index `i`.
            fn set<T: Copy>(&mut self, i: usize, v: T) {
                // SAFETY: the data area was sized for these elements.
                unsafe {
                    let p = self
                        .base()
                        .add(cratonvm_types::ARRAY_DATA_OFFSET + i * std::mem::size_of::<T>());
                    std::ptr::write_unaligned(p as *mut T, v);
                }
            }

            /// Read one element of `T` back.
            fn get<T: Copy>(&mut self, i: usize) -> T {
                // SAFETY: as above.
                unsafe {
                    let p = self
                        .base()
                        .add(cratonvm_types::ARRAY_DATA_OFFSET + i * std::mem::size_of::<T>());
                    std::ptr::read_unaligned(p as *const T)
                }
            }
        }

        /// Compile `code` with both array throw paths pointed at the fakes
        /// above, and the caller's word that the exception table is empty.
        fn compile_with_array_paths(
            descriptor: &str,
            locals: usize,
            stack: usize,
            code: &[u8],
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then two real function addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.jit_npe_with_action = test_npe as usize;
            h.throw_aioobe = test_aioobe as usize;
            b.set_helpers(h);
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, stack, code);
            assert!(result.success, "{descriptor} must compile");
            publish_compiled_method(&result).expect("publishes")
        }

        /// `arraylength` reads the real header word, and a null receiver
        /// leaves through the NPE stub with this opcode's own action code.
        #[test]
        fn r9w12_arraylength_executes_and_a_null_receiver_reaches_the_stub() {
            let _serial = exec_guard();
            // aload_0; arraylength; ireturn
            let cm = compile_with_array_paths("([I)I", 1, 2, &[0x2a, 0xbe, 0xac]);
            let mut arr = FakeArray::new(7, 7 * 4, 0);
            // SAFETY: a static `([I)I`, so the entry is `fn(i64) -> i64`.
            let len = unsafe { cm.try_call(&[arr.addr()]) }.expect("callable");
            assert_eq!(len, 7, "the length comes out of the header word");

            let before = NPE_CALLS.load(Ordering::SeqCst);
            // SAFETY: as above; a null receiver is what the stub is for.
            let got = unsafe { cm.try_call(&[0]) }.expect("callable");
            assert_eq!(
                NPE_CALLS.load(Ordering::SeqCst),
                before + 1,
                "a null array must reach the NPE stub"
            );
            assert_eq!(
                NPE_ACTION.load(Ordering::SeqCst),
                u64::from(npe_action::ARRAY_LENGTH),
                "and report arraylength's own JEP-358 action"
            );
            assert_eq!(
                got,
                i64::MIN,
                "the stub owes the deopt sentinel, which the helper does not return"
            );
        }

        /// `iaload` reads the element the header says is there, an
        /// out-of-range index reaches the AIOOBE stub with all four arguments,
        /// and a NEGATIVE index reaches it through the same single branch.
        #[test]
        fn r9w13_an_int_array_load_executes_and_both_out_of_range_directions_trap() {
            let _serial = exec_guard();
            // aload_0; iload_1; iaload; ireturn
            let cm = compile_with_array_paths("([II)I", 2, 3, &[0x2a, 0x1b, 0x2e, 0xac]);
            let mut arr = FakeArray::new(4, 4 * 4, 0);
            for i in 0..4usize {
                // Cast: a small test value.
                arr.set::<i32>(i, (i as i32 + 1) * 100);
            }
            for i in 0..4i64 {
                // SAFETY: a static `([II)I`, so the entry is `fn(i64, i64) -> i64`.
                let got = unsafe { cm.try_call(&[arr.addr(), i]) }.expect("callable");
                assert_eq!(got, (i + 1) * 100, "element {i}");
            }

            for (idx, why) in [(4i64, "past the end"), (-1, "negative")] {
                let before = AIOOBE_CALLS.load(Ordering::SeqCst);
                // SAFETY: as above.
                let got = unsafe { cm.try_call(&[arr.addr(), idx]) }.expect("callable");
                assert_eq!(
                    AIOOBE_CALLS.load(Ordering::SeqCst),
                    before + 1,
                    "{why}: one unsigned compare must catch it"
                );
                let args: Vec<u64> = AIOOBE_ARGS
                    .iter()
                    .map(|a| a.load(Ordering::SeqCst))
                    .collect();
                // Cast: the i64 arguments as the statics kept them.
                assert_eq!(args[0], idx as u64, "{why}: the index");
                assert_eq!(args[1], 4, "{why}: the length");
                assert_eq!(args[2], arr.addr() as u64, "{why}: the array");
                assert_eq!(args[3], 2, "{why}: this site's bci");
                assert_eq!(got, i64::MIN, "{why}: the helper's sentinel comes back");
            }
        }

        /// The sub-word loads narrow the way JVMS says: `baload` and `saload`
        /// sign-extend, `caload` does not.
        #[test]
        fn r9w13_sub_word_array_loads_narrow_as_the_jvms_requires() {
            let _serial = exec_guard();
            // aload_0; iload_1; <load>; ireturn
            let b = compile_with_array_paths("([BI)I", 2, 3, &[0x2a, 0x1b, 0x33, 0xac]);
            let mut bytes = FakeArray::new(2, 2, 0);
            bytes.set::<u8>(0, 0xFF);
            bytes.set::<u8>(1, 0x7F);
            // SAFETY: a static `([BI)I`, so the entry is `fn(i64, i64) -> i64`.
            assert_eq!(
                unsafe { b.try_call(&[bytes.addr(), 0]) }.expect("callable"),
                -1,
                "baload sign-extends a byte[] element"
            );
            assert_eq!(
                unsafe { b.try_call(&[bytes.addr(), 1]) }.expect("callable"),
                127
            );

            let c = compile_with_array_paths("([CI)I", 2, 3, &[0x2a, 0x1b, 0x34, 0xac]);
            let s = compile_with_array_paths("([SI)I", 2, 3, &[0x2a, 0x1b, 0x35, 0xac]);
            let mut halves = FakeArray::new(1, 2, 0);
            halves.set::<u16>(0, 0xFFFF);
            // SAFETY: as above.
            assert_eq!(
                unsafe { c.try_call(&[halves.addr(), 0]) }.expect("callable"),
                65535,
                "caload is unsigned"
            );
            // SAFETY: as above.
            assert_eq!(
                unsafe { s.try_call(&[halves.addr(), 0]) }.expect("callable"),
                -1,
                "saload sign-extends"
            );
        }

        /// `iastore`/`lastore` write through the scaled address, and the
        /// element really lands where the layout says.
        #[test]
        fn r9w13_array_stores_execute_at_the_right_element() {
            let _serial = exec_guard();
            // aload_0; iload_1; iload_2; iastore; return
            let ints = compile_with_array_paths("([III)V", 3, 4, &[0x2a, 0x1b, 0x1c, 0x4f, 0xb1]);
            let mut arr = FakeArray::new(4, 4 * 4, 0);
            for i in 0..4i64 {
                // SAFETY: a static `([III)V`, entry `fn(i64, i64, i64) -> i64`.
                unsafe { ints.try_call(&[arr.addr(), i, (i + 1) * 7]) }.expect("callable");
            }
            for i in 0..4usize {
                // Cast: a small test value.
                assert_eq!(arr.get::<i32>(i), (i as i32 + 1) * 7, "element {i}");
            }

            // aload_0; iload_1; lload_2; lastore; return
            let longs = compile_with_array_paths("([JIJ)V", 4, 5, &[0x2a, 0x1b, 0x20, 0x50, 0xb1]);
            let mut wide = FakeArray::new(3, 3 * 8, 0);
            // SAFETY: a static `([JIJ)V`, entry `fn(i64, i64, i64) -> i64`.
            unsafe { longs.try_call(&[wide.addr(), 2, i64::MIN + 1]) }.expect("callable");
            assert_eq!(
                wide.get::<i64>(2),
                i64::MIN + 1,
                "a long element is scaled by 8, not by 4"
            );
            assert_eq!(wide.get::<i64>(0), 0, "and nothing else was written");
        }

        /// THE `bastore` MASK, RUN. A `boolean[]` keeps `value & 1`; a
        /// `byte[]` keeps the low byte. One opcode, one compiled body, and the
        /// answer comes from the array header's own kind/element byte.
        #[test]
        fn r9w13_bastore_masks_a_boolean_array_and_truncates_a_byte_array() {
            let _serial = exec_guard();
            // aload_0; iload_1; iload_2; bastore; return
            let cm = compile_with_array_paths("([BII)V", 3, 4, &[0x2a, 0x1b, 0x1c, 0x54, 0xb1]);
            let bool_tag =
                cratonvm_types::primitive_array_kind_tags_byte("[Z").expect("[Z has a tag");
            let byte_tag =
                cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B has a tag");
            assert_ne!(bool_tag, byte_tag, "the test needs the two to differ");

            let mut booleans = FakeArray::new(1, 1, bool_tag);
            let mut bytes = FakeArray::new(1, 1, byte_tag);
            for (arr, want, why) in [
                (&mut booleans, 1u8, "a boolean[] narrows by & 1"),
                (&mut bytes, 3, "a byte[] keeps its low byte"),
            ] {
                // SAFETY: a static `([BII)V`, entry `fn(i64, i64, i64) -> i64`.
                unsafe { cm.try_call(&[arr.addr(), 0, 3]) }.expect("callable");
                assert_eq!(arr.get::<u8>(0), want, "{why}");
            }
        }

        /// A `float[]` element survives the round trip through a general
        /// register and back, bit-exactly.
        #[test]
        fn r9w13_a_float_array_element_round_trips_through_the_bit_move() {
            let _serial = exec_guard();
            // aload_0; iload_1; fload_2 (0x24 -- 0x22 is fload_0); fastore; return
            let store = compile_with_array_paths("([FIF)V", 3, 4, &[0x2a, 0x1b, 0x24, 0x51, 0xb1]);
            // aload_0; iload_1; faload; freturn
            let load = compile_with_array_paths("([FI)F", 2, 3, &[0x2a, 0x1b, 0x30, 0xae]);
            let mut arr = FakeArray::new(2, 2 * 4, 0);
            let bits = i64::from(f32::to_bits(-2.5f32));
            // SAFETY: a static `([FIF)V`; the VM passes a float as its bits.
            unsafe { store.try_call(&[arr.addr(), 1, bits]) }.expect("callable");
            assert_eq!(arr.get::<f32>(1), -2.5f32);
            assert_eq!(arr.get::<f32>(0), 0.0, "and nothing else was written");
            // SAFETY: a static `([FI)F`; the VM reads the result as bits.
            let got = unsafe { load.try_call(&[arr.addr(), 1]) }.expect("callable");
            // Truncation: a `float` result occupies the low 32 bits of X0.
            assert_eq!(f32::from_bits(got as u32), -2.5f32);
        }

        /// `iadd` wraps at 32 bits, and the result comes back sign-extended.
        ///
        /// Formerly `iadd_does_not_wrap_at_32_bits_and_this_is_a_bug`, which pinned
        /// the 64-bit answer while the correct assertion sat `#[ignore]`d beside
        /// it. JVMS 6.5 `iadd`: "the result is the 32 low-order bits of the true
        /// mathematical result".
        #[test]
        fn iadd_wraps_at_32_bits_as_the_jvms_requires() {
            let _serial = exec_guard();
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success);
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, i64::from(i32::MAX), 1), i64::from(i32::MIN));
        }

        /// `ishl` masks its distance to five bits.
        ///
        /// Formerly `ishl_does_not_mask_the_shift_to_five_bits_and_this_is_a_bug`.
        /// JVMS 6.5 `ishl`: the distance is "the value of the low 5 bits", so
        /// `1 << 32` is `1` and `1 << 31` is `Integer.MIN_VALUE`.
        #[test]
        fn ishl_masks_the_shift_to_five_bits() {
            let _serial = exec_guard();
            // iload_0; iload_1; ishl; ireturn
            let code = [0x1a, 0x1b, 0x78, 0xac];
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            let result = b.compile_method(2, 2, 4, &code);
            assert!(result.success, "ishl must compile");
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, 1, 32), 1);
            assert_eq!(call2(&cm, 1, 33), 2);
            assert_eq!(call2(&cm, 1, 31), i64::from(i32::MIN));
        }

        /// The reported miscompile: `static int f(int a, int b) { return a -
        /// (b+1+2+3+4); }` returned -4 for `f(100, 5)`, because the scratch
        /// allocator wrapped onto a live register and popping the new value
        /// reloaded the old one.
        #[test]
        fn the_expression_that_returned_minus_four_returns_85() {
            let _serial = exec_guard();
            let code = [
                0x1a, 0x1b, 0x04, 0x60, 0x05, 0x60, 0x06, 0x60, 0x07, 0x60, 0x64, 0xac,
            ];
            let cm = compile_static("(II)I", 2, 3, &code);
            assert_eq!(call2(&cm, 100, 5), 85);
        }

        /// `dreturn` hands back the double's bits in X0 through the epilogue, and a
        /// `double` parameter is homed in its frame slot. Both were broken: the
        /// return discarded the value with a bare `RET`, and a frame-homed
        /// parameter was never stored.
        #[test]
        fn a_double_parameter_round_trips_through_dreturn() {
            let _serial = exec_guard();
            // static double f(double a, double b) { return b; }  -- dload_2; dreturn
            let cm = compile_static("(DD)D", 4, 2, &[0x28, 0xaf]);
            let got = call2(&cm, 1.5f64.to_bits() as i64, (-2.25f64).to_bits() as i64);
            assert_eq!(f64::from_bits(got as u64), -2.25);
        }

        /// `fcmpg` of NaN is +1 and `fcmpl` of NaN is -1.
        #[test]
        fn nan_compares_land_where_the_jvms_puts_them() {
            let _serial = exec_guard();
            let nan = i64::from(f32::NAN.to_bits());
            let one = i64::from(1.0f32.to_bits());
            // fload_0; fload_1; fcmpg|fcmpl; ireturn
            let g = compile_static("(FF)I", 2, 2, &[0x22, 0x23, 0x96, 0xac]);
            let l = compile_static("(FF)I", 2, 2, &[0x22, 0x23, 0x95, 0xac]);
            assert_eq!(call2(&g, nan, one), 1);
            assert_eq!(call2(&l, nan, one), -1);
            assert_eq!(call2(&g, i64::from(0.5f32.to_bits()), one), -1);
            assert_eq!(call2(&l, one, one), 0);
        }

        /// JVMS §6.5 `ireturn` narrowing, executed: `iload_0; ireturn` with a
        /// narrow declared return hands back the narrowed, sign-extended value.
        #[test]
        fn ireturn_narrows_to_the_declared_return_type() {
            let _serial = exec_guard();
            let code = [0x1a, 0xac];
            let z = compile_static("(II)Z", 2, 1, &code);
            let b = compile_static("(II)B", 2, 1, &code);
            let c = compile_static("(II)C", 2, 1, &code);
            let s = compile_static("(II)S", 2, 1, &code);
            assert_eq!(call2(&z, 2, 0), 0);
            assert_eq!(call2(&z, 3, 0), 1);
            assert_eq!(call2(&b, 0x1FF, 0), -1);
            assert_eq!(call2(&c, -1, 0), 0xFFFF);
            assert_eq!(call2(&s, 0x18000, 0), -32768);
        }
        // -- Round 9 wave 15: the mid-method helper call, EXECUTED ----------
        //
        // The wave-12/13 lowerings above leave the frame when they call. This
        // one comes BACK, which is the whole difficulty: the operand stack has
        // to be where the model says it is on the far side of a call that
        // destroys every register it lives in. No encoding assertion can show
        // that; these run it.

        /// What the fake `jit_putfield_int` recorded, and how often.
        static PUTFIELD_CALLS: AtomicU64 = AtomicU64::new(0);
        static PUTFIELD_ARGS: [AtomicU64; 3] =
            [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

        /// The four real `jit_putfield_*` helpers share this shape:
        /// `(obj_ptr, field_index, val) -> ()`, with a `float`/`double`
        /// arriving as its bit pattern in an INTEGER register.
        extern "C" fn test_putfield(obj: i64, index: i64, val: i64) {
            PUTFIELD_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: three i64 arguments, kept as their bit patterns.
            for (slot, v) in PUTFIELD_ARGS.iter().zip([obj, index, val]) {
                slot.store(v as u64, Ordering::SeqCst);
            }
        }

        /// Compile `code` with the NPE path and all four `jit_putfield_*`
        /// slots pointed at [`test_putfield`], and `field` resolved at every
        /// instance-field site.
        fn compile_with_putfield(
            descriptor: &str,
            locals: usize,
            stack: usize,
            code: &[u8],
            field: Arm64InstanceField,
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then two real function addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.jit_npe_with_action = test_npe as usize;
            h.putfield_int = test_putfield as usize;
            h.putfield_long = test_putfield as usize;
            h.putfield_float = test_putfield as usize;
            h.putfield_double = test_putfield as usize;
            b.set_helpers(h);
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            let mut map = HashMap::new();
            for (pc, _op, _cp) in instance_field_sites(code) {
                map.insert(pc, field);
            }
            b.set_instance_field_info(map);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, stack, code);
            assert!(result.success, "{descriptor} must compile");
            publish_compiled_method(&result).expect("publishes")
        }

        fn putfield_args() -> (u64, u64, u64) {
            (
                PUTFIELD_ARGS[0].load(Ordering::SeqCst),
                PUTFIELD_ARGS[1].load(Ordering::SeqCst),
                PUTFIELD_ARGS[2].load(Ordering::SeqCst),
            )
        }

        /// `putfield` of every primitive width reaches the helper with the
        /// receiver, the resolved slot index and the VALUE -- a `float` and a
        /// `double` as their bit patterns, because the helpers take all three
        /// arguments in integer registers.
        #[test]
        fn r9w15_putfield_reaches_the_helper_with_its_three_arguments() {
            let _serial = exec_guard();
            // aload_0; <load_1>; putfield #3; return
            for (tag, descriptor, load, arg, want) in [
                (b'I', "(LX;I)V", 0x1bu8, 0x7fff_0000i64, 0x7fff_0000u64),
                (b'J', "(LX;J)V", 0x1f, -3, (-3i64) as u64),
                (
                    b'F',
                    "(LX;F)V",
                    0x23,
                    i64::from(1.5f32.to_bits()),
                    u64::from(1.5f32.to_bits()),
                ),
                (
                    b'D',
                    "(LX;D)V",
                    0x27,
                    // Cast: the bit pattern, which is how the VM passes a
                    // double to a compiled entry.
                    2.5f64.to_bits() as i64,
                    2.5f64.to_bits(),
                ),
            ] {
                let code = [0x2a, load, 0xb5, 0x00, 0x03, 0xb1];
                let cm = compile_with_putfield(
                    descriptor,
                    3,
                    4,
                    &code,
                    Arm64InstanceField {
                        field_index: 9,
                        type_tag: tag,
                        is_volatile: false,
                    },
                );
                let before = PUTFIELD_CALLS.load(Ordering::SeqCst);
                // A receiver that is merely non-null: the helper is a fake and
                // never dereferences it.
                let receiver = 0x1234_5678i64;
                // SAFETY: a static method of `descriptor`, so the entry takes
                // one `i64` per argument.
                unsafe { cm.try_call(&[receiver, arg]) }.expect("callable");
                assert_eq!(
                    PUTFIELD_CALLS.load(Ordering::SeqCst),
                    before + 1,
                    "'{}': the helper must be called exactly once",
                    char::from(tag)
                );
                let (obj, index, val) = putfield_args();
                // Cast: the receiver, as its bit pattern.
                assert_eq!(obj, receiver as u64, "'{}': receiver", char::from(tag));
                assert_eq!(index, 9, "'{}': field index", char::from(tag));
                assert_eq!(val, want, "'{}': value", char::from(tag));
            }
        }

        /// THE OPERAND STACK SURVIVES THE CALL.
        ///
        /// `iload_1` leaves a value in a caller-saved scratch register, the
        /// `putfield` calls a helper that is free to destroy it, and the
        /// `ireturn` reads it afterwards. Without the spill and the reload
        /// this returns whatever the helper happened to leave behind -- and
        /// because that is a legitimate `int`, nothing downstream would flag
        /// it. This is the test the wave exists for.
        #[test]
        fn r9w15_an_operand_live_across_the_call_comes_back() {
            let _serial = exec_guard();
            // iload_1; aload_0; iload_1; putfield #3; ireturn
            let code = [0x1b, 0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xac];
            let cm = compile_with_putfield(
                "(LX;I)I",
                2,
                4,
                &code,
                Arm64InstanceField {
                    field_index: 2,
                    type_tag: b'I',
                    is_volatile: false,
                },
            );
            // SAFETY: a static `(LX;I)I`, so the entry is `fn(i64, i64) -> i64`.
            let got = unsafe { cm.try_call(&[0x1234_5678, 4242]) }.expect("callable");
            assert_eq!(
                got, 4242,
                "the operand under the putfield must survive the helper call"
            );
            let (_, _, val) = putfield_args();
            assert_eq!(val, 4242, "and the same value reaches the helper");
        }

        /// A null receiver leaves through the NPE stub with
        /// `npe_action::NONE` and the deopt sentinel, and the helper is NOT
        /// called -- which is the bug this null check exists to prevent:
        /// `jit_putfield_int`'s own guard returns without raising, so a
        /// `putfield` on null through it silently drops the store.
        #[test]
        fn r9w15_a_null_receiver_throws_instead_of_dropping_the_store() {
            let _serial = exec_guard();
            let code = [0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1];
            let cm = compile_with_putfield(
                "(LX;I)V",
                2,
                4,
                &code,
                Arm64InstanceField {
                    field_index: 0,
                    type_tag: b'I',
                    is_volatile: false,
                },
            );
            let npe_before = NPE_CALLS.load(Ordering::SeqCst);
            let put_before = PUTFIELD_CALLS.load(Ordering::SeqCst);
            // SAFETY: as above; a null receiver is what the stub is for.
            let got = unsafe { cm.try_call(&[0, 7]) }.expect("callable");
            assert_eq!(
                NPE_CALLS.load(Ordering::SeqCst),
                npe_before + 1,
                "a null receiver must reach the NPE stub"
            );
            assert_eq!(
                NPE_ACTION.load(Ordering::SeqCst),
                u64::from(npe_action::NONE),
                "JEP 358 has no field-access action, so the code is NONE"
            );
            assert_eq!(
                PUTFIELD_CALLS.load(Ordering::SeqCst),
                put_before,
                "and the store must NOT happen"
            );
            assert_eq!(got, i64::MIN, "the stub owes the deopt sentinel");
        }

        /// The JVMS §6.5 narrowing runs on the VALUE, not just in the
        /// comments: a `byte` field handed 300 stores 44.
        #[test]
        fn r9w15_putfield_narrows_the_value_to_the_declared_field_type() {
            let _serial = exec_guard();
            let code = [0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1];
            for (tag, given, want) in [
                (b'B', 300i64, 44u64),
                (b'S', 70_000, 4464),
                (b'C', -1, 0xFFFF),
                (b'Z', 2, 0),
                (b'I', 300, 300),
            ] {
                let cm = compile_with_putfield(
                    "(LX;I)V",
                    2,
                    4,
                    &code,
                    Arm64InstanceField {
                        field_index: 1,
                        type_tag: tag,
                        is_volatile: false,
                    },
                );
                // SAFETY: a static `(LX;I)V`, so the entry is `fn(i64, i64)`.
                unsafe { cm.try_call(&[0x1234_5678, given]) }.expect("callable");
                let (_, _, val) = putfield_args();
                assert_eq!(
                    val,
                    want,
                    "'{}' field given {given} must store {want}",
                    char::from(tag)
                );
            }
        }

        /// A `volatile` field still reaches the helper with the same three
        /// arguments -- i.e. the two `DMB ISH`s the ordering half adds are
        /// encodable and executable, not just emittable.
        #[test]
        fn r9w15_a_volatile_putfield_executes_through_its_fences() {
            let _serial = exec_guard();
            let code = [0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1];
            let cm = compile_with_putfield(
                "(LX;I)V",
                2,
                4,
                &code,
                Arm64InstanceField {
                    field_index: 4,
                    type_tag: b'I',
                    is_volatile: true,
                },
            );
            let before = PUTFIELD_CALLS.load(Ordering::SeqCst);
            // SAFETY: a static `(LX;I)V`, so the entry is `fn(i64, i64)`.
            unsafe { cm.try_call(&[0x1234_5678, 99]) }.expect("callable");
            assert_eq!(PUTFIELD_CALLS.load(Ordering::SeqCst), before + 1);
            let (_, index, val) = putfield_args();
            assert_eq!((index, val), (4, 99));
        }
        // -- Round 9 wave 16: the context ABI and `getfield`, EXECUTED ------

        /// What the fake `jit_getfield` was handed, what it returns, and how
        /// often it was called.
        static GETFIELD_CALLS: AtomicU64 = AtomicU64::new(0);
        static GETFIELD_ARGS: [AtomicU64; 3] =
            [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
        static GETFIELD_RESULT: AtomicU64 = AtomicU64::new(0);
        /// What the fake `dispatch_threw` answers, and how often it is asked.
        static THREW_CALLS: AtomicU64 = AtomicU64::new(0);
        static THREW_ANSWER: AtomicU64 = AtomicU64::new(0);

        extern "C" fn test_getfield(vm: i64, obj: i64, index: i64) -> i64 {
            GETFIELD_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: three i64 arguments, kept as their bit patterns.
            for (slot, v) in GETFIELD_ARGS.iter().zip([vm, obj, index]) {
                slot.store(v as u64, Ordering::SeqCst);
            }
            // Cast: the value the test staged, back as an i64.
            GETFIELD_RESULT.load(Ordering::SeqCst) as i64
        }

        extern "C" fn test_dispatch_threw() -> i64 {
            THREW_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: 0 or 1, staged by the test.
            THREW_ANSWER.load(Ordering::SeqCst) as i64
        }

        /// Compile `code` with the NPE path, `jit_getfield` and
        /// `dispatch_threw` pointed at the fakes above.
        fn compile_with_getfield(
            descriptor: &str,
            locals: usize,
            stack: usize,
            code: &[u8],
            field: Arm64InstanceField,
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then three real function addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.jit_npe_with_action = test_npe as usize;
            h.getfield = test_getfield as usize;
            h.dispatch_threw = test_dispatch_threw as usize;
            b.set_helpers(h);
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            let mut map = HashMap::new();
            for (pc, _op, _cp) in instance_field_sites(code) {
                map.insert(pc, field);
            }
            b.set_instance_field_info(map);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, stack, code);
            assert!(result.success, "{descriptor} must compile");
            let cm = publish_compiled_method(&result).expect("publishes");
            assert!(
                cm.needs_context(),
                "a body with a getfield must declare the context ABI"
            );
            cm
        }

        fn field(index: usize, tag: u8) -> Arm64InstanceField {
            Arm64InstanceField {
                field_index: index,
                type_tag: tag,
                is_volatile: false,
            }
        }

        /// THE CONTEXT ABI EXECUTES.
        ///
        /// The VM passes the context in X0 ahead of every Java argument, so a
        /// prologue that homed its first argument out of X0 would read the
        /// context pointer as the receiver -- the exact failure
        /// `CompiledMethod::try_call_with_context`'s doc warns about. This
        /// runs it: the helper sees the context the caller passed AND the
        /// receiver the caller passed, in the right registers.
        #[test]
        fn r9w16_the_context_and_the_arguments_arrive_in_the_right_registers() {
            let _serial = exec_guard();
            // aload_0; getfield #3; ireturn
            let cm = compile_with_getfield(
                "(LX;)I",
                2,
                4,
                &[0x2a, 0xb4, 0x00, 0x03, 0xac],
                field(6, b'I'),
            );
            GETFIELD_RESULT.store(1234, Ordering::SeqCst);
            let before = GETFIELD_CALLS.load(Ordering::SeqCst);
            let vm = 0x7777_0000i64;
            let receiver = 0x1234_5678i64;
            // SAFETY: a context-taking static `(LX;)I`, so the entry is
            // `fn(i64, i64) -> i64` with the context first.
            let got = unsafe { cm.try_call_with_context(vm, &[receiver]) }.expect("callable");
            assert_eq!(
                GETFIELD_CALLS.load(Ordering::SeqCst),
                before + 1,
                "the helper must be called once"
            );
            assert_eq!(
                (
                    GETFIELD_ARGS[0].load(Ordering::SeqCst),
                    GETFIELD_ARGS[1].load(Ordering::SeqCst),
                    GETFIELD_ARGS[2].load(Ordering::SeqCst),
                ),
                // Cast: the two pointers, as their bit patterns.
                (vm as u64, receiver as u64, 6),
                "(vm_ptr, obj_ptr, field_index), in that order"
            );
            assert_eq!(got, 1234, "and the value comes back as the result");
        }

        /// A null receiver leaves through the NPE stub and the helper is never
        /// reached -- even though `jit_getfield` would itself have raised.
        #[test]
        fn r9w16_a_null_receiver_never_reaches_the_getfield_helper() {
            let _serial = exec_guard();
            let cm = compile_with_getfield(
                "(LX;)I",
                2,
                4,
                &[0x2a, 0xb4, 0x00, 0x03, 0xac],
                field(0, b'I'),
            );
            GETFIELD_RESULT.store(9, Ordering::SeqCst);
            let npe_before = NPE_CALLS.load(Ordering::SeqCst);
            let get_before = GETFIELD_CALLS.load(Ordering::SeqCst);
            // SAFETY: as above; a null receiver is what the stub is for.
            let got = unsafe { cm.try_call_with_context(0x7777_0000, &[0]) }.expect("callable");
            assert_eq!(NPE_CALLS.load(Ordering::SeqCst), npe_before + 1);
            assert_eq!(
                NPE_ACTION.load(Ordering::SeqCst),
                u64::from(npe_action::NONE)
            );
            assert_eq!(GETFIELD_CALLS.load(Ordering::SeqCst), get_before);
            assert_eq!(got, i64::MIN, "the stub owes the deopt sentinel");
        }

        /// THE SENTINEL. An `int` field cannot legitimately return
        /// `i64::MIN`, so the helper returning it means an exception is
        /// pending and the body leaves with the sentinel.
        #[test]
        fn r9w16_the_sentinel_leaves_the_frame_for_an_unambiguous_tag() {
            let _serial = exec_guard();
            let cm = compile_with_getfield(
                "(LX;)I",
                2,
                4,
                &[0x2a, 0xb4, 0x00, 0x03, 0xac],
                field(0, b'I'),
            );
            let threw_before = THREW_CALLS.load(Ordering::SeqCst);
            GETFIELD_RESULT.store(i64::MIN as u64, Ordering::SeqCst);
            // SAFETY: as above.
            let got =
                unsafe { cm.try_call_with_context(0x7777_0000, &[0x1234_5678]) }.expect("callable");
            assert_eq!(got, i64::MIN, "the pending exception must reach the caller");
            assert_eq!(
                THREW_CALLS.load(Ordering::SeqCst),
                threw_before,
                "an int field owes no disambiguation -- i64::MIN is not a legal int"
            );

            // ...and an ordinary value goes straight through.
            GETFIELD_RESULT.store(42, Ordering::SeqCst);
            // SAFETY: as above.
            let got =
                unsafe { cm.try_call_with_context(0x7777_0000, &[0x1234_5678]) }.expect("callable");
            assert_eq!(got, 42);
        }

        /// THE AMBIGUOUS TAGS, WHICH ARE THE POINT OF `dispatch_threw`.
        ///
        /// A `long` field holding `Long.MIN_VALUE` -- and a `double` field
        /// holding `-0.0`, whose bit pattern IS `i64::MIN` -- return exactly
        /// the sentinel. Reading that as "an exception is pending" throws a
        /// `NullPointerException` out of a correct program, so these two ask,
        /// and KEEP the value when the answer is "nothing is pending".
        #[test]
        fn r9w16_long_min_value_and_negative_zero_survive_the_sentinel() {
            let _serial = exec_guard();
            for (tag, ret) in [(b'J', 0xadu8), (b'D', 0xaf)] {
                let cm = compile_with_getfield(
                    "(LX;)V",
                    2,
                    4,
                    &[0x2a, 0xb4, 0x00, 0x03, ret],
                    field(1, tag),
                );
                GETFIELD_RESULT.store(i64::MIN as u64, Ordering::SeqCst);

                // Nothing pending: the i64::MIN is the field's real value.
                THREW_ANSWER.store(0, Ordering::SeqCst);
                let threw_before = THREW_CALLS.load(Ordering::SeqCst);
                // SAFETY: a context-taking static, entry `fn(i64, i64) -> i64`.
                let got = unsafe { cm.try_call_with_context(0x7777_0000, &[0x1234_5678]) }
                    .expect("callable");
                assert_eq!(
                    THREW_CALLS.load(Ordering::SeqCst),
                    threw_before + 1,
                    "'{}': the ambiguity must be resolved by asking",
                    char::from(tag)
                );
                assert_eq!(
                    got,
                    i64::MIN,
                    "'{}': Long.MIN_VALUE / -0.0 must survive as a VALUE",
                    char::from(tag)
                );

                // A genuine pending signal: the body leaves.
                THREW_ANSWER.store(1, Ordering::SeqCst);
                // SAFETY: as above.
                let got = unsafe { cm.try_call_with_context(0x7777_0000, &[0x1234_5678]) }
                    .expect("callable");
                assert_eq!(
                    got,
                    i64::MIN,
                    "'{}': a real exception still reaches the caller",
                    char::from(tag)
                );
            }
            THREW_ANSWER.store(0, Ordering::SeqCst);
        }

        /// An operand live across a `getfield`'s helper call comes back, and
        /// the two calls of a `J` field's sentinel path do not disturb it
        /// either.
        #[test]
        fn r9w16_an_operand_live_across_a_getfield_comes_back() {
            let _serial = exec_guard();
            // iload_1; aload_0; getfield #3; iadd; ireturn
            let cm = compile_with_getfield(
                "(LX;I)I",
                3,
                4,
                &[0x1b, 0x2a, 0xb4, 0x00, 0x03, 0x60, 0xac],
                field(2, b'I'),
            );
            GETFIELD_RESULT.store(100, Ordering::SeqCst);
            // SAFETY: a context-taking static `(LX;I)I`, entry
            // `fn(i64, i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x7777_0000, &[0x1234_5678, 23]) }
                .expect("callable");
            assert_eq!(
                got, 123,
                "the operand under the getfield must survive the helper call"
            );
        }

        /// A `float` field's bits come back as a `float`, not as an integer:
        /// the helper hands back `f.to_bits() as i64` and the lowering owes
        /// the `FMOV` into the FP file.
        #[test]
        fn r9w16_a_float_field_comes_back_in_the_fp_file() {
            let _serial = exec_guard();
            // aload_0; getfield #3; freturn
            let cm = compile_with_getfield(
                "(LX;)F",
                2,
                4,
                &[0x2a, 0xb4, 0x00, 0x03, 0xae],
                field(3, b'F'),
            );
            GETFIELD_RESULT.store(u64::from(2.5f32.to_bits()), Ordering::SeqCst);
            // SAFETY: a context-taking static `(LX;)F`, entry
            // `fn(i64, i64) -> i64` returning the float's bits.
            let got =
                unsafe { cm.try_call_with_context(0x7777_0000, &[0x1234_5678]) }.expect("callable");
            // Cast: the low 32 bits are the float the method returned.
            assert_eq!(
                f32::from_bits(got as u32),
                2.5f32,
                "the bits must travel through the FP file and back"
            );
        }
        // -- Round 9 wave 17: `putstatic` through its helper, EXECUTED ------

        /// What the fake `jit_putstatic_*` was handed, how often, and what it
        /// returns (0 for success, `i64::MIN` for a failed `<clinit>`).
        static PUTSTATIC_CALLS: AtomicU64 = AtomicU64::new(0);
        static PUTSTATIC_ARGS: [AtomicU64; 4] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        static PUTSTATIC_RESULT: AtomicU64 = AtomicU64::new(0);

        extern "C" fn test_putstatic(vm: i64, class_id: i64, index: i64, val: i64) -> i64 {
            PUTSTATIC_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: four i64 arguments, kept as their bit patterns.
            for (slot, v) in PUTSTATIC_ARGS.iter().zip([vm, class_id, index, val]) {
                slot.store(v as u64, Ordering::SeqCst);
            }
            // Cast: 0 or the sentinel, staged by the test.
            PUTSTATIC_RESULT.load(Ordering::SeqCst) as i64
        }

        /// Compile `code` with all four `jit_putstatic_*` slots pointed at
        /// [`test_putstatic`] and `field` resolved at every static site.
        fn compile_with_putstatic(
            descriptor: &str,
            locals: usize,
            code: &[u8],
            field: Arm64StaticField,
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then one real function address in
            // four slots.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.putstatic_int = test_putstatic as usize;
            h.putstatic_long = test_putstatic as usize;
            h.putstatic_float = test_putstatic as usize;
            h.putstatic_double = test_putstatic as usize;
            b.set_helpers(h);
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            let mut map = HashMap::new();
            for (pc, _op, _cp) in static_field_sites(code) {
                map.insert(pc, field);
            }
            b.set_static_field_info(map);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, 4, code);
            assert!(result.success, "{descriptor} must compile");
            let cm = publish_compiled_method(&result).expect("publishes");
            assert!(cm.needs_context(), "a putstatic body takes the VM pointer");
            cm
        }

        fn a_static(class_id: u32, index: usize, tag: u8) -> Arm64StaticField {
            Arm64StaticField {
                class_id,
                field_index: index,
                // Not read by the lowering: the caller's proof that the class
                // is initialized. Any non-zero value stands for one here.
                base_cell: 0x1000,
                type_tag: tag,
                is_volatile: false,
            }
        }

        /// The four arguments arrive in order, and the JVMS narrowing runs on
        /// the value before they do.
        #[test]
        fn r9w17_putstatic_reaches_the_helper_with_its_four_arguments() {
            let _serial = exec_guard();
            PUTSTATIC_RESULT.store(0, Ordering::SeqCst);
            for (tag, descriptor, load, given, want) in [
                (b'I', "(I)V", 0x1au8, 77i64, 77u64),
                (b'B', "(I)V", 0x1a, 300, 44),
                (b'Z', "(I)V", 0x1a, 2, 0),
                (b'J', "(J)V", 0x1e, -5, (-5i64) as u64),
                (
                    b'F',
                    "(F)V",
                    0x22,
                    i64::from(0.5f32.to_bits()),
                    u64::from(0.5f32.to_bits()),
                ),
            ] {
                // <load_0>; putstatic #3; return
                let cm = compile_with_putstatic(
                    descriptor,
                    2,
                    &[load, 0xb3, 0x00, 0x03, 0xb1],
                    a_static(21, 3, tag),
                );
                let before = PUTSTATIC_CALLS.load(Ordering::SeqCst);
                let vm = 0x5555_0000i64;
                // SAFETY: a context-taking static, so the entry takes the
                // context and then one `i64` per argument.
                unsafe { cm.try_call_with_context(vm, &[given]) }.expect("callable");
                assert_eq!(
                    PUTSTATIC_CALLS.load(Ordering::SeqCst),
                    before + 1,
                    "'{}': the helper must be called once",
                    char::from(tag)
                );
                let got: Vec<u64> = PUTSTATIC_ARGS
                    .iter()
                    .map(|a| a.load(Ordering::SeqCst))
                    .collect();
                // Cast: the context pointer, as its bit pattern.
                assert_eq!(
                    got,
                    vec![vm as u64, 21, 3, want],
                    "'{}': (vm_ptr, class_id, field_index, value)",
                    char::from(tag)
                );
            }
        }

        /// The sentinel leaves the frame. `jit_putstatic_*` returns `i64::MIN`
        /// only after setting a pending exception, so this is a THROW rather
        /// than a re-run -- which is what lets this lowering exist without a
        /// deopt point.
        #[test]
        fn r9w17_a_failed_class_init_leaves_through_the_epilogue() {
            let _serial = exec_guard();
            let cm = compile_with_putstatic(
                "(I)I",
                2,
                // iload_0; putstatic #3; iconst_1; ireturn
                &[0x1a, 0xb3, 0x00, 0x03, 0x04, 0xac],
                a_static(21, 0, b'I'),
            );
            PUTSTATIC_RESULT.store(0, Ordering::SeqCst);
            // SAFETY: a context-taking static `(I)I`, entry
            // `fn(i64, i64) -> i64`.
            let ok = unsafe { cm.try_call_with_context(0x5555_0000, &[1]) }.expect("callable");
            assert_eq!(ok, 1, "a successful store falls through to the body");

            PUTSTATIC_RESULT.store(i64::MIN as u64, Ordering::SeqCst);
            // SAFETY: as above.
            let failed = unsafe { cm.try_call_with_context(0x5555_0000, &[1]) }.expect("callable");
            assert_eq!(
                failed,
                i64::MIN,
                "the sentinel must reach the caller, not be stored and ignored"
            );
            PUTSTATIC_RESULT.store(0, Ordering::SeqCst);
        }

        /// An operand live across a `putstatic` comes back, as it must across
        /// any of these calls.
        #[test]
        fn r9w17_an_operand_live_across_a_putstatic_comes_back() {
            let _serial = exec_guard();
            PUTSTATIC_RESULT.store(0, Ordering::SeqCst);
            let cm = compile_with_putstatic(
                "(I)I",
                2,
                // iload_0; iload_0; putstatic #3; ireturn
                &[0x1a, 0x1a, 0xb3, 0x00, 0x03, 0xac],
                a_static(21, 0, b'I'),
            );
            // SAFETY: a context-taking static `(I)I`, entry
            // `fn(i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x5555_0000, &[4242]) }.expect("callable");
            assert_eq!(got, 4242);
        }
        // -- Round 9 wave 18: allocation at a safepoint, EXECUTED -----------

        /// What the fake allocation helpers were handed, how often, and what
        /// they return (a fake object pointer, or 0 for "an error is pending").
        static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
        static ALLOC_ARGS: [AtomicU64; 3] =
            [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
        static ALLOC_RESULT: AtomicU64 = AtomicU64::new(0);

        extern "C" fn test_alloc(vm: i64, class_or_atype: i64, third: i64) -> i64 {
            ALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: three i64 arguments, kept as their bit patterns.
            for (slot, v) in ALLOC_ARGS.iter().zip([vm, class_or_atype, third]) {
                slot.store(v as u64, Ordering::SeqCst);
            }
            // Cast: the fake object pointer the test staged.
            ALLOC_RESULT.load(Ordering::SeqCst) as i64
        }

        /// Compile `code` with all three allocation helpers pointed at
        /// [`test_alloc`] and `site` resolved at every `new`/`anewarray`.
        fn compile_with_alloc(
            descriptor: &str,
            locals: usize,
            code: &[u8],
            site: Arm64NewSite,
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then one real function address in
            // three slots.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.new_object = test_alloc as usize;
            h.newarray = test_alloc as usize;
            h.anewarray_object = test_alloc as usize;
            b.set_helpers(h);
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            let mut map = HashMap::new();
            for (pc, op, _operand) in allocation_sites(code) {
                if op != 0xbc {
                    map.insert(pc, site);
                }
            }
            b.set_new_site_info(map);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, 4, code);
            assert!(result.success, "{descriptor} must compile");
            let cm = publish_compiled_method(&result).expect("publishes");
            assert!(
                cm.needs_context(),
                "an allocating body takes the VM pointer"
            );
            cm
        }

        /// The three arguments arrive and the allocated reference comes back
        /// as the method's result.
        #[test]
        fn r9w18_new_reaches_its_helper_and_returns_the_object() {
            let _serial = exec_guard();
            // new #3; areturn
            let cm = compile_with_alloc(
                "()LX;",
                2,
                &[0xbb, 0x00, 0x03, 0xb0],
                Arm64NewSite {
                    class_id: 33,
                    num_fields: 5,
                },
            );
            ALLOC_RESULT.store(0x2000_1000, Ordering::SeqCst);
            let before = ALLOC_CALLS.load(Ordering::SeqCst);
            let vm = 0x6666_0000i64;
            // SAFETY: a context-taking static `()LX;`, so the entry is
            // `fn(i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(vm, &[]) }.expect("callable");
            assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), before + 1);
            let args: Vec<u64> = ALLOC_ARGS
                .iter()
                .map(|a| a.load(Ordering::SeqCst))
                .collect();
            // Cast: the context pointer, as its bit pattern.
            assert_eq!(
                args,
                vec![vm as u64, 33, 5],
                "(vm_ptr, class_id, num_fields)"
            );
            assert_eq!(got, 0x2000_1000, "the object reference is the result");
        }

        // -------------------------------------------------------------------
        // Round 9 wave 21: the frame base, READ BACK the way the GC reads it.
        // -------------------------------------------------------------------

        /// What the prologue published through `helpers.frame_record`.
        static FRAME_BASE: AtomicU64 = AtomicU64::new(0);
        /// `CompiledMethod::sp_id_slot_off`, staged by the test before the call.
        static SP_ID_OFF: AtomicU64 = AtomicU64::new(0);
        /// The safepoint id read out of the running frame, or `u64::MAX`.
        static SEEN_SP_ID: AtomicU64 = AtomicU64::new(u64::MAX);

        extern "C" fn test_frame_record(fp: i64) {
            // Cast: a stack address, kept as its bit pattern.
            FRAME_BASE.store(fp as u64, Ordering::SeqCst);
        }

        /// An allocation helper that does what a COLLECTOR would do if it
        /// stopped this thread here: take the frame base the prologue
        /// published, read the safepoint id out of the frame at
        /// `[frame_base - sp_id_slot_off]`, and record it.
        extern "C" fn test_alloc_reading_sp_id(_vm: i64, _a: i64, _b: i64) -> i64 {
            let base = FRAME_BASE.load(Ordering::SeqCst) as usize;
            let off = SP_ID_OFF.load(Ordering::SeqCst) as usize;
            if base != 0 && off != 0 && base > off {
                // SAFETY: `base` is the FP of the compiled frame that called
                // this helper -- so that frame is live, directly below us on
                // this thread's own stack -- and `off` is the offset the
                // compiler reserved inside it, which is exactly the read
                // `scan_active_oop_map_at_rbp` performs.
                let id = unsafe { ((base - off) as *const usize).read() };
                SEEN_SP_ID.store(id as u64, Ordering::SeqCst);
            }
            // A non-zero "object": `0` means an error is pending and the body
            // would leave through the epilogue instead of pushing it.
            0x2000_5000
        }

        /// THE END-TO-END CHECK THE MAPS HAD NEVER HAD.
        ///
        /// Everything precise this backend emits is addressed from the frame
        /// base, and until wave 21 an ALLOCATING method never published one:
        /// `emit_frame_record` was gated on polls, which wave 18 had already
        /// established is the wrong question for an allocation. The maps were
        /// recorded, the id was stamped, and no collector could reach either.
        ///
        /// So this test stands where the collector stands. It runs the
        /// compiled body for real, and from INSIDE the allocation helper --
        /// the one moment a GC could see this frame -- it performs the
        /// runtime's own two-step: the frame base from the helper the prologue
        /// called, then the id at `[frame_base - sp_id_slot_off]`. The id must
        /// be the bci of the `new`, because that is the key
        /// `find_oop_map_for_safepoint_id` will look the frame's live
        /// references up by.
        #[test]
        fn r9w21_the_allocation_helper_can_read_this_frames_safepoint_id() {
            let _serial = exec_guard();
            // nop; new #3; areturn -- the allocation at bci 1, so a correct
            // read is distinguishable from a zeroed slot.
            let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then two real function addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.new_object = test_alloc_reading_sp_id as usize;
            h.frame_record = test_frame_record as usize;
            b.set_helpers(h);
            b.set_method_descriptor("()LX;", true);
            b.set_exception_table_empty(true);
            let mut map = HashMap::new();
            map.insert(
                1usize,
                Arm64NewSite {
                    class_id: 33,
                    num_fields: 5,
                },
            );
            b.set_new_site_info(map);
            let result = b.compile_method(2, 0, 4, &code);
            assert!(result.success, "the body must compile");
            assert!(
                result.frame_base_published,
                "an allocating method publishes its frame base"
            );
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_ne!(cm.sp_id_slot_off, 0, "an id word is reserved");
            assert!(
                cm.fully_oop_covered,
                "one mapped safepoint, an id word and a published base"
            );

            FRAME_BASE.store(0, Ordering::SeqCst);
            SEEN_SP_ID.store(u64::MAX, Ordering::SeqCst);
            // Cast: a reserved POSITIVE offset, published as its magnitude.
            SP_ID_OFF.store(cm.sp_id_slot_off as u64, Ordering::SeqCst);

            // SAFETY: a context-taking static `()LX;`, so the entry is
            // `fn(i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x7777_0000, &[]) }.expect("callable");
            assert_eq!(got, 0x2000_5000, "the object reference is the result");
            assert_ne!(
                FRAME_BASE.load(Ordering::SeqCst),
                0,
                "the prologue must have called frame_record"
            );
            assert_eq!(
                SEEN_SP_ID.load(Ordering::SeqCst),
                1,
                "the frame carries the bci of the allocation, so the map for \
                 this site is the one a collector would select"
            );
            // ...and the map it would then read names the same site.
            assert_eq!(
                cm.oop_maps.len(),
                1,
                "one allocation, one map"
            );
            assert!(
                cm.find_oop_map_for_safepoint_id(1).is_some(),
                "the id the frame holds resolves to a map"
            );
        }

        // -------------------------------------------------------------------
        // Round 9 wave 22: a CALL, executed.
        // -------------------------------------------------------------------

        /// What the fake `jit_invoke_dispatch` was handed, and what it returns.
        static DISP_CALLS: AtomicU64 = AtomicU64::new(0);
        static DISP_VM: AtomicU64 = AtomicU64::new(0);
        static DISP_INFO: AtomicU64 = AtomicU64::new(0);
        static DISP_N: AtomicU64 = AtomicU64::new(0);
        static DISP_ARGS: [AtomicU64; 4] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        static DISP_RESULT: AtomicU64 = AtomicU64::new(0);
        // The out-of-band "did the callee throw" answer -- the only way to
        // tell a `J` return of `Long.MIN_VALUE` from a bail -- is
        // `THREW_ANSWER` / `THREW_CALLS` / `test_dispatch_threw`, shared with
        // the wave-16 `getfield` fixture above: it is the same helper asked
        // the same question for the same reason.

        extern "C" fn test_dispatch(vm: i64, info: i64, args: i64, n: i64) -> i64 {
            DISP_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: pointers and a count, kept as their bit patterns.
            DISP_VM.store(vm as u64, Ordering::SeqCst);
            DISP_INFO.store(info as u64, Ordering::SeqCst);
            DISP_N.store(n as u64, Ordering::SeqCst);
            for i in 0..(n.max(0) as usize).min(DISP_ARGS.len()) {
                // SAFETY: the compiled caller passed the address of `n`
                // consecutive words inside its own LIVE frame, which is
                // directly below this helper on this thread's stack -- the
                // same read `jit_invoke_dispatch` performs.
                let v = unsafe { (args as *const i64).add(i).read() };
                // Cast: an argument word, kept as its bit pattern.
                DISP_ARGS[i].store(v as u64, Ordering::SeqCst);
            }
            // Cast: the staged result.
            DISP_RESULT.load(Ordering::SeqCst) as i64
        }

        /// Compile `code` with the dispatcher wired and every `invoke*`
        /// resolved to one site.
        fn compile_with_dispatch(
            descriptor: &str,
            locals: usize,
            code: &[u8],
            num_jit_args: usize,
            return_type: u8,
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then two real function addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.invoke_dispatch = test_dispatch as usize;
            h.dispatch_threw = test_dispatch_threw as usize;
            h.frame_record = test_frame_record as usize;
            b.set_helpers(h);
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            let mut map = HashMap::new();
            for (pc, op, _cp) in invoke_sites(code) {
                if op != 0xba {
                    map.insert(
                        pc,
                        Arm64InvokeSite {
                            info_ptr: 0x7FFF_0000_F000,
                            num_jit_args,
                            return_type,
                        },
                    );
                }
            }
            b.set_invoke_site_info(map);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, 6, code);
            assert!(result.success, "{descriptor} must compile");
            publish_compiled_method(&result).expect("publishes")
        }

        fn reset_dispatch() {
            DISP_CALLS.store(0, Ordering::SeqCst);
            DISP_N.store(u64::MAX, Ordering::SeqCst);
            THREW_CALLS.store(0, Ordering::SeqCst);
            THREW_ANSWER.store(0, Ordering::SeqCst);
            for a in DISP_ARGS.iter() {
                a.store(0, Ordering::SeqCst);
            }
        }

        /// THE WHOLE WAVE, IN ONE RUN: the four helper arguments arrive, the
        /// argument BUFFER holds this call's operands in the helper's own
        /// ascending order, and the result becomes the method's.
        #[test]
        fn r9w22_invokestatic_reaches_the_dispatcher_with_its_arguments() {
            let _serial = exec_guard();
            // iload_0; iload_1; invokestatic #1; ireturn
            let cm = compile_with_dispatch("(II)I", 2, &[0x1a, 0x1b, 0xb8, 0x00, 0x01, 0xac], 2, b'I');
            reset_dispatch();
            DISP_RESULT.store(4242, Ordering::SeqCst);
            let vm = 0x5555_0000i64;
            // SAFETY: a context-taking static `(II)I`, entry
            // `fn(i64, i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(vm, &[7, 9]) }.expect("callable");
            assert_eq!(DISP_CALLS.load(Ordering::SeqCst), 1);
            // Cast: the context pointer and the site's info pointer.
            assert_eq!(DISP_VM.load(Ordering::SeqCst), vm as u64, "the VM pointer first");
            assert_eq!(
                DISP_INFO.load(Ordering::SeqCst),
                0x7FFF_0000_F000,
                "then the site's JitInvokeInfo"
            );
            assert_eq!(DISP_N.load(Ordering::SeqCst), 2, "then the count");
            assert_eq!(
                (
                    DISP_ARGS[0].load(Ordering::SeqCst),
                    DISP_ARGS[1].load(Ordering::SeqCst)
                ),
                (7, 9),
                "args[0] is the DEEPEST operand -- the helper reads the buffer \
                 in ascending address order and this backend's operand area \
                 ascends with depth"
            );
            assert_eq!(got, 4242, "the callee's result is the method's");
        }

        /// EVERY ARGUMENT WIDTH, through the buffer.
        ///
        /// The helper decodes each word by the descriptor -- `Value::Long(raw)`,
        /// `Value::Float(f32::from_bits(raw as u32))` -- so a `long` owes the
        /// whole word and a `float` owes its low 32 bits. This backend stores
        /// an operand at its OWN width (`STR Dn` for a double, `STR Sn` for a
        /// float), which is the encoding that makes both true, and nothing but
        /// running it says so.
        #[test]
        fn r9w22_a_long_and_a_float_argument_arrive_correctly_encoded() {
            let _serial = exec_guard();
            // lload_0; fload_2; invokestatic #1 (JF)J; lreturn
            let cm = compile_with_dispatch(
                "(JF)J",
                4,
                &[0x1e, 0x24, 0xb8, 0x00, 0x01, 0xad],
                2,
                b'J',
            );
            reset_dispatch();
            DISP_RESULT.store(5, Ordering::SeqCst);
            THREW_ANSWER.store(0, Ordering::SeqCst);
            let long_bits: i64 = 0x1234_5678_9ABC_DEF0u64 as i64;
            let float_bits = (-2.5f32).to_bits();
            // SAFETY: a context-taking static `(JF)J`; a `float` argument
            // arrives as its zero-extended bit pattern, which is the entry
            // convention this backend's prologue homes.
            let got = unsafe {
                cm.try_call_with_context(0x5555_0000, &[long_bits, i64::from(float_bits)])
            }
            .expect("callable");
            // Cast: the long, kept as its bit pattern.
            assert_eq!(
                DISP_ARGS[0].load(Ordering::SeqCst),
                long_bits as u64,
                "a `long` occupies the whole argument word"
            );
            assert_eq!(
                DISP_ARGS[1].load(Ordering::SeqCst) as u32,
                float_bits,
                "and a `float` its low 32 bits, which is all the helper reads"
            );
            assert_eq!(got, 5);
        }

        /// The receiver is `args[0]` for every kind but `invokestatic`.
        #[test]
        fn r9w22_invokevirtual_passes_the_receiver_first() {
            let _serial = exec_guard();
            // aload_0; iload_1; invokevirtual #1; ireturn
            let cm = compile_with_dispatch(
                "(Ljava/lang/Object;I)I",
                2,
                &[0x2a, 0x1b, 0xb6, 0x00, 0x01, 0xac],
                2,
                b'I',
            );
            reset_dispatch();
            DISP_RESULT.store(1, Ordering::SeqCst);
            // SAFETY: a context-taking static, entry `fn(i64, i64, i64) -> i64`.
            let _ = unsafe { cm.try_call_with_context(0x5555_0000, &[0x3000_0008, 5]) }
                .expect("callable");
            assert_eq!(
                (
                    DISP_ARGS[0].load(Ordering::SeqCst),
                    DISP_ARGS[1].load(Ordering::SeqCst)
                ),
                (0x3000_0008, 5),
                "the receiver is args[0]"
            );
        }

        /// AN OPERAND UNDER THE ARGUMENTS SURVIVES THE CALL, and the result
        /// lands on top of it.
        ///
        /// This is the consuming call's contract in one run. The call spills
        /// the whole operand stack, is stopped for the duration, drops exactly
        /// the entries it consumed, and reloads the rest -- so an entry the
        /// call did not take must come back holding what it held.
        #[test]
        fn r9w22_an_operand_under_the_arguments_survives_the_call() {
            let _serial = exec_guard();
            // iload_0; iload_1; iload_2; invokestatic #1 (II)I; iadd; ireturn
            let cm = compile_with_dispatch(
                "(III)I",
                3,
                &[0x1a, 0x1b, 0x1c, 0xb8, 0x00, 0x01, 0x60, 0xac],
                2,
                b'I',
            );
            reset_dispatch();
            DISP_RESULT.store(1000, Ordering::SeqCst);
            // SAFETY: a context-taking static `(III)I`, entry
            // `fn(i64, i64, i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x5555_0000, &[11, 22, 33]) }
                .expect("callable");
            assert_eq!(
                (
                    DISP_ARGS[0].load(Ordering::SeqCst),
                    DISP_ARGS[1].load(Ordering::SeqCst)
                ),
                (22, 33),
                "the call takes the TOP two entries"
            );
            assert_eq!(got, 11 + 1000, "and the third survived to be added to it");
        }

        /// A callee that threw leaves this frame through the epilogue, with
        /// the deopt sentinel, for the interpreter's drain to raise.
        #[test]
        fn r9w22_a_throwing_callee_leaves_through_the_epilogue() {
            let _serial = exec_guard();
            // iload_0; iload_1; invokestatic #1; iconst_1; iadd; ireturn --
            // the trailing arithmetic is what a bail must SKIP.
            let cm = compile_with_dispatch(
                "(II)I",
                2,
                &[0x1a, 0x1b, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac],
                2,
                b'I',
            );
            reset_dispatch();
            // Cast: the deopt sentinel, staged as the helper's answer.
            DISP_RESULT.store(i64::MIN as u64, Ordering::SeqCst);
            // SAFETY: a context-taking static `(II)I`.
            let got = unsafe { cm.try_call_with_context(0x5555_0000, &[1, 2]) }.expect("callable");
            assert_eq!(got, i64::MIN, "the sentinel is the method's result");
            assert_eq!(
                THREW_CALLS.load(Ordering::SeqCst),
                0,
                "an `int` return is unambiguous: no probe is owed"
            );
        }

        /// ...and a `long`-returning callee may legitimately return
        /// `Long.MIN_VALUE`, which is bit-identical to the sentinel.
        ///
        /// The discriminator is `lconst_1; ladd` after the call: on the value
        /// path the method answers `Long.MIN_VALUE + 1`, on the bail path it
        /// answers the sentinel itself, having skipped the addition.
        #[test]
        fn r9w22_a_long_returning_callee_may_return_long_min_value() {
            let _serial = exec_guard();
            // invokestatic #1 ()J; lconst_1; ladd; lreturn
            let cm = compile_with_dispatch("()J", 2, &[0xb8, 0x00, 0x01, 0x0a, 0x61, 0xad], 0, b'J');

            reset_dispatch();
            // Cast: the ambiguous value.
            DISP_RESULT.store(i64::MIN as u64, Ordering::SeqCst);
            THREW_ANSWER.store(0, Ordering::SeqCst);
            // SAFETY: a context-taking static `()J`, entry `fn(i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x5555_0000, &[]) }.expect("callable");
            assert_eq!(DISP_N.load(Ordering::SeqCst), 0, "a zero-argument site");
            assert_eq!(
                THREW_CALLS.load(Ordering::SeqCst),
                1,
                "the `J` return must ASK rather than read the bits"
            );
            assert_eq!(
                got,
                i64::MIN.wrapping_add(1),
                "no signal pending, so Long.MIN_VALUE is the callee's real value"
            );

            // ...and the same bits with a signal pending IS a bail.
            reset_dispatch();
            DISP_RESULT.store(i64::MIN as u64, Ordering::SeqCst);
            THREW_ANSWER.store(1, Ordering::SeqCst);
            // SAFETY: as above.
            let got = unsafe { cm.try_call_with_context(0x5555_0000, &[]) }.expect("callable");
            assert_eq!(THREW_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(got, i64::MIN, "a pending signal leaves through the epilogue");
        }

        // -------------------------------------------------------------------
        // Round 9 wave 23: the type checks, executed.
        // -------------------------------------------------------------------

        static TC_CALLS: AtomicU64 = AtomicU64::new(0);
        static TC_ARGS: [AtomicU64; 4] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        static TC_RESULT: AtomicU64 = AtomicU64::new(0);

        extern "C" fn test_typecheck(vm: i64, obj: i64, name: i64, len: i64) -> i64 {
            TC_CALLS.fetch_add(1, Ordering::SeqCst);
            // Cast: four i64 arguments, kept as their bit patterns.
            for (slot, v) in TC_ARGS.iter().zip([vm, obj, name, len]) {
                slot.store(v as u64, Ordering::SeqCst);
            }
            // Cast: the verdict the test staged.
            TC_RESULT.load(Ordering::SeqCst) as i64
        }

        fn compile_with_typecheck(descriptor: &str, code: &[u8]) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            // SAFETY: zeroed helper table, then real function addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.checkcast = test_typecheck as usize;
            h.instanceof_check = test_typecheck as usize;
            h.frame_record = test_frame_record as usize;
            b.set_helpers(h);
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            let mut map = HashMap::new();
            for (pc, _op, _cp) in typecheck_sites(code) {
                map.insert(
                    pc,
                    Arm64TypecheckSite {
                        name_ptr: 0x7FFF_0001_2000,
                        name_len: 16,
                    },
                );
            }
            b.set_typecheck_site_info(map);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(2, slots, 4, code);
            assert!(result.success, "{descriptor} must compile");
            publish_compiled_method(&result).expect("publishes")
        }

        /// `checkcast` hands the helper `(vm, obj, name, len)` and pushes the
        /// reference the HELPER returns -- not the one it was handed, which is
        /// the difference that matters under a collector that moved it.
        #[test]
        fn r9w23_checkcast_returns_the_helpers_reference() {
            let _serial = exec_guard();
            // aload_0; checkcast #1; areturn
            let cm = compile_with_typecheck(
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[0x2a, 0xc0, 0x00, 0x01, 0xb0],
            );
            TC_CALLS.store(0, Ordering::SeqCst);
            // A DIFFERENT address from the argument: a moved object.
            TC_RESULT.store(0x4000_2000, Ordering::SeqCst);
            // SAFETY: a context-taking static, entry `fn(i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x8888_0000, &[0x4000_1000]) }
                .expect("callable");
            assert_eq!(TC_CALLS.load(Ordering::SeqCst), 1);
            let args: Vec<u64> = TC_ARGS.iter().map(|a| a.load(Ordering::SeqCst)).collect();
            assert_eq!(
                args,
                vec![0x8888_0000, 0x4000_1000, 0x7FFF_0001_2000, 16],
                "(vm_ptr, obj, class_name_ptr, class_name_len)"
            );
            assert_eq!(got, 0x4000_2000, "the helper's reference is the result");
        }

        /// `instanceof` pushes the helper's `int`.
        #[test]
        fn r9w23_instanceof_pushes_the_helpers_int() {
            let _serial = exec_guard();
            // aload_0; instanceof #1; ireturn
            let cm = compile_with_typecheck("(Ljava/lang/Object;)I", &[0x2a, 0xc1, 0x00, 0x01, 0xac]);
            TC_CALLS.store(0, Ordering::SeqCst);
            TC_RESULT.store(1, Ordering::SeqCst);
            // SAFETY: a context-taking static, entry `fn(i64, i64) -> i64`.
            let got =
                unsafe { cm.try_call_with_context(0x8888_0000, &[0x4000_1000]) }.expect("callable");
            assert_eq!(got, 1);

            TC_RESULT.store(0, Ordering::SeqCst);
            // SAFETY: as above.
            let got =
                unsafe { cm.try_call_with_context(0x8888_0000, &[0x4000_1000]) }.expect("callable");
            assert_eq!(got, 0);
        }

        /// A definitively failed cast leaves through the epilogue with the
        /// sentinel, for the drain to raise the stashed `ClassCastException`.
        #[test]
        fn r9w23_a_failed_cast_leaves_through_the_epilogue() {
            let _serial = exec_guard();
            // aload_0; checkcast #1; areturn
            let cm = compile_with_typecheck(
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[0x2a, 0xc0, 0x00, 0x01, 0xb0],
            );
            TC_CALLS.store(0, Ordering::SeqCst);
            // Cast: the deopt sentinel, staged as the helper's answer.
            TC_RESULT.store(i64::MIN as u64, Ordering::SeqCst);
            // SAFETY: a context-taking static, entry `fn(i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x8888_0000, &[0x4000_1000]) }
                .expect("callable");
            assert_eq!(got, i64::MIN, "the sentinel is the method's result");
        }

        /// `newarray` passes its `atype` operand byte and the length operand.
        #[test]
        fn r9w18_newarray_passes_its_atype_and_length() {
            let _serial = exec_guard();
            // iload_0; newarray T_INT(10); areturn
            let cm = compile_with_alloc(
                "(I)LX;",
                2,
                &[0x1a, 0xbc, 0x0a, 0xb0],
                Arm64NewSite {
                    class_id: 0,
                    num_fields: 0,
                },
            );
            ALLOC_RESULT.store(0x2000_2000, Ordering::SeqCst);
            // SAFETY: a context-taking static `(I)LX;`, entry
            // `fn(i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x6666_0000, &[7]) }.expect("callable");
            let args: Vec<u64> = ALLOC_ARGS
                .iter()
                .map(|a| a.load(Ordering::SeqCst))
                .collect();
            assert_eq!(args[1], 10, "T_INT, straight out of the bytecode");
            assert_eq!(args[2], 7, "the length operand");
            assert_eq!(got, 0x2000_2000);
        }

        /// A NULL result leaves through the epilogue with the deopt sentinel
        /// instead of pushing the null. Without this check the null is pushed
        /// and the next field access or `arraylength` dereferences it.
        #[test]
        fn r9w18_a_null_allocation_leaves_instead_of_pushing_it() {
            let _serial = exec_guard();
            let cm = compile_with_alloc(
                "()LX;",
                2,
                &[0xbb, 0x00, 0x03, 0xb0],
                Arm64NewSite {
                    class_id: 33,
                    num_fields: 5,
                },
            );
            ALLOC_RESULT.store(0, Ordering::SeqCst);
            // SAFETY: as above.
            let got = unsafe { cm.try_call_with_context(0x6666_0000, &[]) }.expect("callable");
            assert_eq!(
                got,
                i64::MIN,
                "an OOM must reach the caller as the deopt sentinel, not as null"
            );
            ALLOC_RESULT.store(0x2000_1000, Ordering::SeqCst);
        }

        /// A REFERENCE LOCAL live across the allocation comes back. The call
        /// stores it to its safepoint home and reloads it afterwards; the
        /// reload is what would carry a moved object's new address, and this
        /// is the round trip that proves the pair is emitted and executes.
        #[test]
        fn r9w18_a_reference_local_live_across_an_allocation_comes_back() {
            let _serial = exec_guard();
            // aload_0; astore_1; new #3; pop; aload_1; areturn
            let cm = compile_with_alloc(
                "(LX;)LX;",
                3,
                &[0x2a, 0x4c, 0xbb, 0x00, 0x03, 0x57, 0x2b, 0xb0],
                Arm64NewSite {
                    class_id: 33,
                    num_fields: 5,
                },
            );
            ALLOC_RESULT.store(0x2000_3000, Ordering::SeqCst);
            let receiver = 0x4242_4240i64;
            // SAFETY: a context-taking static `(LX;)LX;`, entry
            // `fn(i64, i64) -> i64`.
            let got =
                unsafe { cm.try_call_with_context(0x6666_0000, &[receiver]) }.expect("callable");
            assert_eq!(
                got, receiver,
                "the reference local must survive the allocation call"
            );
        }

        /// ...and a reference OPERAND does too, from under the new object.
        #[test]
        fn r9w18_a_reference_operand_under_the_new_object_comes_back() {
            let _serial = exec_guard();
            // aload_0; new #3; pop; areturn
            let cm = compile_with_alloc(
                "(LX;)LX;",
                2,
                &[0x2a, 0xbb, 0x00, 0x03, 0x57, 0xb0],
                Arm64NewSite {
                    class_id: 33,
                    num_fields: 5,
                },
            );
            ALLOC_RESULT.store(0x2000_4000, Ordering::SeqCst);
            let receiver = 0x4343_4340i64;
            // SAFETY: as above.
            let got =
                unsafe { cm.try_call_with_context(0x6666_0000, &[receiver]) }.expect("callable");
            assert_eq!(got, receiver);
        }
        // -- Round 9 wave 19: the reference stores, EXECUTED ----------------

        /// Per-helper call counts: 0 `putfield_object`, 1 `putstatic_object`,
        /// 2 `aaload`, 3 `aastore_type_check`, 4 `aastore`.
        static REF_CALLS: [AtomicU64; 5] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        /// The arguments of the last call. Words 0..4 are whatever helper ran
        /// last EXCEPT the type check, whose three go in 4..7 -- `aastore`
        /// runs both at one bytecode, and a test that could not see both sets
        /// could not show that the array and the value survived the first.
        static REF_ARGS: [AtomicU64; 8] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        /// What `aaload` returns, and what the type check answers.
        static REF_RESULT: AtomicU64 = AtomicU64::new(0);

        fn ref_record(which: usize, base: usize, args: &[i64]) {
            REF_CALLS[which].fetch_add(1, Ordering::SeqCst);
            for (i, v) in args.iter().enumerate() {
                // Cast: an i64 argument, kept as its bit pattern.
                REF_ARGS[base + i].store(*v as u64, Ordering::SeqCst);
            }
        }

        extern "C" fn test_putfield_object(vm: i64, obj: i64, index: i64, val: i64) {
            ref_record(0, 0, &[vm, obj, index, val]);
        }
        extern "C" fn test_putstatic_object(vm: i64, class_id: i64, index: i64, val: i64) -> i64 {
            ref_record(1, 0, &[vm, class_id, index, val]);
            0
        }
        extern "C" fn test_aaload(vm: i64, arr: i64, index: i64) -> i64 {
            ref_record(2, 0, &[vm, arr, index]);
            // Cast: the element the test staged.
            REF_RESULT.load(Ordering::SeqCst) as i64
        }
        extern "C" fn test_type_check(vm: i64, arr: i64, val: i64) -> i64 {
            ref_record(3, 4, &[vm, arr, val]);
            // Cast: 0 (legal) or the sentinel, staged by the test.
            REF_RESULT.load(Ordering::SeqCst) as i64
        }
        extern "C" fn test_aastore(vm: i64, arr: i64, index: i64, val: i64) {
            ref_record(4, 0, &[vm, arr, index, val]);
        }

        fn exec_ref_helpers() -> crate::JitRuntimeHelpers {
            // SAFETY: zeroed helper table, then real function addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.jit_npe_with_action = test_npe as usize;
            h.throw_aioobe = test_aioobe as usize;
            h.putfield_object = test_putfield_object as usize;
            h.putstatic_object = test_putstatic_object as usize;
            h.aaload = test_aaload as usize;
            h.aastore = test_aastore as usize;
            h.aastore_type_check = test_type_check as usize;
            h
        }

        fn compile_ref_body(
            descriptor: &str,
            locals: usize,
            code: &[u8],
            instance: Option<Arm64InstanceField>,
            statics: Option<Arm64StaticField>,
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            b.set_helpers(exec_ref_helpers());
            b.set_method_descriptor(descriptor, true);
            b.set_exception_table_empty(true);
            if let Some(f) = instance {
                let mut map = HashMap::new();
                for (pc, _op, _cp) in instance_field_sites(code) {
                    map.insert(pc, f);
                }
                b.set_instance_field_info(map);
            }
            if let Some(f) = statics {
                let mut map = HashMap::new();
                for (pc, _op, _cp) in static_field_sites(code) {
                    map.insert(pc, f);
                }
                b.set_static_field_info(map);
            }
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, 4, code);
            assert!(result.success, "{descriptor} must compile");
            publish_compiled_method(&result).expect("publishes")
        }

        /// A reference `putfield` and `putstatic` reach their barrier helpers
        /// with the context first and the value as a pointer.
        #[test]
        fn r9w19_the_reference_stores_reach_their_barrier_helpers() {
            let _serial = exec_guard();
            let vm = 0x8888_0000i64;
            let obj = 0x5150_0000i64;
            let val = 0x5150_1000i64;

            // aload_0; aload_1; putfield #3; return
            let cm = compile_ref_body(
                "(LX;LY;)V",
                2,
                &[0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1],
                Some(Arm64InstanceField {
                    field_index: 4,
                    type_tag: b'L',
                    is_volatile: false,
                }),
                None,
            );
            let before = REF_CALLS[0].load(Ordering::SeqCst);
            // SAFETY: a context-taking static `(LX;LY;)V`, entry
            // `fn(i64, i64, i64) -> i64`.
            unsafe { cm.try_call_with_context(vm, &[obj, val]) }.expect("callable");
            assert_eq!(REF_CALLS[0].load(Ordering::SeqCst), before + 1);
            let args: Vec<u64> = REF_ARGS[..4]
                .iter()
                .map(|a| a.load(Ordering::SeqCst))
                .collect();
            // Cast: three pointers and an index, as their bit patterns.
            assert_eq!(
                args,
                vec![vm as u64, obj as u64, 4, val as u64],
                "(vm_ptr, obj_ptr, field_index, value)"
            );

            // aload_0; putstatic #3; return
            let cm = compile_ref_body(
                "(LX;)V",
                2,
                &[0x2a, 0xb3, 0x00, 0x03, 0xb1],
                None,
                Some(Arm64StaticField {
                    class_id: 12,
                    field_index: 6,
                    base_cell: 0x1000,
                    type_tag: b'L',
                    is_volatile: false,
                }),
            );
            let before = REF_CALLS[1].load(Ordering::SeqCst);
            // SAFETY: a context-taking static `(LX;)V`, entry
            // `fn(i64, i64) -> i64`.
            unsafe { cm.try_call_with_context(vm, &[val]) }.expect("callable");
            assert_eq!(REF_CALLS[1].load(Ordering::SeqCst), before + 1);
            let args: Vec<u64> = REF_ARGS[..4]
                .iter()
                .map(|a| a.load(Ordering::SeqCst))
                .collect();
            assert_eq!(args, vec![vm as u64, 12, 6, val as u64]);
        }

        /// `aaload` guards, calls, and hands back the element as a reference.
        #[test]
        fn r9w19_aaload_returns_the_element_the_helper_gives_it() {
            let _serial = exec_guard();
            // aload_0; iload_1; aaload; areturn
            let cm = compile_ref_body("([LX;I)LX;", 3, &[0x2a, 0x1b, 0x32, 0xb0], None, None);
            let mut arr = FakeArray::new(4, 4 * 8, 0);
            REF_RESULT.store(0x5150_2000, Ordering::SeqCst);
            // SAFETY: a context-taking static `([LX;I)LX;`, entry
            // `fn(i64, i64, i64) -> i64`.
            let got = unsafe { cm.try_call_with_context(0x8888_0000, &[arr.addr(), 2]) }
                .expect("callable");
            assert_eq!(got, 0x5150_2000, "the element comes back as the result");
            assert_eq!(
                REF_ARGS[2].load(Ordering::SeqCst),
                2,
                "the index reaches the helper"
            );

            // ...and an out-of-range index never reaches it: this backend's
            // own bounds check takes the AIOOBE stub first.
            let before = REF_CALLS[2].load(Ordering::SeqCst);
            let aioobe_before = AIOOBE_CALLS.load(Ordering::SeqCst);
            // SAFETY: as above.
            let bail = unsafe { cm.try_call_with_context(0x8888_0000, &[arr.addr(), 9]) }
                .expect("callable");
            assert_eq!(REF_CALLS[2].load(Ordering::SeqCst), before);
            assert_eq!(AIOOBE_CALLS.load(Ordering::SeqCst), aioobe_before + 1);
            assert_eq!(bail, i64::MIN);
        }

        /// THE TWO-CALL SEQUENCE. The type check runs first and sees the array
        /// and the value; the store then runs and sees the SAME array and
        /// value, having survived the first call.
        #[test]
        fn r9w19_aastore_type_checks_then_stores_the_same_operands() {
            let _serial = exec_guard();
            // aload_0; iload_1; aload_2; aastore; return
            let cm = compile_ref_body(
                "([LX;ILX;)V",
                4,
                &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
                None,
                None,
            );
            let mut arr = FakeArray::new(4, 4 * 8, 0);
            let val = 0x5150_3000i64;
            REF_RESULT.store(0, Ordering::SeqCst); // the store is legal

            let checks = REF_CALLS[3].load(Ordering::SeqCst);
            let stores = REF_CALLS[4].load(Ordering::SeqCst);
            // SAFETY: a context-taking static `([LX;ILX;)V`, entry
            // `fn(i64, i64, i64, i64) -> i64`.
            unsafe { cm.try_call_with_context(0x8888_0000, &[arr.addr(), 1, val]) }
                .expect("callable");
            assert_eq!(REF_CALLS[3].load(Ordering::SeqCst), checks + 1);
            assert_eq!(REF_CALLS[4].load(Ordering::SeqCst), stores + 1);
            // Cast: the array address and the value, as their bit patterns.
            assert_eq!(
                (
                    REF_ARGS[5].load(Ordering::SeqCst),
                    REF_ARGS[6].load(Ordering::SeqCst)
                ),
                (arr.addr() as u64, val as u64),
                "the type check sees the array and the value"
            );
            assert_eq!(
                (
                    REF_ARGS[1].load(Ordering::SeqCst),
                    REF_ARGS[2].load(Ordering::SeqCst),
                    REF_ARGS[3].load(Ordering::SeqCst)
                ),
                (arr.addr() as u64, 1, val as u64),
                "and the store sees the SAME ones, across the first call"
            );
        }

        /// A REFUSED element type leaves the frame, and the store does not
        /// happen. Without the type check, `jit_aastore`'s own
        /// ArrayStoreException would travel by the pending-signal channel and
        /// surface only at method return -- after this frame had run on.
        #[test]
        fn r9w19_a_refused_element_type_never_reaches_the_store() {
            let _serial = exec_guard();
            let cm = compile_ref_body(
                "([LX;ILX;)V",
                4,
                &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
                None,
                None,
            );
            let mut arr = FakeArray::new(4, 4 * 8, 0);
            REF_RESULT.store(i64::MIN as u64, Ordering::SeqCst);
            let stores = REF_CALLS[4].load(Ordering::SeqCst);
            // SAFETY: as above.
            let got =
                unsafe { cm.try_call_with_context(0x8888_0000, &[arr.addr(), 0, 0x5150_4000]) }
                    .expect("callable");
            assert_eq!(
                REF_CALLS[4].load(Ordering::SeqCst),
                stores,
                "the store must NOT run once the type check refused it"
            );
            assert_eq!(got, i64::MIN, "and the frame leaves with the sentinel");
            REF_RESULT.store(0, Ordering::SeqCst);
        }
    }
}
