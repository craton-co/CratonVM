#[allow(unused_imports)]
use super::*;

    // ── IR inline caches (jit-inlining-and-ir-calls) ────────────────────

    /// Build → lower a `static int f(Obj o, int n) { return o.g(n); }` shaped
    /// body whose single `invokevirtual` is planned as an inline-cache site.
    ///
    /// `ic` is the `pc → (mic, pic)` map handed to `lower_inner`; passing an
    /// empty map exercises the unchanged generic-dispatch path, which is what
    /// makes the "absent plan is byte-identical" assertion below meaningful.
    ///
    /// The returned code is never EXECUTED — the baked helper/slot addresses
    /// are test sentinels, not real functions. These tests assert on the
    /// emitted instruction stream, which is the part this change owns; the
    /// end-to-end execution coverage for IR call lowering lives in
    /// `jit/tests/ir_vs_singlepass.rs` (see the doc for the cases to add
    /// there — that file is outside this change's ownership).

    /// Every inline-cache hit in this tier restores the innermost-frame mirror
    /// before it does anything else.
    ///
    /// A compiled callee's prologue publishes ITS `(rbp, compile id)` there, and
    /// nothing on the return path of a raw JIT->JIT call restores the caller's.
    /// Until 2026-08-24 neither of this tier's two inline-cache arms — the
    /// monomorphic MIC hit and each rung of the polymorphic PIC cascade — did,
    /// so after any hit the mirror named a frame that had already returned.
    /// `moving_young_frame_coverage_complete` then read
    /// `[stale_rbp - stale_method.sp_id_slot_off]`, matched no map, and refused
    /// the whole collection (`ACTIVE_FRAME_MAP`). Measured on H2
    /// `TestMVStoreTool`: one rbp with ONE saved return address claimed across
    /// collections by four different methods, one of them `RootReference.isLocked`
    /// with `maps=0` — a method that emits no safepoint and therefore cannot be
    /// the frame at a collection at all.
    ///
    /// The single-pass backend republishes at both of its equivalent arms and
    /// the shared hashed/vtable stub is handed `frame_record` for the same
    /// reason; these two were the gap.
    ///
    /// Asserted as "the byte right after the CALL", because that placement is
    /// the property: anything emitted between the return and the republish runs
    /// under a mirror naming a dead frame. The zero-`frame_record` arm is the
    /// control — it proves the assertion can FAIL, so a future edit that drops
    /// the republish cannot leave this test passing vacuously.
pub(in crate::ir_lower::tests) fn lower_virtual_call_with_ic(ic: &HashMap<usize, (usize, usize)>) -> Vec<u8> {
        lower_virtual_call_with_ic_fr(ic, 0)
    }


    /// As [`lower_virtual_call_with_ic`], with the frame-record helper
    /// pointer set. Zero is what `no_helpers()` gives, and zero switches the
    /// whole frame record off, so a test about the record has to pass a live
    /// one or it is asking a question the emitter never reaches.
    pub(in crate::ir_lower) fn lower_virtual_call_with_ic_fr(
        ic: &HashMap<usize, (usize, usize)>,
        frame_record: usize,
    ) -> Vec<u8> {
        lower_virtual_call_with_ic_owner(ic, frame_record, "pkg/Obj")
    }

    /// As [`lower_virtual_call_with_ic_fr`], with the site's constant-pool
    /// owner named: since round 13 wave 11 (lane mega9, M13-6) it decides
    /// whether the cascade screens the receiver's kind.
    pub(in crate::ir_lower::tests) fn lower_virtual_call_with_ic_owner(
        ic: &HashMap<usize, (usize, usize)>,
        frame_record: usize,
        owner: &'static str,
    ) -> Vec<u8> {
        // aload_0; iload_1; invokevirtual #2; ireturn
        let code = [0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac, 0x00, 0x00];
        // A live `JitInvokeInfo` for the site. Leaked deliberately: `Op::Call`
        // bakes its address, and the lowerer dereferences it to read
        // `invoke_kind`.
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: owner,
            method_name: "g",
            descriptor: "(I)I",
            num_jit_args: 2, // receiver + one int
            return_type: b'I',
            invoke_kind: 0, // virtual
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let info_ptr = info as *const JitInvokeInfo as usize;

        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(2usize, (info_ptr, 2usize, b'I'));
        builder.set_invoke_info(invoke_info);
        let graph = builder.build(&code, 6).expect("IR build of virtual call");
        let schedule = ir_schedule::schedule(&graph);

        let mut helpers = no_helpers();
        // Distinct non-zero sentinels so each path is identifiable in the
        // emitted imm64s.
        helpers.invoke_dispatch = 0x1111_2222_3333_4440;
        helpers.invoke_virtual_mic = 0x1111_2222_3333_4441;
        helpers.dispatch_threw = 0x1111_2222_3333_4442;
        helpers.frame_record = frame_record;

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            2,
            2,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("virtual-call method must lower")
        .code_bytes()
        .to_vec()
    }

    /// Lower one `invokestatic` with `n` int parameters as a DIRECT call to
    /// `entry`, and return the emitted bytes.
    ///
    /// `needs_ctx` picks whether the callee takes the hidden VM context
    /// pointer in `abi[0]`, which is what decides how many Java arguments are
    /// left for the register file.
    pub(in crate::ir_lower::tests) fn lower_direct_call_with_n_args(n: usize, entry: usize, needs_ctx: bool) -> Vec<u8> {
        lower_direct_call_cm(n, entry, needs_ctx).code_bytes().to_vec()
    }


    pub(in crate::ir_lower::tests) fn lower_direct_call_cm(n: usize, entry: usize, needs_ctx: bool) -> CompiledMethod {
        assert!((1..=8).contains(&n));
        // The arguments are CONSTANTS, not this method's parameters. A caller
        // with `n` parameters would be refused outright by `lower()` when `n`
        // exceeds `incoming_abi_reg_capacity()` — which is exactly the case
        // under test — and the refusal is about the PROLOGUE, a different
        // question from the one these tests ask.
        let mut code: Vec<u8> = Vec::new();
        for _ in 0..n {
            code.push(0x03); // iconst_0
        }
        let invoke_pc = code.len();
        code.extend_from_slice(&[0xb8, 0x00, 0x02]); // invokestatic #2
        code.push(0xac); // ireturn
        let code_len = code.len();
        // `IrBuilder::build` reads a little past the end for operand fetch.
        code.extend_from_slice(&[0x00, 0x00]);

        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/Wide",
            method_name: "f",
            descriptor: "(IIIIIII)I",
            num_jit_args: n,
            return_type: b'I',
            invoke_kind: 3, // static — the statically bound, directly bindable kind
            declaring_class_id: 0,
            owner_class_id: 0,
        }));

        let mut builder = IrBuilder::new(0, 1);
        builder.set_param_types(&[]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(invoke_pc, (info as *const JitInvokeInfo as usize, n, b'I'));
        builder.set_invoke_info(invoke_info);
        let graph = builder
            .build(&code, code_len)
            .expect("IR build of wide call");
        let schedule = ir_schedule::schedule(&graph);

        let mut helpers = no_helpers();
        helpers.invoke_dispatch = 0x1111_2222_3333_4440;

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let mut direct: HashMap<usize, (usize, bool)> = HashMap::new();
        direct.insert(invoke_pc, (entry, needs_ctx));
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            0,
            1,
            &helpers,
            &empty_hints,
            &[],
            None,
            &direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("wide direct call must lower")
    }

    /// A direct call whose arguments do not all fit `ENTRY_ABI_REGS` marshals
    /// the remainder on the stack instead of falling back to the dispatch
    /// helper.
    ///
    /// This is the blocker `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md`
    /// named: `emit_direct_cross_call` was register-only, so a site needing
    /// seven incoming slots kept the full `jit_invoke_dispatch` round trip no
    /// matter what the binding side had resolved. The map entry was recorded
    /// and then silently dropped by the lowerer — nothing warned.
    ///
    /// The assertion is structural rather than a byte-for-byte golden: the
    /// block size differs by platform (Win64 reserves 32 bytes of shadow
    /// space below the outgoing arguments, SysV none) and so does how many
    /// arguments are left over, and pinning both would only restate
    /// `stack_arg_block_size`.
    #[test]
    pub(in crate::ir_lower::tests) fn a_direct_call_past_the_register_file_marshals_its_tail_on_the_stack() {
        crate::x64::set_moving_young_override(Some(false));
        const ENTRY: usize = 0x7fff_0000_0000_5000;
        let n = ENTRY_ABI_REGS.len() + 2; // context + n args exceeds the file
        let code = lower_direct_call_with_n_args(n, ENTRY, true);

        assert!(
            contains_seq(&code, &(ENTRY as u64).to_le_bytes()),
            "the site must bake the callee entry, not fall back to dispatch"
        );
        assert!(
            !contains_seq(&code, &0x1111_2222_3333_4440u64.to_le_bytes()),
            "a bound direct site must not also emit the dispatch helper call"
        );

        // SUB RSP, imm32 / ADD RSP, imm32 with the SAME immediate: the block
        // is reserved and released around one call, so an unbalanced pair
        // would leave the frame's RSP permanently low.
        //
        // The FIRST `SUB RSP, imm32` in any body is the prologue's own frame
        // allocation, which shares this encoding — hence `skip(1)` rather than
        // `position`. Taking the first would assert against `frame_size` and
        // pass whether or not a stack-argument block was ever emitted.
        let subs: Vec<usize> = code
            .windows(3)
            .enumerate()
            .filter(|(_, w)| *w == [0x48, 0x81, 0xEC])
            .map(|(i, _)| i)
            .collect();
        assert!(
            subs.len() >= 2,
            "the prologue's frame allocation plus a stack-argument block: got {} SUB RSP",
            subs.len()
        );
        let sub_at = subs[1];
        let add_at = code
            .windows(3)
            .position(|w| w == [0x48, 0x81, 0xC4])
            .expect("the stack-argument block must be released");
        let sub_imm = i32::from_le_bytes(code[sub_at + 3..sub_at + 7].try_into().unwrap());
        let add_imm = i32::from_le_bytes(code[add_at + 3..add_at + 7].try_into().unwrap());
        assert_eq!(sub_imm, add_imm, "the reserve and the release must match");
        assert!(sub_imm > 0, "a stack-argument block must be non-empty here");
        assert_eq!(
            sub_imm % 16,
            0,
            "RSP must stay 16-byte aligned at the CALL, so the block is a multiple of 16"
        );
        assert!(add_at > sub_at, "the release must follow the reserve");

        // One `MOV [RSP + disp32], RAX` per argument past the register file.
        let stack_args = n - (ENTRY_ABI_REGS.len() - 1);
        assert_eq!(
            count_seq(&code, &[0x48, 0x89, 0x84, 0x24]),
            stack_args,
            "every argument past the entry-ABI register file must be stored to the block"
        );
    }

    /// The same lowering with arguments that DO fit emits no stack block at
    /// all — the narrow path is unchanged.
    ///
    /// Without this the test above would pass just as well against a backend
    /// that reserved a block on every direct call, which is a pessimisation of
    /// every call site in the tree.
    #[test]
    pub(in crate::ir_lower::tests) fn a_direct_call_within_the_register_file_reserves_no_stack_block() {
        crate::x64::set_moving_young_override(Some(false));
        const ENTRY: usize = 0x7fff_0000_0000_5000;
        let code = lower_direct_call_with_n_args(ENTRY_ABI_REGS.len() - 1, ENTRY, true);
        assert!(
            contains_seq(&code, &(ENTRY as u64).to_le_bytes()),
            "the narrow site must still bind directly"
        );
        assert_eq!(
            count_seq(&code, &[0x48, 0x81, 0xEC]),
            1,
            "only the prologue's own frame allocation — a call whose arguments \
             all fit registers must add no second SUB RSP"
        );
        assert_eq!(
            count_seq(&code, &[0x48, 0x89, 0x84, 0x24]),
            0,
            "a call whose arguments all fit registers must store nothing to the stack"
        );
    }

    /// A self-recursive call must not PUBLISH to the shadow stack.
    ///
    /// That route bypasses `emit_call_return_check`, the one site that emits
    /// the matching reload, so a push emitted here is never retracted: the
    /// thread's shadow `top` advances once per execution of the site and a
    /// recursive method walks it off the end of the shadow stack, after which
    /// the next push writes through into whatever follows the mapping. It
    /// presented as a SIGSEGV inside the C allocator's free list, on an
    /// unrelated thread, the moment reference field reads made
    /// `BinTreesClassic.itemCheck` IR-eligible.
    ///
    /// The lowerer used to emit the push and then "withdraw the claim" by
    /// clearing `pending_shadow` — which suppressed the RELOAD, not the push.
    #[test]
    pub(in crate::ir_lower::tests) fn self_recursive_call_balances_its_shadow_push_and_reload() {
        // The PER-CALL protocol is this test's subject; the frame block
        // publishes once per activation and has no reload to find.
        let _per_call = FrameBlockForce::set(false);
        crate::x64::set_moving_young_override(Some(false));
        // static int f(Obj o, int n) { return f(o, n); }
        // aload_0; iload_1; invokestatic #2; ireturn
        let code = [0x2a, 0x1b, 0xb8, 0x00, 0x02, 0xac, 0x00, 0x00];
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/Obj",
            method_name: "f",
            descriptor: "(Lpkg/Obj;I)I",
            num_jit_args: 2,
            return_type: b'I',
            invoke_kind: 4, // the direct self-recursive route
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(
            2usize,
            (info as *const JitInvokeInfo as usize, 2usize, b'I'),
        );
        builder.set_invoke_info(invoke_info);
        let graph = builder
            .build(&code, 6)
            .expect("IR build of self-recursive call");
        let schedule = ir_schedule::schedule(&graph);

        // The shadow machinery is live only when the thread helper is wired,
        // so a stub table with a zero `get_current_thread` would make this
        // test vacuous. Give it sentinels.
        let mut helpers = no_helpers();
        helpers.get_current_thread = 0x7fff_0000_0000_3000;
        helpers.shadow_stack_offset_in_thread = 0x40;
        helpers.self_call_stack_guard = 0x7fff_0000_0000_4000;

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let cm = lower_inner(
            &graph,
            &schedule,
            2,
            2,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect(
            "the self-recursive body must lower — an imbalance now makes \
             lower_inner discard it, so a None here means the push came back",
        );
        // Lowering at all is most of the assertion: `lower_inner` counts the
        // emitted pushes against the emitted reloads and DISCARDS the body when
        // they disagree, so a `None` above is exactly the bug this test exists
        // for. What remains to check is that the retraction is really emitted
        // on this route rather than inherited from a choke point it never
        // reaches: `MOV RCX, [R11]` reads a published value back, and only the
        // reload does that. (Deliberately not an exact count — the prologue's
        // own `top` watermark shares the push's encoding, so counting pushes
        // here measures the prologue too.)
        let bytes = cm.code_bytes();
        assert!(
            count_seq(bytes, &[0x49, 0x8B, 0x0B]) >= 1,
            "the self-recursive route bypasses emit_call_return_check, so it \
             must emit its own shadow reload — without it every execution of \
             the site advances the thread's shadow top and nothing retracts it"
        );
        assert!(
            !cm.shadow_frame_block,
            "a per-call body must not tell the collector's frame walk to stand down"
        );
        crate::x64::set_moving_young_override(None);
    }





    /// A planned inline-cache site must emit the full three-tier cascade:
    /// the receiver null check, ONE class-id load, the monomorphic guard, three
    /// polymorphic guards, four `CALL R11` cached-entry calls, and the
    /// `jit_invoke_virtual_mic` miss path — NOT the blind `jit_invoke_dispatch`
    /// helper the pre-2026-07-26 lowerer used for every virtual call.
    #[test]
    pub(in crate::ir_lower::tests) fn ic_site_emits_mic_pic_cascade_not_blind_dispatch() {
        // This test exercises the optimizing IR pipeline, which is gated off
        // whenever the young generation can relocate. Pin the policy so the
        // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
        crate::x64::set_moving_young_override(Some(false));
        const MIC: usize = 0x7fff_0000_0000_1000;
        let pic_slot = crate::inline_cache_pic::JitPICSlot::new();
        let pic_addr = &pic_slot as *const _ as usize;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, pic_addr));
        let code = lower_virtual_call_with_ic(&ic);

        // Receiver null check + the single class-id load feeding every guard.
        assert!(
            contains_seq(&code, &[0x48, 0x85, 0xC0]),
            "TEST RAX,RAX receiver null check must be emitted"
        );
        assert_eq!(
            count_seq(&code, &[0x8B, 0x00]),
            1,
            "the receiver class id must be loaded exactly ONCE for the whole cascade"
        );
        // Both slot base pointers are baked as imm64 (`MOV R10, imm64` = 49 BA).
        assert!(
            contains_seq(&code, &[0x49, 0xBA]) && contains_seq(&code, &MIC.to_le_bytes()),
            "the MIC slot address must be baked into the guard"
        );
        assert!(
            contains_seq(&code, &pic_addr.to_le_bytes()),
            "the PIC slot address must be baked into the cascade"
        );
        // MIC + four PIC entries + the hashed stub all call via R11. Since
        // round 9 wave 3 (invoke2 X2) the two hashed ways share ONE tail, so
        // the stub contributes one `CALL R11`, not one per way.
        assert_eq!(
            count_seq(&code, &[0x41, 0xFF, 0xD3]),
            1 + crate::JIT_PIC_ENTRIES + 1,
            "one cached-entry CALL R11 per MIC/PIC entry plus the hashed stub's shared tail"
        );
        // The miss path resolves AND populates via `jit_invoke_virtual_mic`.
        assert!(
            contains_seq(&code, &0x1111_2222_3333_4441u64.to_le_bytes()),
            "the inline-cache miss path must call jit_invoke_virtual_mic"
        );
        assert!(
            !contains_seq(&code, &0x1111_2222_3333_4440u64.to_le_bytes()),
            "a planned IC site must NOT also emit the blind jit_invoke_dispatch helper"
        );
    }

    /// Every cached entry below is selected by the 4-byte
    /// `ObjectHeader.class_id`, and a reference array stores its COMPONENT
    /// class id in that same word — so `Foo[]` and `Foo` are indistinguishable
    /// to a class-id-only guard, and a site warmed on a `Foo` receiver
    /// dispatched a later `Foo[]` receiver into `Foo`'s own body (whose first
    /// `checkcast Foo` threw `class [LFoo; cannot be cast to class Foo`).
    /// `ObjectHeader.kind` is what separates them.
    #[test]
    pub(in crate::ir_lower::tests) fn ic_cascade_rejects_array_receivers_before_the_class_id_guard() {
        // This test exercises the optimizing IR pipeline, which is gated off
        // whenever the young generation can relocate. Pin the policy so the
        // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
        crate::x64::set_moving_young_override(Some(false));
        const MIC: usize = 0x7fff_0000_0000_1000;
        let pic_slot = crate::inline_cache_pic::JitPICSlot::new();
        let pic_addr = &pic_slot as *const _ as usize;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, pic_addr));
        // Round 13 wave 11 (lane mega9, M13-6): an `Object`-owned site, which
        // an array CAN reach; a `pkg/Obj`-owned `invokevirtual` no longer
        // screens (`r13w11_mega9_ic_kind_screen_follows_the_static_owner`).
        let code = lower_virtual_call_with_ic_owner(&ic, 0, "java/lang/Object");

        // TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
        let guard = [
            0xF6u8,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ];
        assert!(
            contains_seq(&code, &guard),
            "the cascade must reject a non-object receiver kind before trusting the class id"
        );
        // ... and it must come BEFORE the single class-id load it protects.
        let guard_at = code
            .windows(guard.len())
            .position(|w| w == guard)
            .expect("guard present");
        let load_at = code
            .windows(2)
            .position(|w| w == [0x8B, 0x00])
            .expect("class-id load present");
        assert!(
            guard_at < load_at,
            "the kind guard must precede the class-id load (guard@{guard_at}, load@{load_at})"
        );
    }


    /// The guards must read the MIC/PIC fields at the offsets the shared slot
    /// types publish, not at hand-copied literals — this is what keeps the IR
    /// cascade layout-compatible with the single-pass one and with the runtime
    /// helper that populates both.
    #[test]
    pub(in crate::ir_lower::tests) fn ic_guards_use_published_slot_offsets() {
        // This test exercises the optimizing IR pipeline, which is gated off
        // whenever the young generation can relocate. Pin the policy so the
        // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
        crate::x64::set_moving_young_override(Some(false));
        use crate::{JitMICSlot, JitPICSlot};
        const MIC: usize = 0x7fff_0000_0000_1000;
        let pic_slot = crate::inline_cache_pic::JitPICSlot::new();
        let pic_addr = &pic_slot as *const _ as usize;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, pic_addr));
        let code = lower_virtual_call_with_ic(&ic);

        // MIC: class id at offset 0 → `CMP EAX,[R10]` = 41 3B 02;
        // tagged entry word at 8 → `MOV R11,[R10+8]` = 4D 8B 5A 08.
        assert_eq!(JitMICSlot::CACHED_CLASS_ID_OFFSET, 0);
        assert!(contains_seq(&code, &[0x41, 0x3B, 0x02]));
        assert!(contains_seq(
            &code,
            &[
                0x4D,
                0x8B,
                0x5A,
                crate::x64::disp::disp8_const(JitMICSlot::CACHED_ENTRY_PTR_OFFSET as i64) as u8,
            ]
        ));

        // PIC: one guard + one entry-word load per entry.
        for i in 0..crate::JIT_PIC_ENTRIES {
            let cid = JitPICSlot::CLASS_ID_OFFSETS[i] as u8;
            if cid == 0 {
                assert!(contains_seq(&code, &[0x41, 0x3B, 0x02]));
            } else {
                assert!(
                    contains_seq(&code, &[0x41, 0x3B, 0x42, cid]),
                    "PIC entry {i} class-id guard at offset {cid} must be emitted"
                );
            }
            assert!(
                contains_seq(
                    &code,
                    &[
                        0x4D,
                        0x8B,
                        0x5A,
                        crate::x64::disp::disp8_const(JitPICSlot::ENTRY_PTR_OFFSETS[i] as i64)
                            as u8,
                    ]
                ),
                "PIC entry {i} entry-word load must be emitted"
            );
        }

        // Every cached-entry call — the MIC, the four PIC ways and the two
        // hashed ways — tests the word it loaded for zero (a class-only profile
        // seed, or a withdrawn target) and strips the context tag from THAT
        // word. One of each per `CALL R11`.
        //
        // Round 9 wave 3 (invoke2 X2): the two hashed ways share one tail, so
        // they share its one `CALL R11` and its one `BTR` -- but each way
        // still loads, and so still zero-tests, its OWN word.
        let calls = count_seq(&code, &[0x41, 0xFF, 0xD3]);
        assert_eq!(calls, 1 + crate::JIT_PIC_ENTRIES + 1);
        // The hashed stub's out-of-line probes join that same tail, and each
        // tests what it loaded itself too (round 12 wave 4, lane mega3, who
        // read this count after wave 3 moved it from 7 to 11): the shared
        // table's probe (wave 3) loads and zero-tests the word of each of its
        // four ways before `JNZ tail`; the class-slot probe (wave 4) tests the
        // directory root, the receiver's array, dense cell word, sparse segment and
        // sparse cell word, each right
        // after the `MOV R11` that loaded it. None of them tests a register it
        // did not load, and none calls: `calls` and the `BTR` count stay put.
        let mega_table = crate::runtime_lowering::hashed_stub_mega_table_enabled();
        let class_slots = mega_table && crate::runtime_lowering::hashed_stub_class_slots_enabled();
        let probe_tests = if mega_table {
            crate::MEGA_DISPATCH_WAYS
        } else {
            0
        } + if class_slots { 5 } else { 0 };
        assert_eq!(
            count_seq(&code, &[0x4D, 0x85, 0xDB]),
            1 + crate::JIT_PIC_ENTRIES + crate::JIT_MEGA_WAYS + probe_tests,
            "every cached-entry load must reject a zero word it loaded itself"
        );
        assert_eq!(
            count_seq(&code, &[0x49, 0x0F, 0xBA, 0xF3, 0x00]),
            calls,
            "every cached-entry call must take its ABI from the word it calls"
        );
        // And nothing may still read a separate needs-context byte: the MIC's
        // old flag at offset 16, or the PIC's at 48..=51.
        for off in [16u8, 48, 49, 50, 51] {
            assert!(
                !contains_seq(&code, &[0x41, 0x80, 0x7A, off, 0x00]),
                "a separate needs-context byte at offset {off} must not be read"
            );
        }
        // Offset 48 is the megamorphic flag since round 12 wave 6 (lane
        // mega5, W5-4): ONE gate reads it as a dword (`83 /7`), after the MIC
        // and in front of the four rungs.
        let gate = [
            0x41u8,
            0x83,
            0x7A,
            JitPICSlot::MEGAMORPHIC_OFFSET as u8,
            0x00,
        ];
        if crate::runtime_lowering::megamorphic_gate_admitted(pic_addr, 2) {
            assert_eq!(count_seq(&code, &gate), 1, "one megamorphic gate");
            let gate_at = code.windows(gate.len()).position(|w| w == gate);
            let rung1 = [0x41u8, 0x3B, 0x42, JitPICSlot::CLASS_ID_OFFSETS[1] as u8];
            let rung1_at = code.windows(rung1.len()).position(|w| w == rung1);
            assert!(gate_at < rung1_at, "the gate precedes the rungs");
        } else {
            assert_eq!(count_seq(&code, &gate), 0, "no stub, no gate");
        }
    }

    /// Round 13 wave 5 (lane callcost2, M13-2): the megamorphic region's
    /// class-id-carrying edges stage through R11 and jump to the hashed stub's
    /// gate entry (`MOV EDX, EAX; JMP`), exactly once per site; the raw edges
    /// (null, kind) keep the full entry. Structural: the executed stub tests in
    /// `runtime_lowering` cover the gate entry's own behaviour.
    #[test]
    pub(in crate::ir_lower::tests) fn r13w5_screened_megamorphic_edges_enter_the_stub_gate_entry() {
        crate::x64::set_moving_young_override(Some(false));
        const MIC: usize = 0x7fff_0000_0000_1000;
        let pic_slot = crate::inline_cache_pic::JitPICSlot::new();
        let pic_addr = &pic_slot as *const _ as usize;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, pic_addr));
        let code = lower_virtual_call_with_ic(&ic);
        let on = crate::runtime_lowering::mega_gate_fast_entry_enabled()
            && crate::runtime_lowering::hashed_stub_admitted(pic_addr, 2);
        // The stub's gate entry: `MOV EDX, EAX` then `JMP rel32` back.
        let entries: Vec<usize> = (0..code.len().saturating_sub(3))
            .filter(|&i| code[i..i + 3] == [0x89, 0xC2, 0xE9])
            .collect();
        // Every `JMP rel32` that lands on one of them.
        let jumps_in: Vec<usize> = (0..code.len().saturating_sub(5))
            .filter(|&i| {
                code[i] == 0xE9 && {
                    let rel = i32::from_le_bytes(code[i + 1..i + 5].try_into().expect("rel32"));
                    let target = i as i64 + 5 + i64::from(rel);
                    entries.iter().any(|&e| e as i64 == target)
                }
            })
            .collect();
        if !on {
            assert!(
                jumps_in.is_empty(),
                "switched off: nothing enters a gate entry"
            );
            return;
        }
        assert_eq!(entries.len(), 1, "one stub, one gate entry");
        assert_eq!(jumps_in.len(), 1, "the screened edges share one jump to it");
        // That jump is preceded by the R11 staging store of the last argument:
        // `MOV [RBP - disp8], R11` (`4C 89 5D d8`) or its disp32 form
        // (`4C 89 9D d32`).
        let j = jumps_in[0];
        assert!(
            (j >= 4 && code[j - 4..j - 1] == [0x4C, 0x89, 0x5D])
                || (j >= 7 && code[j - 7..j - 4] == [0x4C, 0x89, 0x9D]),
            "the gate edge stages through R11, keeping EAX (bytes before the \
             jump: {:02x?})",
            &code[j.saturating_sub(4)..j]
        );
    }



    /// No plan ⇒ the historical generic-dispatch lowering, unchanged. This is
    /// the guarantee that every un-planned call site in every method is
    /// byte-identical to the pre-2026-07-26 backend.
    #[test]
    pub(in crate::ir_lower::tests) fn absent_ic_plan_keeps_generic_dispatch() {
        let empty: HashMap<usize, (usize, usize)> = HashMap::new();
        let code = lower_virtual_call_with_ic(&empty);
        assert!(
            contains_seq(&code, &0x1111_2222_3333_4440u64.to_le_bytes()),
            "an unplanned virtual site must still route through jit_invoke_dispatch"
        );
        assert!(
            !contains_seq(&code, &0x1111_2222_3333_4441u64.to_le_bytes()),
            "an unplanned site must NOT reach the inline-cache miss helper"
        );
        assert_eq!(
            count_seq(&code, &[0x41, 0xFF, 0xD3]),
            0,
            "an unplanned site must emit no cached-entry CALL R11"
        );
    }

    /// Every branch the inline-cache cascade emits is patched before the
    /// emitter returns, so no `rel32` operand is left as its `00 00 00 00`
    /// placeholder. A stale placeholder would jump to the instruction
    /// immediately after itself — a silent fall-through into the next cache
    /// entry's body rather than to `.pic` / `.slow` / `.done`.
    ///
    /// (`patch_rel32_to_here` swallows patch errors by design, because they are
    /// only reachable on an overflowed buffer that `lower_inner` discards — so
    /// this test is what proves the non-overflow path really does patch.)
    #[test]
    pub(in crate::ir_lower::tests) fn ic_cascade_leaves_no_unpatched_branch() {
        const MIC: usize = 0x7fff_0000_0000_1000;
        let pic_slot = crate::inline_cache_pic::JitPICSlot::new();
        let pic_addr = &pic_slot as *const _ as usize;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, pic_addr));
        let code = lower_virtual_call_with_ic(&ic);

        // Scan for a Jcc rel32 / JMP rel32 whose displacement is still zero.
        for w in 0..code.len().saturating_sub(6) {
            let is_jcc = code[w] == 0x0F && (code[w + 1] == 0x84 || code[w + 1] == 0x85);
            if is_jcc && code[w + 2..w + 6] == [0, 0, 0, 0] {
                panic!("unpatched Jcc rel32 placeholder at native offset {w}");
            }
        }
        for w in 0..code.len().saturating_sub(5) {
            if code[w] == 0xE9 && code[w + 1..w + 5] == [0, 0, 0, 0] {
                panic!("unpatched JMP rel32 placeholder at native offset {w}");
            }
        }
    }

    /// A site whose receiver + arguments + hidden VM context pointer overflow
    /// the entry ABI register file must fall back to the helper rather than
    /// emit a call with garbage in an unwritten register. On Win64 the file is
    /// 4 registers wide, so a 4-argument virtual site (receiver + 3) already
    /// needs 5 and must decline; SysV has 6, so the same shape fits there and
    /// the assertion is platform-specific by construction.
    #[test]
    pub(in crate::ir_lower::tests) fn ic_declines_when_args_overflow_the_abi_register_file() {
        // Since round 11 wave 13 the guard is `ir_ic_admits_arity`: the file
        // plus `ir_ic_max_stack_words()` context-ABI stack words. Pin the
        // constants rather than re-deriving them, so a future ABI change has
        // to visit this test.
        #[cfg(target_os = "windows")]
        assert_eq!(ENTRY_ABI_REGS.len(), 4);
        #[cfg(not(target_os = "windows"))]
        assert_eq!(ENTRY_ABI_REGS.len(), 6);
        // The shared planner bound must agree with the lowerer's.
        assert_eq!(crate::ir_entry_abi_reg_count(), ENTRY_ABI_REGS.len());
        let regs = ENTRY_ABI_REGS.len();
        assert!(
            !ir_ic_admits_arity(0),
            "a virtual site always has a receiver"
        );
        for n in 1..regs {
            assert!(
                ir_ic_admits_arity(n),
                "n={n} fits the file with the context word"
            );
            assert_eq!(
                Lowerer::ic_wide_stack_block(n),
                None,
                "n={n} needs no stack block"
            );
        }
        // `HashMap.newNode` on Win64 (receiver + 4): two context-ABI words.
        for n in regs..=(regs + ir_ic_max_stack_words() - 1) {
            assert!(ir_ic_admits_arity(n), "n={n} overflows by at most the cap");
            let (total, base) =
                Lowerer::ic_wide_stack_block(n).expect("an admitted over-wide site has a block");
            let words = (n + 1 - regs) as i32;
            assert_eq!(total % 16, 0, "n={n}: the CALL must stay 16-aligned");
            assert!(
                total >= base + words * 8,
                "n={n}: the block holds every word"
            );
            assert_eq!((total, base), stack_arg_block_size(n + 1 - regs));
        }
        // Past the cap the site keeps the helper, in the planner and here.
        let too_wide = regs + ir_ic_max_stack_words();
        assert!(!ir_ic_admits_arity(too_wide));
        assert_eq!(Lowerer::ic_wide_stack_block(too_wide), None);
    }

    /// Lower `aload_0; iconst_0 x (n-1); invokevirtual #2; ireturn` with the
    /// site's MIC/PIC planned, the callee-deopt service wired, and return the
    /// bytes. `n` counts the receiver.
    pub(in crate::ir_lower::tests) fn lower_wide_virtual_call_with_ic(n: usize) -> Vec<u8> {
        const MIC: usize = 0x7fff_0000_0000_1000;
        let pic_slot = crate::inline_cache_pic::JitPICSlot::new();
        let pic_addr = &pic_slot as *const _ as usize;
        let mut code: Vec<u8> = vec![0x2a]; // aload_0
        for _ in 1..n {
            code.push(0x03); // iconst_0
        }
        let invoke_pc = code.len();
        code.extend_from_slice(&[0xb6, 0x00, 0x02, 0xac]); // invokevirtual #2; ireturn
        let code_len = code.len();
        code.extend_from_slice(&[0x00, 0x00]);
        let descriptor: &'static str =
            Box::leak(format!("({})I", "I".repeat(n - 1)).into_boxed_str());
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/Wide",
            method_name: "newNode",
            descriptor,
            num_jit_args: n,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let mut builder = IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Ref]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(invoke_pc, (info as *const JitInvokeInfo as usize, n, b'I'));
        builder.set_invoke_info(invoke_info);
        let graph = builder
            .build(&code, code_len)
            .expect("IR build of a wide virtual call");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.invoke_dispatch = 0x1111_2222_3333_4440;
        helpers.invoke_virtual_mic = 0x1111_2222_3333_4441;
        helpers.dispatch_threw = 0x1111_2222_3333_4442;
        helpers.service_callee_deopt = 0x1111_2222_3333_4443;
        let mut ic = HashMap::new();
        ic.insert(invoke_pc, (MIC, pic_addr));
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            1,
            1,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("a wide virtual call must lower")
        .code_bytes()
        .to_vec()
    }


    /// Past `ir_ic_max_stack_words()` the site keeps the blind dispatch even when
    /// a plan hands it slots: the lowerer's gate is the planner's.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w13_a_site_past_the_stack_word_cap_keeps_the_dispatch_helper() {
        crate::x64::set_moving_young_override(Some(false));
        let n = ENTRY_ABI_REGS.len() + ir_ic_max_stack_words();
        let code = lower_wide_virtual_call_with_ic(n);
        crate::x64::set_moving_young_override(None);
        assert!(contains_seq(&code, &0x1111_2222_3333_4440u64.to_le_bytes()));
        assert_eq!(count_seq(&code, &[0x41, 0xFF, 0xD3]), 0);
    }

    /// The context shift moves arguments in DESCENDING order, so no argument is
    /// overwritten before it has been read.
    ///
    /// `emit_ic_premarshal_no_context` builds the no-context layout once, ahead
    /// of the whole inline-cache cascade, and a context-needing arm converts it
    /// by shifting every argument up one register. Ascending order would write
    /// `ENTRY_ABI_REGS[1]` from `[0]` before reading `[1]`, so argument 1 would
    /// become a second copy of argument 0 and every argument above it the same
    /// — silently, and only for callees that need the context pointer.
    ///
    /// Asserted on the decoded `(dst, src)` pairs rather than on literal bytes,
    /// because `ENTRY_ABI_REGS` differs between Win64 and System V and a
    /// byte-literal test would pin one platform's answer as the property.
    #[test]
    pub(in crate::ir_lower::tests) fn the_context_shift_moves_arguments_from_the_top_down() {
        for num_args in 1..=(ENTRY_ABI_REGS.len() - 1) {
            let mut lo = lowerer_with_resident_xmm(4096, None);
            let at = lo.buf.pos();
            lo.emit_ic_shift_for_context(num_args);
            let code = lo.buf.as_slice()[at..].to_vec();

            // `MOV r64, r64` is `REX.W(+R+B) 89 ModRM(mod=11)`. Decode every one
            // in order; the trailing context load is `8B` and is skipped.
            let mut moves: Vec<(u8, u8)> = Vec::new();
            let mut i = 0usize;
            while i + 2 < code.len() {
                if (0x48..=0x4F).contains(&code[i]) && code[i + 1] == 0x89 && code[i + 2] >= 0xC0 {
                    let rex = code[i];
                    let modrm = code[i + 2];
                    let src = ((modrm >> 3) & 7) | (((rex >> 2) & 1) << 3);
                    let dst = (modrm & 7) | ((rex & 1) << 3);
                    moves.push((dst, src));
                    i += 3;
                } else {
                    i += 1;
                }
            }

            let want: Vec<(u8, u8)> = (0..num_args)
                .rev()
                .map(|k| (ENTRY_ABI_REGS[k + 1], ENTRY_ABI_REGS[k]))
                .collect();
            assert_eq!(
                moves, want,
                "num_args={num_args}: the shift must run from the top down, so \
                 every destination holds a value that has already been moved",
            );

            // …and the context lands in ABI[0], after the shift has vacated it.
            // `MOV r64, [rbp - disp]` is `REX.W 8B ModRM`, and ABI[0]'s encoding
            // appears in the ModRM `reg` field.
            let ctx = ENTRY_ABI_REGS[0];
            let ctx_loaded = code.windows(3).any(|w| {
                (0x48..=0x4F).contains(&w[0])
                    && w[1] == 0x8B
                    && ((w[2] >> 3) & 7) == (ctx & 7)
                    && (w[2] & 0xC0) != 0xC0
            });
            assert!(
                ctx_loaded,
                "num_args={num_args}: the context pointer must be loaded into \
                 ABI[0] once the shift has vacated it",
            );
        }
    }


    // ── wire-tiered-manager Step 4: PGO branch-bias in the IR (C2) path ──

    /// The optimizing IR lowerer must consume the profiled branch bias: the two
    /// hints emit opposite branch polarities, and a hint outranks the block
    /// layout.
    ///
    /// # What this test used to assert, and why it changed on 2026-09-11
    ///
    /// It used to pin the UNHINTED emission as the reference — "no hint ⇒ the
    /// historical `JE` layout, and a usually-taken hint reproduces it
    /// byte-for-byte". That reference moved deliberately: with no hint the arm
    /// now takes its fall-through edge from the block layout
    /// (`ir_branch_layout_polarity_enabled`), because the previous fallback —
    /// "the `true` edge is the near one" — is inverted for every javac counted
    /// loop and cost one taken branch and one `JMP rel32` per iteration.
    ///
    /// On THIS fixture the layout happens to agree with the not-taken hint, so
    /// the old assertion ("a not-taken hint must invert the baseline") could no
    /// longer hold however the code behaved: baseline and hinted arm are now
    /// the same bytes. That is the change working, not the hint failing.
    ///
    /// **So the property is restated against the two hints rather than against
    /// the baseline**, which is what the test was always about and is immune to
    /// the default moving again: the two hints must disagree with each other,
    /// in opposite polarities, and each must be reachable. The unhinted arm is
    /// still asserted — against the LAYOUT's answer, and equal to whichever
    /// hint agrees with it, which is the fact that would otherwise be silently
    /// lost.
/// A hint for an UNRELATED bytecode PC must not perturb codegen — only the
    /// branch whose own PC is marked not-taken inverts.
    #[test]
    pub(in crate::ir_lower::tests) fn step4_ir_lower_branch_bias_keyed_by_pc() {
        let code = [0x1a, 0x99, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac];
        let build = || {
            let builder = IrBuilder::new(1, 1);
            let mut graph = builder.build(&code, 8).expect("IR build");
            ir_optimize::optimize(&mut graph);
            let schedule = ir_schedule::schedule(&graph);
            (graph, schedule)
        };
        let (g0, s0) = build();
        let base = lower(&g0, &s0, 1, 1, &no_helpers())
            .expect("lower")
            .code_bytes()
            .to_vec();
        // Hint a PC that does not correspond to this method's branch (7).
        let mut hints = HashMap::new();
        hints.insert(7usize, false);
        let (g1, s1) = build();
        let other = lower_with_branch_hints(&g1, &s1, 1, 1, &no_helpers(), &hints)
            .expect("lower")
            .code_bytes()
            .to_vec();
        assert_eq!(
            base, other,
            "a hint for an unrelated PC must not change codegen"
        );
    }

    // ── real-frame-deopt step 2: deopt points + lookup ───────────────────

// ── real-frame-deopt step 3: route one guard end-to-end ──────────────

    #[test]
    pub(in crate::ir_lower::tests) fn test_guard_deopt_reconstructs_live_frame() {
        use crate::deopt::{take_last_deopt, FrameValue};
        use crate::ir::SafepointSnapshot;

        // Hand-build: i64 f(i64 cond, i64 val) {
        //     guard(cond != 0) [bci 5];   // deopt if cond == 0
        //     return val;
        // }
        // Node layout mirrors IrBuilder::new.
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let val = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        let _guard = graph.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, cond], None);
        let ret = graph.add(Op::Return, IrType::Void, vec![ctrl, val], None);
        graph.exit = ret;

        // Safepoint at the guard's bci: locals = [cond, val], empty stack.
        graph.safepoints.push(SafepointSnapshot::new(
            5,
            vec![cond, val],
            vec![],
            Vec::new(),
        ));

        let schedule = ir_schedule::schedule(&graph);
        let method = lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lower guarded method");

        // Guard passes (cond != 0): normal return of `val`.
        let _ = take_last_deopt(); // clear any stale state
                                   // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let ok = unsafe { method.try_call(&[1, 777]).expect("call (guard ok)") };
        assert_eq!(ok, 777, "guard passes → returns val");
        assert!(take_last_deopt().is_none(), "no deopt when guard passes");

        // Guard fails (cond == 0): deopt sentinel returned, frame reconstructed
        // from the LIVE machine frame — locals resolved to the actual argument
        // values sitting in their spill slots.
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let sentinel = unsafe { method.try_call(&[0, 777]).expect("call (guard fail)") };
        assert_eq!(sentinel, i64::MIN, "guard fails → deopt sentinel");
        let frame = take_last_deopt().expect("deopt reconstructed a frame");
        assert_eq!(frame.bci, 5, "resumes at the guard's bci");
        assert_eq!(
            frame.locals,
            vec![FrameValue::Int(0), FrameValue::Int(777)],
            "locals read back from the live native frame",
        );
        assert!(frame.stack.is_empty());
        assert!(frame.caller_frames.is_empty());
    }

    // ── Guard-surviving scalar replacement (producer) ────────────────────

    /// Build a 3-block graph modelling a scalar-replaced object live at a guard:
    ///
    /// ```text
    /// block0 (entry):  o = new Foo(); o.x = 7; if (cond) ...   (New + store here)
    /// block1 (taken):  guard(cond != 0) [bci 10]; return o.x   (deopt point here)
    /// block2 (else):   return 0
    /// ```
    ///
    /// `same_block_guard` puts the guard in block0 instead (no `If`), created
    /// BEFORE the New/store — so the allocation neither dominates the guard's
    /// block nor precedes the guard inside it: the temporal-hazard bail case.
    /// `dup_local` puts the object in TWO local slots (sharing → `VirtualObjectRef`).
    /// Returns `(graph, sr_map, new_id)`; the New + store are marked `Op::Dead`
    /// (simulating `apply_ea_to_ir`) and `sr_map` captures their control inputs.
    pub(in crate::ir_lower::tests) fn build_sr_deopt_graph(
        same_block_guard: bool,
        dup_local: bool,
    ) -> (Graph, ScalarReplacementMap, NodeId) {
        use crate::ir::CmpOp;
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
        // Same-block mode: the guard comes FIRST, so in block0's execution order
        // the deopt fires before the object exists.
        if same_block_guard {
            let _guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![c0, cond], None);
        }
        // o = new Foo(); o.x = 7  (both controlled by c0 → block0)
        let newo = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![c0, mem],
            None,
        );
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None); // field index 0
        let v7 = g.add(Op::Const(7), IrType::Int, vec![], None); // field value
        let store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![c0, mem, newo, f0, v7],
            None,
        );
        let new_ctrl = c0;
        let store_ctrl = c0;

        // Locals at the guard's safepoint: [cond, o] (or [cond, o, o] for sharing).
        let locals = if dup_local {
            vec![cond, newo, newo]
        } else {
            vec![cond, newo]
        };

        let guard_ctrl = if same_block_guard {
            // Guard already in block0 (before the New), no branch → same block.
            let ret = g.add(Op::Return, IrType::Void, vec![c0, v7], None);
            g.exit = ret;
            c0
        } else {
            // if (cond) → block1 (taken) / block2 (else); guard in block1.
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cond, zero], None);
            let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let e = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![t, cond], None);
            let _ = guard;
            let ret1 = g.add(Op::Return, IrType::Void, vec![t, v7], None);
            let ret2 = g.add(Op::Return, IrType::Void, vec![e, v7], None);
            g.exit = ret1;
            let _ = ret2;
            t
        };
        let _ = guard_ctrl;

        g.safepoints
            .push(SafepointSnapshot::new(10, locals, vec![], Vec::new()));

        // Simulate `apply_ea_to_ir`: mark the New + store dead (their inputs are
        // cleared), exactly as the production path does before scheduling.
        g.nodes[newo as usize].op = Op::Dead;
        g.nodes[newo as usize].inputs.clear();
        g.nodes[store as usize].op = Op::Dead;
        g.nodes[store as usize].inputs.clear();

        let mut objects = HashMap::new();
        objects.insert(
            newo,
            VirtualObjectInfo {
                non_null_cond: None,
                loop_phis: None,
                array_element_type: None,
                class_id: 7,
                num_fields: 1,
                field_values: vec![Some(v7)],
                new_ctrl,
                store_ctrls: vec![store_ctrl],
                store_nodes: vec![store],
                store_fields: Vec::new(),
            },
        );
        (g, ScalarReplacementMap { objects }, newo)
    }

    /// Find the deopt point at `bci` among a lowered method's baked guard boxes
    /// (`_deopt_point_boxes` — the runtime-used frame states; an `Op::Guard`
    /// emits its box there via `emit_deopt_unless`, NOT into the `bci_native`-keyed
    /// `deopt_points` list).
    pub(in crate::ir_lower::tests) fn deopt_locals_at(cm: &CompiledMethod, bci: u32) -> Vec<FrameValue> {
        cm._deopt_point_boxes
            .iter()
            .find(|p| p.bci == bci)
            .unwrap_or_else(|| panic!("no deopt box at bci {bci}"))
            .frame_state
            .locals
            .clone()
    }


    /// A division the bytecode reaches on only one arm must deopt from its
    /// control-anchored `Op::Guard`, not from the floating `Op::Div` the
    /// scheduler placed wherever its operands happened to be available.
    ///
    /// Before this, `MemoryEstimator.estimateMemory` (H2's `MVMap` key/value
    /// size estimator) had its `ldiv` hoisted above the branch that guarantees
    /// a positive divisor, and the lowerer's own guard then deopted on a path
    /// the interpreter never takes — which the interpreter faithfully resumed
    /// into, throwing `ArithmeticException: / by zero` out of a program that
    /// cannot divide by zero.
    #[test]
    pub(in crate::ir_lower::tests) fn a_guarded_divisions_trap_comes_from_the_guard_not_the_floating_div() {
        // 0: iload_0  1: ifne +7 (→8)  4: iload_1  5: iload_2  6: idiv
        // 7: ireturn  8: iconst_0  9: ireturn
        let code = [
            0x1a, 0x9a, 0x00, 0x07, 0x1b, 0x1c, 0x6c, 0xac, 0x03, 0xac, 0, 0,
        ];
        let graph = crate::ir::IrBuilder::new(3, 3)
            .build(&code, 10)
            .expect("IR build failed");
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower_inner(
            &graph,
            &schedule,
            3,
            3,
            &no_helpers(),
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("lower");

        let at_div: Vec<_> = cm
            ._deopt_point_boxes
            .iter()
            .filter(|p| p.bci == 6)
            .collect();
        assert_eq!(
            at_div.len(),
            1,
            "one deopt point at the idiv's bci, not one per emitter"
        );
        assert_eq!(
            at_div[0].reason,
            DeoptReason::UncommonTrap,
            "the surviving trap is the control-anchored Op::Guard's; a \
             `DivByZero` here means the floating div still deopts"
        );
        assert!(
            !cm._deopt_point_boxes
                .iter()
                .any(|p| p.reason == DeoptReason::DivByZero),
            "the lowerer must not emit its own div-by-zero deopt once the \
             builder anchored a guard"
        );
    }

    /// Round 12 wave 3 (lane replay): an IR SITE TRAP's point carries
    /// `ResumeSemantics::first_arrival` (the VM's replay rule reads it), and a
    /// conditional guard in the same body does not — here the div-by-zero
    /// guard, whose `UncommonTrap` reason and `REEXECUTE` resume are the same.
    #[test]
    pub(in crate::ir_lower::tests) fn r12w3_only_a_site_trap_point_is_first_arrival() {
        // 0: invokedynamic #1 (void, no arguments)
        // 5: iload_0  6: iload_1  7: idiv  8: ireturn
        let code = [0xba, 0x00, 0x01, 0x00, 0x00, 0x1a, 0x1b, 0x6c, 0xac, 0, 0];
        let mut builder = crate::ir::IrBuilder::new(2, 2);
        let mut sites = HashMap::new();
        sites.insert(0usize, (0usize, b'V'));
        builder.set_indy_trap_sites(sites);
        let graph = builder.build(&code, 9).expect("IR build failed");
        assert_eq!(graph.site_trap_pcs, vec![0], "the builder lists its trap");
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower_inner(
            &graph,
            &schedule,
            2,
            2,
            &no_helpers(),
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("lower");
        let trap = cm
            ._deopt_point_boxes
            .iter()
            .find(|p| p.bci == 0)
            .expect("the site trap's point");
        assert!(trap.semantics.first_arrival);
        assert_eq!(trap.semantics.resume_kind(), ResumeSemantics::REEXECUTE);
        let div = cm
            ._deopt_point_boxes
            .iter()
            .find(|p| p.bci == 7)
            .expect("the div guard's point");
        assert_eq!(div.reason, DeoptReason::UncommonTrap);
        assert!(
            !div.semantics.first_arrival,
            "a conditional guard may fire on a later arrival"
        );
    }

    /// Round 16 (lane indy, SY4-1): the site word a bridged indy bakes.
    /// Eight distinct bytes, so a match in the code is the immediate.
    pub(in crate::ir_lower::tests) const R16_INDY_SITE: usize = 0x5A5A_1357_9BDF_2468;

    /// Build `code` with the bridged `invokedynamic` at `pc` and lower it with
    /// `indy_bridge` as the bridge entry; `None` when the lowering refuses.
    pub(in crate::ir_lower::tests) fn r16_lower_bridged_indy(
        code: &[u8],
        len: usize,
        params: usize,
        pc: usize,
        shape: (usize, u8),
        indy_bridge: usize,
        dispatch_threw: usize,
    ) -> Option<CompiledMethod> {
        let mut builder = crate::ir::IrBuilder::new(params, params.max(1));
        let mut traps = HashMap::new();
        traps.insert(pc, shape);
        builder.set_indy_trap_sites(traps);
        let mut bridged = HashMap::new();
        bridged.insert(pc, R16_INDY_SITE);
        builder.set_indy_bridge_sites(bridged);
        let graph = builder.build(code, len).expect("IR build");
        assert!(
            graph.nodes.iter().any(|n| n.op
                == Op::IndyBridge {
                    site: R16_INDY_SITE
                }),
            "the builder bridged the site"
        );
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.dispatch_threw = dispatch_threw;
        let direct = crate::DirectHelperTable {
            indy_bridge,
            ..crate::DirectHelperTable::EMPTY
        };
        lower_inner_with_direct_helpers(
            &direct,
            &graph,
            &schedule,
            params,
            params.max(1),
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
    }

    pub(in crate::ir_lower::tests) fn r16_occurrences(code: &[u8], word: usize) -> usize {
        let target = word.to_le_bytes();
        code.windows(8).filter(|w| *w == target).count()
    }

    /// Round 16 (lane indy, SY4-1): an `Op::IndyBridge` lowers to exactly one
    /// call of `DirectHelperTable::indy_bridge` -- the single-pass tier's
    /// entry -- with the site word baked as its second argument, and no deopt
    /// point at the indy bci: the site is not a trap.
    #[test]
    pub(in crate::ir_lower::tests) fn r16_a_bridged_indy_lowers_to_one_bridge_call() {
        const BRIDGE: usize = 0x1111_2222_3333_4466;
        // 0: iload_0  1: iload_1  2: invokedynamic #1 (II)I  7: ireturn
        let code = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
        let cm = r16_lower_bridged_indy(&code, 8, 2, 2, (2, b'I'), BRIDGE, 0)
            .expect("an int-returning bridged indy lowers");
        let bytes = cm.code_bytes();
        assert_eq!(r16_occurrences(bytes, BRIDGE), 1, "one CALL of the bridge");
        assert_eq!(
            r16_occurrences(bytes, R16_INDY_SITE),
            1,
            "the site word, once"
        );
        assert!(
            !cm._deopt_point_boxes
                .iter()
                .any(|p| p.bci == 2 && p.reason == DeoptReason::UncommonTrap),
            "a bridged site carries no trap point"
        );
    }


    /// Round 16 (lane indy): every refusal the arm owns refuses the compile
    /// instead of emitting a CALL through address 0 or a sentinel check whose
    /// cold path calls address 0.
    #[test]
    pub(in crate::ir_lower::tests) fn r16_a_bridged_indy_refuses_without_its_helpers() {
        const BRIDGE: usize = 0x1111_2222_3333_4466;
        const THREW: usize = 0x1111_2222_3333_4477;
        let int_site = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
        assert!(
            r16_lower_bridged_indy(&int_site, 8, 2, 2, (2, b'I'), 0, THREW).is_none(),
            "no bridge entry, no body"
        );
        // 0: iload_0  1: invokedynamic #1 (I)J  6: lreturn
        let long_site = [0x1a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xad, 0, 0];
        assert!(
            r16_lower_bridged_indy(&long_site, 7, 1, 1, (1, b'J'), BRIDGE, 0).is_none(),
            "a `long` result needs `dispatch_threw` to tell Long.MIN_VALUE from the sentinel"
        );
        let cm = r16_lower_bridged_indy(&long_site, 7, 1, 1, (1, b'J'), BRIDGE, THREW)
            .expect("with it, the long site lowers");
        assert_eq!(r16_occurrences(cm.code_bytes(), BRIDGE), 1);
        assert!(
            r16_occurrences(cm.code_bytes(), THREW) >= 1,
            "the wide sentinel peek, on the cold side"
        );
    }


    pub(in crate::ir_lower::tests) fn r12w3_no_osr_entry_jumps_past_a_range_predicate_without_recheck() {
        let _osr = OsrEntryForce::on();
        //  0: iconst_0   1: istore_2   2: iconst_0   3: istore_3
        //  4: iload_3    5: iload_1    6: if_icmpge 21
        //  9: iload_2   10: aload_0   11: iload_3   12: iaload   13: iadd
        // 14: istore_2  15: iinc 3,1  18: goto 4    21: iload_2  22: ireturn
        let code = [
            0x03, 0x3d, 0x03, 0x3e, 0x1d, 0x1b, 0xa2, 0x00, 0x0f, 0x1c, 0x2a, 0x1d, 0x2e, 0x60,
            0x3d, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf2, 0x1c, 0xac, 0, 0,
        ];
        let mut builder = IrBuilder::new(2, 4);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        let mut graph = builder.build(&code, 23).expect("IR build");
        let despec = crate::deopt::DespecRegistry::new();
        ir_optimize::optimize_with(
            &mut graph,
            ir_optimize::OptimizeContext {
                despec: Some(&despec),
                method_key: "R12W3.sum:([II)I",
                osr_route: false,
            },
        );
        let planted = graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { bci: 4 }));
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower(&graph, &schedule, 2, 4, &no_helpers()).expect("the loop lowers");
        if !planted {
            // No predicate, nothing for an entry to skip.
            return;
        }
        assert!(
            cm.ir_osr_entry_addr(4).is_none(),
            "an entry at the header would run the loop with its bounds check \
             elided on a bound nobody checked"
        );
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_scalar_deopt_emits_virtual_object() {
        // A scalar-replaced object whose New + (constant) field store strictly
        // dominate the guard's block lowers to a `VirtualObject` with the stored
        // value as its field — the guard-surviving case.
        let (g, sr_map, newo) = build_sr_deopt_graph(false, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        // local[1] is the object → VirtualObject{ class_id:7, field_values:[Int(7)] }.
        match &locals[1] {
            FrameValue::VirtualObject(state) => {
                assert_eq!(state.id, newo as usize);
                assert_eq!(state.class_id, 7);
                assert_eq!(state.num_fields, 1);
                assert_eq!(state.field_values, vec![FrameValue::Int(7)]);
            }
            other => panic!("expected VirtualObject, got {other:?}"),
        }
    }

    /// The commonest shape there is: `o = new Foo(); o.x = 7; ...; guard` in ONE
    /// straight-line block. Strict block dominance cannot answer (same block),
    /// and until 2026-09-12 that alone refused the recipe — so an object live at
    /// a guard in a branch-free method could never be described. The allocation
    /// and its store precede the guard in the block's execution order, which is
    /// the builder's node-id order, and the recipe is emitted.
    #[test]
    pub(in crate::ir_lower::tests) fn test_scalar_deopt_describes_an_object_stored_earlier_in_the_guards_block() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let newo = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![c0, mem],
            None,
        );
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let v7 = g.add(Op::Const(7), IrType::Int, vec![], None);
        let store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![c0, mem, newo, f0, v7],
            None,
        );
        // AFTER the store, same control → same block, later in it.
        let _guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![c0, cond], None);
        let ret = g.add(Op::Return, IrType::Void, vec![c0, v7], None);
        g.exit = ret;
        g.safepoints.push(SafepointSnapshot::new(
            10,
            vec![cond, newo],
            vec![],
            Vec::new(),
        ));
        g.nodes[newo as usize].op = Op::Dead;
        g.nodes[newo as usize].inputs.clear();
        g.nodes[store as usize].op = Op::Dead;
        g.nodes[store as usize].inputs.clear();
        let mut objects = HashMap::new();
        objects.insert(
            newo,
            VirtualObjectInfo {
                non_null_cond: None,
                loop_phis: None,
                array_element_type: None,
                class_id: 7,
                num_fields: 1,
                field_values: vec![Some(v7)],
                new_ctrl: c0,
                store_ctrls: vec![c0],
                store_nodes: vec![store],
                store_fields: Vec::new(),
            },
        );
        let sr_map = ScalarReplacementMap { objects };

        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        match &deopt_locals_at(&cm, 10)[1] {
            FrameValue::VirtualObject(state) => {
                assert_eq!(state.id, newo as usize);
                assert_eq!(state.field_values, vec![FrameValue::Int(7)]);
            }
            other => panic!("expected VirtualObject, got {other:?}"),
        }
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_scalar_deopt_bails_when_store_not_dominating() {
        // Guard in the SAME block as the New/store but BEFORE them → neither
        // strict dominance nor the in-block order proves the object existed
        // when the guard fires. The bail names the
        // eliminated allocation instead of claiming the slot is undefined, so
        // the resume is REFUSED (safe whole-method re-run) rather than served a
        // fabricated `Int(0)` — which for this reference local would be `null`.
        let (g, sr_map, newo) = build_sr_deopt_graph(true, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[1],
            FrameValue::MaterializationRequired(EliminatedValue::allocation(
                newo,
                7,
                EliminationCause::ScalarReplacedObject,
            )),
            "a non-dominating store must bail to MaterializationRequired, naming \
             the producer node and the class it allocated"
        );
        let fs = &cm
            ._deopt_point_boxes
            .iter()
            .find(|p| p.bci == 10)
            .expect("deopt box at bci 10")
            .frame_state;
        assert!(
            !crate::deopt::frame_state_is_resumable(fs),
            "the frame must be unresumable, which is what turns the old silent \
             null into a refused deopt"
        );
        assert_eq!(crate::deopt::count_materialization_required(fs), 1);
    }

    /// Round 11 wave 5 (lane lower): `o = new Foo(); guard; o.x = 7; o.y = 8`
    /// in one block. At the guard the allocation has run and neither store
    /// has, so the object is its zero-initialised self: a `VirtualObject` with
    /// zero fields, not a refusal. With `o.x = 7` moved BEFORE the guard the
    /// stores are mixed (one ran, one did not). Wave 6: with a complete
    /// `store_fields` that is `{x: 7, y: 0}`; without one it still refuses.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w5_a_store_after_the_deopt_in_the_allocating_block_describes_zero() {
        for (x_before_guard, with_store_fields) in [(false, false), (true, false), (true, true)] {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
            let newo = g.add(
                Op::New {
                    class_id: 7,
                    num_fields: 2,
                },
                IrType::Ref,
                vec![c0, mem],
                None,
            );
            let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
            let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
            let v7 = g.add(Op::Const(7), IrType::Int, vec![], None);
            let v8 = g.add(Op::Const(8), IrType::Int, vec![], None);
            let mut store_x = NO_NODE;
            if x_before_guard {
                store_x = g.add(
                    Op::Store(MemKind::Int),
                    IrType::Memory,
                    vec![c0, mem, newo, f0, v7],
                    None,
                );
            }
            let _guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![c0, cond], None);
            if !x_before_guard {
                store_x = g.add(
                    Op::Store(MemKind::Int),
                    IrType::Memory,
                    vec![c0, mem, newo, f0, v7],
                    None,
                );
            }
            let store_y = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![c0, mem, newo, f1, v8],
                None,
            );
            let ret = g.add(Op::Return, IrType::Void, vec![c0, v7], None);
            g.exit = ret;
            g.safepoints.push(SafepointSnapshot::new(
                10,
                vec![cond, newo],
                vec![],
                Vec::new(),
            ));
            for dead in [newo, store_x, store_y] {
                g.nodes[dead as usize].op = Op::Dead;
                g.nodes[dead as usize].inputs.clear();
            }
            let mut objects = HashMap::new();
            objects.insert(
                newo,
                VirtualObjectInfo {
                non_null_cond: None,
                    loop_phis: None,
                    array_element_type: None,
                    class_id: 7,
                    num_fields: 2,
                    field_values: vec![Some(v7), Some(v8)],
                    new_ctrl: c0,
                    store_ctrls: vec![c0, c0],
                    store_nodes: vec![store_x, store_y],
                    store_fields: if with_store_fields {
                        vec![Some((0, Some(v7))), Some((1, Some(v8)))]
                    } else {
                        Vec::new()
                    },
                },
            );
            let sr_map = ScalarReplacementMap { objects };
            let schedule = ir_schedule::schedule(&g);
            let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
                .expect("lower");
            let got = &deopt_locals_at(&cm, 10)[1];
            if x_before_guard && with_store_fields {
                match got {
                    FrameValue::VirtualObject(state) => {
                        assert_eq!(state.id, newo as usize);
                        assert_eq!(
                            state.field_values,
                            vec![FrameValue::Int(7), FrameValue::Int(0)],
                            "x's store ran at the guard and y's did not"
                        );
                    }
                    other => panic!("expected VirtualObject, got {other:?}"),
                }
            } else if x_before_guard {
                assert_eq!(
                    *got,
                    FrameValue::MaterializationRequired(EliminatedValue::allocation(
                        newo,
                        7,
                        EliminationCause::ScalarReplacedObject,
                    )),
                    "mixed run/pending stores without a store_fields list must still refuse"
                );
            } else {
                match got {
                    FrameValue::VirtualObject(state) => {
                        assert_eq!(state.id, newo as usize);
                        assert_eq!(
                            state.field_values,
                            vec![FrameValue::Int(0), FrameValue::Int(0)],
                            "no store has run at the guard: every field is its zero default"
                        );
                    }
                    other => panic!("expected VirtualObject, got {other:?}"),
                }
            }
        }
    }

    /// Round 11 wave 6 (lane lower): `o = new Foo(); o.x = 7; o.x = 9; guard;
    /// o.y = 8; o.x = 5` in one block. At the guard `x` holds its last RUN
    /// store's value, 9 -- not the earlier 7, and not the final 5 that
    /// `field_values` records -- and `y` is still its zero default. One entry
    /// of `store_fields` unknown makes the object refuse again.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w6_a_field_holds_its_last_run_store_at_a_mixed_deopt() {
        for complete in [true, false] {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
            let newo = g.add(
                Op::New {
                    class_id: 7,
                    num_fields: 2,
                },
                IrType::Ref,
                vec![c0, mem],
                None,
            );
            let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
            let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
            let v7 = g.add(Op::Const(7), IrType::Int, vec![], None);
            let v9 = g.add(Op::Const(9), IrType::Int, vec![], None);
            let v8 = g.add(Op::Const(8), IrType::Int, vec![], None);
            let v5 = g.add(Op::Const(5), IrType::Int, vec![], None);
            let store = |g: &mut Graph, field: NodeId, value: NodeId| {
                g.add(
                    Op::Store(MemKind::Int),
                    IrType::Memory,
                    vec![c0, mem, newo, field, value],
                    None,
                )
            };
            let sx1 = store(&mut g, f0, v7);
            let sx2 = store(&mut g, f0, v9);
            let _guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![c0, cond], None);
            let sy = store(&mut g, f1, v8);
            let sx3 = store(&mut g, f0, v5);
            let ret = g.add(Op::Return, IrType::Void, vec![c0, v5], None);
            g.exit = ret;
            g.safepoints.push(SafepointSnapshot::new(
                10,
                vec![cond, newo],
                vec![],
                Vec::new(),
            ));
            for dead in [newo, sx1, sx2, sy, sx3] {
                g.nodes[dead as usize].op = Op::Dead;
                g.nodes[dead as usize].inputs.clear();
            }
            let mut store_fields = vec![
                Some((0, Some(v7))),
                Some((0, Some(v9))),
                Some((1, Some(v8))),
                Some((0, Some(v5))),
            ];
            if !complete {
                // `o.x = 7`'s entry unknown.
                store_fields[0] = None;
            }
            let mut objects = HashMap::new();
            objects.insert(
                newo,
                VirtualObjectInfo {
                non_null_cond: None,
                    loop_phis: None,
                    array_element_type: None,
                    class_id: 7,
                    num_fields: 2,
                    field_values: vec![Some(v5), Some(v8)],
                    new_ctrl: c0,
                    store_ctrls: vec![c0, c0, c0, c0],
                    store_nodes: vec![sx1, sx2, sy, sx3],
                    store_fields,
                },
            );
            let sr_map = ScalarReplacementMap { objects };
            // Every store value is pinned, not only the final field values.
            let walked: Vec<NodeId> = scalar_replacement_field_values(&sr_map).collect();
            for v in [v5, v8, v9] {
                assert!(
                    walked.contains(&v),
                    "store value n{v} is not in the pin walk"
                );
            }
            assert_eq!(walked.contains(&v7), complete);
            let schedule = ir_schedule::schedule(&g);
            let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
                .expect("lower");
            let got = &deopt_locals_at(&cm, 10)[1];
            if complete {
                match got {
                    FrameValue::VirtualObject(state) => {
                        assert_eq!(state.id, newo as usize);
                        assert_eq!(
                            state.field_values,
                            vec![FrameValue::Int(9), FrameValue::Int(0)],
                            "x is its last run store's value and y has not been stored"
                        );
                    }
                    other => panic!("expected VirtualObject, got {other:?}"),
                }
            } else {
                assert_eq!(
                    *got,
                    FrameValue::MaterializationRequired(EliminatedValue::allocation(
                        newo,
                        7,
                        EliminationCause::ScalarReplacedObject,
                    )),
                    "an unknown store_fields entry must refuse the mixed object"
                );
            }
        }
    }

    /// Round 11 wave 6 (`r11w6-lower-sr-recipe-values-scheduled-after-the-deopt`):
    /// `o = new Foo(); o.x = cond + 1; guard` in one block, all stores run.
    /// The field's value `cond + 1` is read by nothing once the store is
    /// eliminated, so the scheduler may put it after the guard. The schedule is
    /// edited to place it on either side: before the guard the object is
    /// described, after it the object refuses, because its home word is not
    /// written when the deopt fires.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w6_a_recipe_value_computed_after_the_site_refuses_the_object() {
        for value_after_site in [false, true] {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
            let newo = g.add(
                Op::New {
                    class_id: 7,
                    num_fields: 1,
                },
                IrType::Ref,
                vec![c0, mem],
                None,
            );
            let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
            let one = g.add(Op::Const(1), IrType::Int, vec![], None);
            let t = g.add(Op::Add, IrType::Int, vec![cond, one], None);
            let store = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![c0, mem, newo, f0, t],
                None,
            );
            let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![c0, cond], None);
            let ret = g.add(Op::Return, IrType::Void, vec![c0, cond], None);
            g.exit = ret;
            g.safepoints.push(SafepointSnapshot::new(
                10,
                vec![cond, newo],
                vec![],
                Vec::new(),
            ));
            for dead in [newo, store] {
                g.nodes[dead as usize].op = Op::Dead;
                g.nodes[dead as usize].inputs.clear();
            }
            let mut objects = HashMap::new();
            objects.insert(
                newo,
                VirtualObjectInfo {
                non_null_cond: None,
                    loop_phis: None,
                    array_element_type: None,
                    class_id: 7,
                    num_fields: 1,
                    field_values: vec![Some(t)],
                    new_ctrl: c0,
                    store_ctrls: vec![c0],
                    store_nodes: vec![store],
                    store_fields: vec![Some((0, Some(t)))],
                },
            );
            let sr_map = ScalarReplacementMap { objects };
            let mut schedule = ir_schedule::schedule(&g);
            let db = schedule.node_to_block[guard as usize];
            schedule.node_to_block[t as usize] = db;
            for block in schedule.blocks.iter_mut() {
                block.nodes.retain(|&n| n != t);
            }
            let nodes = &mut schedule.blocks[db].nodes;
            let at = nodes
                .iter()
                .position(|&n| n == guard)
                .expect("the guard is a node of its block");
            nodes.insert(if value_after_site { at + 1 } else { at }, t);
            let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
                .expect("lower");
            let got = &deopt_locals_at(&cm, 10)[1];
            if value_after_site {
                assert_eq!(
                    *got,
                    FrameValue::MaterializationRequired(EliminatedValue::allocation(
                        newo,
                        7,
                        EliminationCause::ScalarReplacedObject,
                    )),
                    "a field value computed after the deopt site must refuse the object"
                );
            } else {
                match got {
                    FrameValue::VirtualObject(state) => assert!(
                        !matches!(
                            state.field_values[0],
                            FrameValue::MaterializationRequired(_)
                                | FrameValue::Undefined
                                | FrameValue::Unsupported
                        ),
                        "the field is described: {:?}",
                        state.field_values[0]
                    ),
                    other => panic!("expected VirtualObject, got {other:?}"),
                }
            }
        }
    }

    /// Round 11 wave 8 (`r11w7-mid-sr-store-after-deopt-outside-the-allocation-block`):
    /// `o = new Foo(); o.x = 1; if (cond) { guard; o.y = 8; return } return`.
    /// The allocation is in the entry block, which strictly dominates the
    /// guard's; at the guard `x`'s store ran and `y`'s did not, so with a
    /// complete `store_fields` the object is `{x: 1, y: 0}`. It used to refuse,
    /// because a store after the site was accepted only on an object allocated
    /// in the deopt block itself. Without `store_fields` it still refuses.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w8_a_store_after_the_deopt_below_the_allocating_block_is_pending() {
        use crate::ir::CmpOp;
        for with_store_fields in [true, false] {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
            let newo = g.add(
                Op::New {
                    class_id: 7,
                    num_fields: 2,
                },
                IrType::Ref,
                vec![c0, mem],
                None,
            );
            let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
            let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
            let v1 = g.add(Op::Const(1), IrType::Int, vec![], None);
            let v8 = g.add(Op::Const(8), IrType::Int, vec![], None);
            let store_x = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![c0, mem, newo, f0, v1],
                None,
            );
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cond, zero], None);
            let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let e = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            let _guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![t, cond], None);
            let store_y = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![t, mem, newo, f1, v8],
                None,
            );
            let ret1 = g.add(Op::Return, IrType::Void, vec![t, v1], None);
            let _ret2 = g.add(Op::Return, IrType::Void, vec![e, v1], None);
            g.exit = ret1;
            g.safepoints.push(SafepointSnapshot::new(
                10,
                vec![cond, newo],
                vec![],
                Vec::new(),
            ));
            for dead in [newo, store_x, store_y] {
                g.nodes[dead as usize].op = Op::Dead;
                g.nodes[dead as usize].inputs.clear();
            }
            let mut objects = HashMap::new();
            objects.insert(
                newo,
                VirtualObjectInfo {
                non_null_cond: None,
                    loop_phis: None,
                    array_element_type: None,
                    class_id: 7,
                    num_fields: 2,
                    field_values: vec![Some(v1), Some(v8)],
                    new_ctrl: c0,
                    store_ctrls: vec![c0, t],
                    store_nodes: vec![store_x, store_y],
                    store_fields: if with_store_fields {
                        vec![Some((0, Some(v1))), Some((1, Some(v8)))]
                    } else {
                        Vec::new()
                    },
                },
            );
            let sr_map = ScalarReplacementMap { objects };
            let schedule = ir_schedule::schedule(&g);
            assert_ne!(
                schedule.node_to_block[c0 as usize], schedule.node_to_block[t as usize],
                "the fixture allocates outside the deopt block"
            );
            let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
                .expect("lower");
            let got = &deopt_locals_at(&cm, 10)[1];
            if with_store_fields {
                match got {
                    FrameValue::VirtualObject(state) => {
                        assert_eq!(state.id, newo as usize);
                        assert_eq!(
                            state.field_values,
                            vec![FrameValue::Int(1), FrameValue::Int(0)],
                            "x's store ran above the guard and y's did not"
                        );
                    }
                    other => panic!("expected VirtualObject, got {other:?}"),
                }
            } else {
                assert_eq!(
                    *got,
                    FrameValue::MaterializationRequired(EliminatedValue::allocation(
                        newo,
                        7,
                        EliminationCause::ScalarReplacedObject,
                    )),
                    "mixed run/pending stores without a store_fields list must still refuse"
                );
            }
        }
    }


    /// Round 11 wave 8 (`r11w7-mid-sr-deopt-block-is-chosen-per-bci-not-per-snapshot`):
    /// two snapshots at bci 10, the original unnamed (index 0) and a copy
    /// (index 1) named by the only site there, a guard. The copy is described
    /// at the guard; the original, which no site publishes, is not described
    /// at the COPY's program point any more — it refuses the object, the
    /// answer for a table point nothing traps at.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w8_a_scalar_replaced_object_is_described_per_snapshot_not_per_bci() {
        use crate::ir::CmpOp;
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let newo = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![c0, mem],
            None,
        );
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let v7 = g.add(Op::Const(7), IrType::Int, vec![], None);
        let store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![c0, mem, newo, f0, v7],
            None,
        );
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cond, zero], None);
        let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let e = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![t, cond], Some(10));
        let ret1 = g.add(Op::Return, IrType::Void, vec![t, v7], None);
        let _ret2 = g.add(Op::Return, IrType::Void, vec![e, v7], None);
        g.exit = ret1;
        for _ in 0..2 {
            g.safepoints.push(SafepointSnapshot::new(
                10,
                vec![cond, newo],
                vec![],
                Vec::new(),
            ));
        }
        assert!(
            g.set_node_frame_snapshot(guard, 1),
            "the copy is at the guard's bci"
        );
        for dead in [newo, store] {
            g.nodes[dead as usize].op = Op::Dead;
            g.nodes[dead as usize].inputs.clear();
        }
        let mut objects = HashMap::new();
        objects.insert(
            newo,
            VirtualObjectInfo {
                non_null_cond: None,
                loop_phis: None,
                array_element_type: None,
                class_id: 7,
                num_fields: 1,
                field_values: vec![Some(v7)],
                new_ctrl: c0,
                store_ctrls: vec![c0],
                store_nodes: vec![store],
                store_fields: vec![Some((0, Some(v7)))],
            },
        );
        let sr_map: &'static ScalarReplacementMap =
            Box::leak(Box::new(ScalarReplacementMap { objects }));
        let schedule = ir_schedule::schedule(&g);
        let graph: &'static Graph = Box::leak(Box::new(g));
        let schedule: &'static Schedule = Box::leak(Box::new(schedule));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let mut lo = lowerer_over(graph, schedule, plan, 4096);
        lo.sr_map = Some(sr_map);
        let copy = lo.resolve_frame_state(1, &graph.safepoints[1]);
        match &copy.locals[1] {
            FrameValue::VirtualObject(state) => {
                assert_eq!(state.field_values, vec![FrameValue::Int(7)]);
            }
            other => panic!("the guard's own frame describes the object, got {other:?}"),
        }
        let original = lo.resolve_frame_state(0, &graph.safepoints[0]);
        assert_eq!(
            original.locals[1],
            FrameValue::MaterializationRequired(EliminatedValue::allocation(
                newo,
                7,
                EliminationCause::ScalarReplacedObject,
            )),
            "no site publishes the original, so it is not described at the copy's point"
        );
    }

    /// ea-loop-carried: `o = new Foo(); o.x = 0; while (cond) { guard; o.x =
    /// o.x + 1 }` with the load answered by the header φ. At the guard (before
    /// the body's store) and after the loop, `x` is that φ; without a φ at the
    /// header the two edges disagree (0 and `x + 1`) and the object refuses.
    #[test]
    pub(in crate::ir_lower::tests) fn ea_loop_carried_a_field_reaches_a_deopt_through_its_header_phi() {
        use crate::ir::CmpOp;
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let newo = g.add(
            Op::New {
                class_id: 7,
                num_fields: 2,
            },
            IrType::Ref,
            vec![c0, mem],
            None,
        );
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st0 = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![c0, mem, newo, f0, zero],
            None,
        );
        let region = g.add(Op::Region, IrType::Control, vec![c0, NO_NODE], None);
        let x = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], None);
        let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cond, zero], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
        let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![body, cond], None);
        let add = g.add(Op::Add, IrType::Int, vec![x, one], None);
        let st_b = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![body, st0, newo, f0, add],
            None,
        );
        g.nodes[region as usize].inputs[1] = body;
        g.nodes[x as usize].inputs[2] = add;
        let after = g.add(Op::Guard { bci: 20 }, IrType::Void, vec![exit, cond], None);
        let ret = g.add(Op::Return, IrType::Void, vec![exit, x], None);
        g.exit = ret;
        let schedule = ir_schedule::schedule(&g);
        let graph: &'static Graph = Box::leak(Box::new(g));
        let schedule: &'static Schedule = Box::leak(Box::new(schedule));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let lo = lowerer_over(graph, schedule, plan, 4096);
        let info = |loop_phis: Vec<(usize, NodeId)>| VirtualObjectInfo {
                non_null_cond: None,
            loop_phis: Some(loop_phis),
            array_element_type: None,
            class_id: 7,
            num_fields: 2,
            field_values: vec![Some(add), None],
            new_ctrl: c0,
            store_ctrls: vec![c0, body],
            store_nodes: vec![st0, st_b],
            store_fields: vec![Some((0, Some(zero))), Some((0, Some(add)))],
        };
        let in_body = schedule.node_to_block[body as usize];
        let in_exit = schedule.node_to_block[exit as usize];
        let with_phi = info(vec![(0, x)]);
        assert_eq!(
            lo.loop_carried_field_values(newo, &with_phi, &[(0, x)], in_body, Some((guard, guard))),
            Some(vec![Some(x), None]),
            "at the body's guard the field holds the header φ; field 1 was never stored"
        );
        assert_eq!(
            lo.loop_carried_field_values(newo, &with_phi, &[(0, x)], in_exit, Some((after, after))),
            Some(vec![Some(x), None]),
            "after the loop too"
        );
        // A φ for another field only: the header's edges bring 0 and x + 1.
        let bogus = [(1usize, x)];
        assert_eq!(
            lo.loop_carried_field_values(
                newo,
                &info(bogus.to_vec()),
                &bogus,
                in_body,
                Some((guard, guard))
            ),
            None,
            "no φ at the join: the predecessors disagree, refuse"
        );
        assert_eq!(
            lo.loop_carried_field_values(
                newo,
                &info(Vec::new()),
                &[],
                in_body,
                Some((guard, guard))
            ),
            None,
            "a recipe whose φs were never created refuses"
        );
    }

    /// Round 11 wave 8: the loop half of the rule above. `o = new Foo();
    /// while (cond) { guard; o.y = 8 }` — the allocation is outside the loop,
    /// so at the guard of the second iteration `o.y = 8` HAS run on this very
    /// object: the store must not be classified pending. The same store is
    /// pending when the allocation is the loop header's own (every trip
    /// allocates a new object), and `block_reaches_avoiding` is what tells the
    /// two apart.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w8_a_store_after_the_deopt_in_a_loop_around_it_is_not_pending() {
        use crate::ir::CmpOp;
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let newo = g.add(
            Op::New {
                class_id: 7,
                num_fields: 2,
            },
            IrType::Ref,
            vec![c0, mem],
            None,
        );
        let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let v8 = g.add(Op::Const(8), IrType::Int, vec![], None);
        let region = g.add(Op::Region, IrType::Control, vec![c0, NO_NODE], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cond, zero], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
        let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![body, cond], None);
        let store_y = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![body, mem, newo, f1, v8],
            None,
        );
        g.nodes[region as usize].inputs[1] = body;
        let ret = g.add(Op::Return, IrType::Void, vec![exit, v8], None);
        g.exit = ret;
        let schedule = ir_schedule::schedule(&g);
        let graph: &'static Graph = Box::leak(Box::new(g));
        let schedule: &'static Schedule = Box::leak(Box::new(schedule));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let lo = lowerer_over(graph, schedule, plan, 4096);
        let db = schedule.node_to_block[body as usize];
        let entry_block = schedule.node_to_block[c0 as usize];
        let header_block = schedule.node_to_block[region as usize];
        assert!(
            entry_block != db && schedule.dom.dominates(entry_block, db),
            "the fixture allocates in a block strictly dominating the deopt block"
        );
        assert!(
            !lo.eliminated_store_pending_after_alloc(
                body,
                Some(store_y),
                db,
                Some(guard),
                entry_block
            ),
            "the back edge carries the store to the next trip's guard on the same object"
        );
        assert!(
            lo.eliminated_store_pending_after_alloc(
                body,
                Some(store_y),
                db,
                Some(guard),
                header_block
            ),
            "with the allocation in the header every trip's object is new"
        );
        assert!(
            !lo.eliminated_store_pending_after_alloc(
                body,
                Some(guard),
                db,
                Some(guard),
                header_block
            ),
            "a node that is not after the last site is never pending"
        );
    }

    /// Round 11 wave 6 (lane lower; the `ir_lower` half of
    /// `r11w5-mid-named-frame-copies-collide-with-the-by-bci-snapshot`): two
    /// snapshots at bci 10, the original unnamed and a copy named by the only
    /// data node at that bci (LICM's pre-header frame has this shape). The
    /// named node anchors its copy through `snapshot_native` and no longer
    /// anchors the bci, so the unnamed original cannot land on the same offset
    /// and win `build_deopt_points`' dedup: the table's one point at bci 10 is
    /// the copy's frame.
    #[test]
    pub(in crate::ir_lower::tests) fn r11w6_a_node_named_frame_copy_is_the_deopt_point_at_its_bci() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p0 = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let p1 = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let c5 = g.add(Op::Const(5), IrType::Int, vec![], None);
        let c9 = g.add(Op::Const(9), IrType::Int, vec![], None);
        let sum = g.add(Op::Add, IrType::Int, vec![p0, p1], Some(10));
        // Two uses, so `sum` is lowered at its own position rather than
        // carried into a single consumer.
        let twice = g.add(Op::Add, IrType::Int, vec![sum, sum], None);
        let _guard = g.add(Op::Guard { bci: 20 }, IrType::Void, vec![c0, p0], None);
        let ret = g.add(Op::Return, IrType::Void, vec![c0, twice], None);
        g.exit = ret;
        for (bci, local1) in [(10, c5), (10, c9), (20, p1)] {
            g.safepoints.push(SafepointSnapshot::new(
                bci,
                vec![p0, local1],
                vec![],
                Vec::new(),
            ));
        }
        assert!(
            g.set_node_frame_snapshot(sum, 1),
            "the copy is at sum's own bci"
        );
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 2, 2, &no_helpers()).expect("lower");
        let at10: Vec<&DeoptimizationPoint> =
            cm.deopt_points.iter().filter(|p| p.bci == 10).collect();
        assert_eq!(at10.len(), 1, "one point at bci 10: {at10:?}");
        assert_eq!(
            at10[0].frame_state.locals.get(1),
            Some(&FrameValue::Int(9)),
            "the unnamed original (local 1 = 5) took the named copy's anchor"
        );
    }

    /// irlower proposal W8-1 (round 11 wave 9): the unnamed original at a bci
    /// gets no table point when the only deopt site there publishes a
    /// node-named copy — even when an unnamed non-site node still anchors the
    /// bci (which is what kept it alive past wave 6's anchor rule above).
    /// Before, bci 10 carried the original's frame (local 1 = 5), which no trap
    /// publishes, beside or instead of the guard's (local 1 = 9).
    #[test]
    pub(in crate::ir_lower::tests) fn r11w9_a_table_point_no_site_publishes_is_not_built_beside_one_that_is() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p0 = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let p1 = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let c5 = g.add(Op::Const(5), IrType::Int, vec![], None);
        let c9 = g.add(Op::Const(9), IrType::Int, vec![], None);
        // An unnamed, non-trapping node at bci 10: it anchors `bci_native`.
        let sum = g.add(Op::Add, IrType::Int, vec![p0, p1], Some(10));
        let twice = g.add(Op::Add, IrType::Int, vec![sum, sum], None);
        // The only deopt site at bci 10, naming the copy (snapshot 1).
        let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![c0, p0], Some(10));
        let ret = g.add(Op::Return, IrType::Void, vec![c0, twice], None);
        g.exit = ret;
        for local1 in [c5, c9] {
            g.safepoints.push(SafepointSnapshot::new(
                10,
                vec![p0, local1],
                vec![],
                Vec::new(),
            ));
        }
        assert!(
            g.set_node_frame_snapshot(guard, 1),
            "the guard is at bci 10"
        );
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 2, 2, &no_helpers()).expect("lower");
        let at10: Vec<&DeoptimizationPoint> =
            cm.deopt_points.iter().filter(|p| p.bci == 10).collect();
        assert_eq!(at10.len(), 1, "one table point at bci 10: {at10:?}");
        assert_eq!(
            at10[0].frame_state.locals.get(1),
            Some(&FrameValue::Int(9)),
            "the table point at bci 10 must be the frame the guard publishes"
        );
    }

    /// A field whose value is itself a scalar-replaced allocation is emitted as
    /// a NESTED `FrameValue::VirtualObject`, not bailed.
    ///
    /// This used to answer `MaterializationRequired(NestedVirtualObject)` -- "the
    /// recipe exists but v1 emits no nested graphs" -- and that restriction is
    /// what stopped a wrapper object and its storage array from both being
    /// deleted: the wrapper's recipe names the array, so the moment the array
    /// became replaceable the wrapper's own recipe was refused and the wrapper
    /// had to stay on the heap. The materializer has always walked nested field
    /// graphs (`collect_virtual_objects` recurses, `field_value_to_value`
    /// resolves a nested state to its shell); only the producer refused.
    #[test]
    pub(in crate::ir_lower::tests) fn scalar_deopt_emits_a_nested_virtual_object_for_a_virtual_field() {
        let (mut g, mut sr_map, newo) = build_sr_deopt_graph(false, false);
        // Make field 0's value itself a scalar-replaced object: add a second
        // dead `Op::New` and register it in the map, then point the first
        // object's only field at it.
        let inner = g.add(Op::Dead, IrType::Ref, vec![], None);
        sr_map.objects.insert(
            inner,
            VirtualObjectInfo {
                non_null_cond: None,
                loop_phis: None,
                array_element_type: None,
                class_id: 9,
                num_fields: 0,
                field_values: vec![],
                new_ctrl: 1, // Proj(0) — the entry control, dominates everything
                store_ctrls: vec![],
                store_nodes: vec![],
                store_fields: Vec::new(),
            },
        );
        sr_map
            .objects
            .get_mut(&newo)
            .expect("outer object")
            .field_values = vec![Some(inner)];

        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let outer = &deopt_locals_at(&cm, 10)[1];
        let FrameValue::VirtualObject(state) = outer else {
            panic!("the outer object must still be a VirtualObject, got {outer:?}");
        };
        assert_eq!(state.field_values.len(), 1);
        assert!(
            matches!(
                &state.field_values[0],
                FrameValue::VirtualObject(inner_state)
                    if inner_state.class_id == 9 && inner_state.num_fields == 0
            ),
            "field 0 must be the nested object's own recipe, got {:?}",
            state.field_values[0]
        );
        let fs = &cm
            ._deopt_point_boxes
            .iter()
            .find(|p| p.bci == 10)
            .expect("deopt box at bci 10")
            .frame_state;
        assert!(
            crate::deopt::frame_state_is_resumable(fs),
            "a nested graph is rebuildable, so the frame must stay resumable"
        );
    }

    /// ...and a nested object that cannot be described refuses the ENCLOSING
    /// one, with the nested cause. Fail-closed one level at a time: the
    /// recursion re-runs every gate on the inner object, so an inner allocation
    /// whose control does not strictly dominate the deopt block takes the outer
    /// object down with it rather than being silently replaced by a null.
    #[test]
    pub(in crate::ir_lower::tests) fn scalar_deopt_an_undescribable_nested_field_bails_the_enclosing_object() {
        let (mut g, mut sr_map, newo) = build_sr_deopt_graph(false, false);
        let inner = g.add(Op::Dead, IrType::Ref, vec![], None);
        sr_map.objects.insert(
            inner,
            VirtualObjectInfo {
                non_null_cond: None,
                loop_phis: None,
                array_element_type: None,
                class_id: 9,
                num_fields: 0,
                field_values: vec![],
                // NO_NODE control: unresolvable, so the inner object's own
                // dominance gate refuses it.
                new_ctrl: NO_NODE,
                store_ctrls: vec![],
                store_nodes: vec![],
                store_fields: Vec::new(),
            },
        );
        sr_map
            .objects
            .get_mut(&newo)
            .expect("outer object")
            .field_values = vec![Some(inner)];

        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        assert_eq!(
            deopt_locals_at(&cm, 10)[1],
            FrameValue::MaterializationRequired(EliminatedValue::allocation(
                newo,
                7,
                EliminationCause::NestedVirtualObject,
            )),
            "an undescribable nested field must bail the enclosing object with \
             NestedVirtualObject, not ScalarReplacedObject and not Undefined"
        );
    }

    /// A `NO_NODE` snapshot slot is the one case that is *genuinely* undefined —
    /// the local was never stored on any path reaching this bci, so the verifier
    /// guarantees the interpreter cannot read it and `Value::Int(0)` is correct.
    /// It must keep resolving to `Undefined`; widening the eliminated marker to
    /// cover it would make every method with an unwritten local unresumable.
    #[test]
    pub(in crate::ir_lower::tests) fn no_node_snapshot_slot_stays_undefined() {
        let (mut g, sr_map, _newo) = build_sr_deopt_graph(false, false);
        // locals were [cond, o]; append an unwritten slot.
        g.safepoints
            .iter_mut()
            .find(|s| s.bci == 10)
            .expect("snapshot at bci 10")
            .locals
            .push(NO_NODE);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[2],
            FrameValue::Undefined,
            "an unwritten local is genuinely undefined, not eliminated"
        );
        // And it does NOT block the resume — only the eliminated marker does.
        assert!(crate::deopt::frame_state_is_resumable(&FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 10,
            locals: vec![FrameValue::Undefined],
            stack: vec![],
            monitors: vec![],
            caller: None,
        }));
    }

    /// `frame_value_for` used to index `graph.nodes` directly, so a snapshot slot
    /// naming an id past the end of the arena panicked — on the compiler thread,
    /// which takes the VM down. It must bail to the unresumable marker instead.
    #[test]
    pub(in crate::ir_lower::tests) fn out_of_range_snapshot_node_id_bails_instead_of_panicking() {
        let (mut g, _sr_map, _newo) = build_sr_deopt_graph(false, false);
        let past_end = g.nodes.len() as NodeId + 7;
        g.safepoints
            .iter_mut()
            .find(|s| s.bci == 10)
            .expect("snapshot at bci 10")
            .locals
            .push(past_end);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 1, 3, &no_helpers()).expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[2],
            FrameValue::MaterializationRequired(EliminatedValue::unknown(
                EliminationCause::Unclassified,
            )),
            "an out-of-range node id must resolve to the unresumable marker"
        );
    }

    // ── speculation ids on IR-tier deopt points (NOTES-cha.md C2) ────

    /// Every deopt point this tier publishes names WHICH speculation it is,
    /// and names it with the value the CONSUMER computes.
    ///
    /// The consumer is `DespecRegistry::contains_speculation`, asked by the
    /// emitter gates in `lib.rs` / `jit_bridge.rs` before a guard goes down and
    /// fed by the VM's de-spec recording in `deopt_resume.rs`. Both derive the
    /// id from the `(bci, reason)` they recovered, so a producer that stamps
    /// anything else is invisible to the registry — which is exactly what
    /// `speculation_id: 0` at every IR producer meant: a wildcard, matching
    /// (and therefore withdrawing) every other speculation at its bci.
    ///
    /// Asserted over BOTH published lists, because this file has three
    /// producers feeding two of them: `_deopt_point_boxes` (the `Op::Guard` arm
    /// and `emit_deopt_unless`, whose pointers the stubs bake) and
    /// `deopt_points` (the safepoint resume points, keyed by native offset).
    #[test]
    pub(in crate::ir_lower::tests) fn an_ir_tier_deopt_point_carries_the_id_the_consumer_derives() {
        let (g, _sr_map, _newo) = build_sr_deopt_graph(false, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 1, 3, &no_helpers()).expect("lower");

        let published: Vec<&DeoptimizationPoint> = cm
            ._deopt_point_boxes
            .iter()
            .map(|b| &**b)
            .chain(cm.deopt_points.iter())
            .collect();
        assert!(
            !published.is_empty(),
            "the fixture carries an `Op::Guard`, so the lowering must have \
             published at least one deopt point — without one this test \
             asserts nothing"
        );
        for p in published {
            assert_ne!(
                p.speculation_id,
                crate::deopt::SPECULATION_ID_ANY,
                "an IR-tier point at bci {} ({:?}) still carries the wildcard; \
                 a de-spec recorded against it would withdraw every other \
                 speculation at that bytecode index",
                p.bci,
                p.reason
            );
            assert_eq!(
                p.speculation_id,
                crate::deopt::speculation_id(p.bci, p.reason),
                "the id on the point at bci {} ({:?}) is not the one the \
                 consumer re-derives from the same `(bci, reason)`",
                p.bci,
                p.reason
            );
        }
    }

    /// Two speculations at ONE bci are told apart, and withdrawing one leaves
    /// the other standing — the property M4 exists for, now reached through an
    /// id this tier actually produced rather than one a test made up.
    ///
    /// `osr_exit.rs` calls the two-points-at-one-bci shape ordinary, not
    /// exotic: a protected invoke publishes a `ReceiverTypeChanged` guard and a
    /// `PendingException` frame at the same bytecode index. Under the old
    /// `(method, bci)` key, four traps on the exceptional frame withdrew the
    /// receiver guard too — on evidence about a speculation that had never
    /// failed.
    #[test]
    pub(in crate::ir_lower::tests) fn two_speculations_at_one_ir_tier_bci_are_withdrawn_independently() {
        let (g, _sr_map, _newo) = build_sr_deopt_graph(false, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 1, 3, &no_helpers()).expect("lower");

        // The guard the fixture builds, as the lowering published it.
        let guard = cm
            ._deopt_point_boxes
            .iter()
            .find(|p| p.bci == 10)
            .expect("the `Op::Guard { bci: 10 }` publishes a boxed point");
        // A second speculation at the SAME bci. Its id is derived the way the
        // VM derives one from a reason it recovered at a trapping bci, so this
        // is the consumer's spelling meeting the producer's.
        let other = crate::deopt::speculation_id(guard.bci, DeoptReason::PendingException);
        assert_ne!(
            guard.reason,
            DeoptReason::PendingException,
            "the fixture's guard must not BE the second speculation, or the \
             two ids are one"
        );
        // FIRST, and load-bearing: the producer must have stamped a real id.
        // An earlier version of this test asserted only `guard.speculation_id
        // != other` and then stored `other`, and it PASSED with the producer
        // reverted to the wildcard -- `0 != other` is true, and
        // `contains_speculation` compares the STORED value against the
        // wildcard, never the queried one. Measured that way: the producer was
        // reverted and this test stayed green. The direction of the store
        // below is what makes the rest of it bite.
        assert_ne!(
            guard.speculation_id,
            crate::deopt::SPECULATION_ID_ANY,
            "the IR tier published a wildcard: a STORED `SPECULATION_ID_ANY` \
             withdraws every speculation at its bci, which is the coarse \
             behaviour M4 exists to end"
        );
        assert_ne!(
            guard.speculation_id, other,
            "two speculations at one bci must be distinguishable, or the \
             third key component buys nothing"
        );

        let registry = crate::deopt::DespecRegistry::new();
        let m = "IrLowerDespec$Protected.call:(I)I";
        // Store the GUARD's own id -- the IR tier's speculation is the one that
        // kept trapping -- and require the OTHER speculation at that bci to
        // survive. Stored this way round, a wildcard producer fails the second
        // assertion, because `SPECULATION_ID_ANY` in the registry answers true
        // for every query at that bci including `other`.
        registry.insert(m, guard.bci, guard.speculation_id);
        assert!(
            registry.contains_speculation(m, guard.bci, guard.speculation_id),
            "the speculation that kept trapping is withdrawn"
        );
        assert!(
            !registry.contains_speculation(m, guard.bci, other),
            "a DIFFERENT speculation at the same bci must survive -- at a \
             protected invoke, four traps on the exceptional frame must not \
             take the receiver guard with them"
        );
        // The coarse question — the one the loop-hoist gates and
        // `recommend_action_at_bci` ask — still answers yes at this bci.
        assert!(registry.contains(m, guard.bci));
    }

    /// No producer in this file may go back to writing the wildcard.
    ///
    /// A source scan rather than a lowering fixture, and deliberately: a FOURTH
    /// producer added later would not be covered by the two tests above, which
    /// can only see the points one fixture happens to publish. The test-module
    /// split is the same one
    /// `every_phi_home_publish_site_is_guarded_against_a_dropped_home` uses.
    #[test]
    pub(in crate::ir_lower::tests) fn no_ir_tier_producer_writes_an_unassigned_speculation_id() {
        let src = concat!(include_str!("../../ir_lower/mod.rs"), "\n", include_str!("../../ir_lower/virtual_object_info.rs"), "\n", include_str!("../../ir_lower/mir_plan_and_init.rs"), "\n", include_str!("../../ir_lower/vector_preloop.rs"), "\n", include_str!("../../ir_lower/value_phi_copies.rs"), "\n", include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("../../ir_lower/int_expr_bounds.rs"), "\n", include_str!("../../ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("../../ir_lower/getfield_legacy_read.rs"), "\n", include_str!("../../ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("../../ir_lower/entry_fold_node.rs"), "\n", include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("../../ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("../../ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("../../ir_lower/scoped_raw_tail.rs"), "\n", include_str!("../../ir_lower/frame_needs.rs"), "\n", include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("../../ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("../../ir_lower/promoted_pending_mark.rs"), "\n", include_str!("../../ir_lower/site_value_live_at.rs"), "\n", include_str!("../../ir_lower/plan_register_residency_with.rs"), "\n", include_str!("../../ir_lower/inner_attempt.rs"), "\n", include_str!("../../ir_lower/g1_aastore_screen.rs"), "\n", include_str!("../../ir_lower/scoped_session_confined_rows.rs"));
        let body = src
            // No brace inside this literal: source scanners that count braces
            // (single_bytecode_decoder_ratchet) would otherwise see this test
            // run to the end of the file.
            .split(
                "
mod tests ",
            )
            .next()
            .unwrap_or(src);
        assert!(
            body.contains("fn emit_deopt_unless"),
            "the scanned region no longer contains the producers — the test \
             module boundary moved"
        );
        let offenders: Vec<&str> = body
            .lines()
            .map(|l| l.trim())
            .filter(|l| {
                l.starts_with("speculation_id:")
                    && (l.contains(": 0") || l.contains("SPECULATION_ID_ANY"))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "an IR-tier producer writes an unassigned speculation id ({offenders:?}); \
             it knows its own `(bci, reason)`, so it must stamp \
             `crate::deopt::speculation_id(bci, reason)` — a stored wildcard \
             withdraws every other speculation at that bci"
        );
    }

    /// The install-time verifier must reject metadata whose reference slot the
    /// oop map does not cover — the disagreement that leaves a live reference
    /// invisible to a relocating collector. Built here directly (rather than
    /// through `lower_inner`) because this backend does not yet anchor its oop
    /// maps to native offsets, so the agreement lane has nothing to join on in a
    /// real compile; the wiring in `lower_inner` arms the moment it does.
    #[test]
    pub(in crate::ir_lower::tests) fn install_verifier_rejects_an_oop_map_deopt_map_disagreement() {
        let point = |locals: Vec<FrameValue>| DeoptimizationPoint {
            native_offset: 0x40,
            bci: 3,
            reason: DeoptReason::NullCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                class_id: 0,
                method_key: String::new(),
                bci: 3,
                locals,
                stack: vec![],
                monitors: vec![],
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        // The map covers [rbp-40]; deopt spells that word as StackSlotRef(-40).
        let verifier = DeoptVerifier::new().with_oop_map(0x40, OopCoverage::complete([40]));
        assert!(
            verifier
                .verify(std::slice::from_ref(&point(vec![
                    FrameValue::StackSlotRef(-40)
                ])))
                .is_ok(),
            "a covered reference slot must pass"
        );
        let err = verifier
            .verify(std::slice::from_ref(&point(vec![
                FrameValue::StackSlotRef(-48),
            ])))
            .expect_err("an uncovered reference slot must bail the install");
        assert!(
            err.to_string().contains("oop map"),
            "the bailout must say which agreement broke, got: {err}"
        );
    }

    /// A `VirtualObject` recipe may only name a removed node the emitter also
    /// registered as materializable. Naming a node the optimizer retired without
    /// a recipe means the metadata was built from a stale graph, and the artifact
    /// must not install.
    #[test]
    pub(in crate::ir_lower::tests) fn install_verifier_rejects_a_recipe_for_an_unmaterializable_removed_node() {
        let point = DeoptimizationPoint {
            native_offset: 0x10,
            bci: 1,
            reason: DeoptReason::DivByZero,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                class_id: 0,
                method_key: String::new(),
                bci: 1,
                locals: vec![FrameValue::VirtualObject(VirtualObjectState {
                    array_element_type: None,
                    id: 12,
                    class_id: 3,
                    num_fields: 1,
                    field_values: vec![FrameValue::Int(0)],
                })],
                stack: vec![],
                monitors: vec![],
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        assert!(
            DeoptVerifier::new()
                .with_removed_nodes([12u32])
                .with_materializable_nodes([12u32])
                .verify(std::slice::from_ref(&point))
                .is_ok(),
            "a removed node registered as materializable is exactly what a \
             VirtualObject slot is allowed to name"
        );
        assert!(
            DeoptVerifier::new()
                .with_removed_nodes([12u32])
                .verify(std::slice::from_ref(&point))
                .is_err(),
            "a removed node with no recipe must bail the install"
        );
    }

    /// `lower_inner` publishes the slot planner's peak-liveness figure to the
    /// per-compilation report. The number itself is `plan_slots`' business; what
    /// this pins is that the metric is no longer `NotMeasured` after a compile,
    /// which is what made it useless to a reader.
    #[test]
    pub(in crate::ir_lower::tests) fn peak_live_values_reaches_the_compilation_report() {
        let _guard = crate::metrics::METRICS_TEST_LOCK.lock();
        crate::metrics::set_enabled_for_test(true);

        // int f(int a, int b) { return a + b; } — two live params at the Add.
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let graph = builder.build(&code, 4).expect("build");
        let schedule = ir_schedule::schedule(&graph);
        let expected = plan_slots(&graph, &schedule, None).peak_live;

        let published = {
            let rec = crate::metrics::CompileRecorder::begin("IrLowerPeak", "f", "(II)I", true);
            lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lower");
            rec.snapshot().expect("in-flight snapshot")
        };
        crate::metrics::set_enabled_for_test(false);

        // `Measured::Value(n)` is distinguishable from `NotMeasured` at every
        // `n`, so this equality proves the wiring on its own — the planner's
        // arithmetic is pinned separately by
        // `non_overlapping_values_share_a_frame_slot`.
        assert_eq!(
            published.peak_live_values,
            crate::metrics::Measured::Value(expected as u32),
            "lower_inner must publish SlotPlan::peak_live"
        );
        assert!(published.to_json().contains("\"peak_live_values\":"));
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_scalar_deopt_shares_via_ref() {
        // The same object in two local slots: first occurrence defines the
        // VirtualObject, the second is a VirtualObjectRef to its id.
        let (g, sr_map, newo) = build_sr_deopt_graph(false, true);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert!(
            matches!(&locals[1], FrameValue::VirtualObject(s) if s.id == newo as usize),
            "first occurrence defines the object, got {:?}",
            locals[1]
        );
        assert_eq!(
            locals[2],
            FrameValue::VirtualObjectRef(newo as usize),
            "second occurrence is a ref to the same id"
        );
    }



    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_produces_code() {
        // int f(int x) { return x; }
        let code = [0x1a, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 2, 1, 1);
        assert!(compiled.is_some(), "Should produce compiled code");
        let method = compiled.unwrap();
        assert!(!method.entry_ptr().is_null(), "Code should not be empty");
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_constant_return() {
        // int f() { return 42; }
        // bipush 42; ireturn
        let code = [0x10, 42, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 3, 0, 0);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        // Execute the compiled code
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let result = unsafe { method.try_call(&[]).expect("test JIT call") };
        assert_eq!(result, 42, "Should return 42");
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_add_constants() {
        // int f() { return 3 + 4; }  → constant-folded to return 7
        let code = [0x06, 0x07, 0x60, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 0, 0);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let result = unsafe { method.try_call(&[]).expect("test JIT call") };
        assert_eq!(result, 7, "Should return 7 after constant folding");
    }

    /// Build `f(a, b) = <sop>(a, b)` (or `f(a) = <sop>(a)`) as a graph, lower
    /// it, and return the compiled body. Direct graph construction rather than
    /// bytecode, because reaching `Op::ScalarIntrinsic` from bytecode needs the
    /// invoke planner and a constant pool -- machinery that is tested where it
    /// lives (`ir::try_ir_scalar_intrinsic`), and that would make this a test
    /// of the planner rather than of the emitted instructions.
    pub(in crate::ir_lower::tests) fn compile_scalar_intrinsic(sop: ScalarOp) -> CompiledMethod {
        let n = sop.arity();
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = graph.add(Op::Param(0), sop.input_ty(0), vec![start], None);
        let mut inputs = vec![a];
        if n >= 2 {
            // `input_ty(1)`, not `input_ty(0)`: a rotate takes a `long` value
            // and an `int` distance, and typing the distance as a `long` here
            // would make this harness disagree with the builder.
            let b = graph.add(Op::Param(1), sop.input_ty(1), vec![start], None);
            inputs.push(b);
        }
        if n == 3 {
            // JIT round 19 wave 2 (lane irops2): `Math.fma`'s addend.
            let c = graph.add(Op::Param(2), sop.input_ty(2), vec![start], None);
            inputs.push(c);
        }
        let v = graph.add(Op::ScalarIntrinsic(sop), sop.result_type(), inputs, Some(0));
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, v], Some(1));
        let schedule = ir_schedule::schedule(&graph);
        lower(&graph, &schedule, n, n, &no_helpers()).expect("the intrinsic body must lower")
    }

    /// The six emitted sequences, EXECUTED against a synthetic receiver, in
    /// BOTH object layouts.
    ///
    /// # Why both layouts, every time
    ///
    /// The compact/legacy branch is per-OBJECT, not per-class, and a
    /// compact-only arm is an arm that almost never fires: `init_object_header`
    /// -- the TLAB fast path serving nearly every allocation -- writes a LEGACY
    /// header unconditionally, whatever layout the class has registered. That
    /// is not hypothetical; `the_gated_arm_emits_both_store_shapes` in this file
    /// records it measured at `inline=0` over 16,384,000 executions when only
    /// the compact shape was emitted.
    ///
    /// When the resolver hands back the SAME offset for both layouts (which it
    /// does when no `CompactLayout` is registered for the forged class id, the
    /// usual case in a unit-test process), the two arms address one word and the
    /// per-layout half of this test is vacuous. It is written to be correct
    /// either way and asserts the arithmetic regardless; the two-arm SELECTION
    /// is pinned by `only_the_read_modify_write_families_carry_a_lock_prefix`,
    /// which counts one access encoding per arm in the emitted image.
    #[test]
    pub(in crate::ir_lower::tests) fn the_unbox_sequences_execute_against_both_object_layouts() {
        use crate::ir::UnboxOp as U;
        // (family, starting field value, delta, expected result, expected field)
        let cases: &[(U, i64, i64, i64, i64)] = &[
            // The three loads leave the field alone.
            (
                U::LongValue,
                0x1234_5678_9ABC_DEF0u64 as i64,
                0,
                0x1234_5678_9ABC_DEF0u64 as i64,
                0x1234_5678_9ABC_DEF0u64 as i64,
            ),
            (U::LongValue, -1, 0, -1, -1),
            (U::AtomicLongGet, i64::MIN, 0, i64::MIN, i64::MIN),
            (U::AtomicLongGet, 42, 0, 42, 42),
            // `Integer.intValue` is 32-bit. The IR tier's `Int` convention
            // ZERO-extends the upper half, so a negative field reads back as
            // its unsigned 32-bit image in the full slot -- checked below by
            // comparing the low half, which is the half every consumer reads.
            (U::IntValue, -7, 0, -7, -7),
            (U::IntValue, 0x7FFF_FFFF, 0, 0x7FFF_FFFF, 0x7FFF_FFFF),
            (U::IntValue, 7, 0, 7, 7),
            (
                U::AtomicIntGet,
                i32::MIN as i64,
                0,
                i32::MIN as i64,
                i32::MIN as i64,
            ),
            (
                U::AtomicIntGet,
                i32::MAX as i64,
                0,
                i32::MAX as i64,
                i32::MAX as i64,
            ),
            (U::AtomicIntGetAndIncrement, 5, 0, 5, 6),
            (U::AtomicIntGetAndIncrement, -1, 0, -1, 0),
            (
                U::AtomicIntGetAndIncrement,
                i32::MAX as i64,
                0,
                i32::MAX as i64,
                i32::MIN as i64,
            ),
            // The three `LOCK XADD` forms. `*AndGet` returns the POST-add
            // value, `getAndAdd` the PRE-add one -- the single place the two
            // differ, and what a fixup emitted for the wrong family breaks.
            (U::AtomicIntIncrementAndGet, 5, 0, 6, 6),
            (U::AtomicIntIncrementAndGet, -1, 0, 0, 0),
            (U::AtomicIntDecrementAndGet, 5, 0, 4, 4),
            (U::AtomicIntDecrementAndGet, 0, 0, -1, -1),
            (
                U::AtomicIntIncrementAndGet,
                i32::MAX as i64,
                0,
                i32::MIN as i64,
                i32::MIN as i64,
            ),
            (U::AtomicLongGetAndAdd, 100, 5, 100, 105),
            (U::AtomicLongGetAndAdd, 100, -5, 100, 95),
            (U::AtomicLongGetAndAdd, 0, 1i64 << 40, 0, 1i64 << 40),
        ];

        for &(op, start, delta, want, want_field) in cases {
            for compact in [true, false] {
                let Some((cm, compact_off, legacy_off)) = compile_unbox(op) else {
                    // No layout for the forged class id: nothing to execute.
                    continue;
                };
                let off = if compact { compact_off } else { legacy_off };
                assert!(
                    off >= 0 && (off as usize) + 8 <= 64,
                    "the resolved offset {off} must leave 8 bytes inside the 64-byte \
                     test object -- the payload plus, for a narrow family, the \
                     poison word above it",
                );
                let mut obj = unbox_test_object(compact);
                // Addressed by BYTE offset, not by word index: a LEGACY
                // int-category payload sits at `cell + FIELD_CELL_PAYLOAD32_
                // OFFSET`, which is not 8-byte aligned, and a first cut of this
                // test that divided by 8 read the wrong half and got its own
                // poison back.
                //
                // The 4 bytes ABOVE a narrow field are poisoned, so a 64-bit
                // load emitted where a 32-bit one belongs returns the poison
                // rather than a plausible answer.
                const POISON: u32 = 0x0BAD_0BAD;
                let base = obj.as_mut_ptr() as *mut u8; // Cast: array base -> byte cursor
                                                        // SAFETY: `off` was asserted to leave 8 bytes inside the
                                                        // 64-byte object, and the writes are unaligned-safe.
                unsafe {
                    let at = base.add(off as usize); // Cast: resolved field offset
                    if op.is_wide() {
                        std::ptr::write_unaligned(at as *mut i64, start);
                    } else {
                        std::ptr::write_unaligned(at as *mut u32, start as u32);
                        std::ptr::write_unaligned(at.add(4) as *mut u32, POISON);
                    }
                }
                let addr = obj.as_mut_ptr() as i64; // Cast: receiver address

                let args: &[i64] = if op.arity() == 1 {
                    &[addr, delta]
                } else {
                    &[addr]
                };
                // SAFETY: the body takes the receiver (and for one family a
                // delta), builds and tears down its own frame, and calls
                // nothing. `obj` is a live 64-byte buffer whose header the
                // emitted guards accept, so no deopt edge is taken.
                let got = unsafe { cm.try_call(args) }.expect("the body runs");

                let layout = if compact { "compact" } else { "legacy" };
                let got = if op.is_wide() { got } else { got as i32 as i64 };
                assert_eq!(
                    got,
                    want,
                    "{} on a {layout} receiver (field={start:#x}, delta={delta})",
                    op.as_str(),
                );
                // SAFETY: same range, same reasoning as the writes above.
                let field = unsafe {
                    let at = base.add(off as usize); // Cast: resolved field offset
                    if op.is_wide() {
                        std::ptr::read_unaligned(at as *const i64)
                    } else {
                        assert_eq!(
                            std::ptr::read_unaligned(at.add(4) as *const u32),
                            POISON,
                            "{} wrote past the 4 bytes a 32-bit field owns",
                            op.as_str(),
                        );
                        i64::from(std::ptr::read_unaligned(at as *const u32) as i32)
                    }
                };
                assert_eq!(
                    field,
                    want_field,
                    "{} left the wrong value in the {layout} field",
                    op.as_str(),
                );
            }
        }
    }



    /// Both guards and the per-object layout branch are emitted, for every
    /// family.
    ///
    /// The class-id compare cannot be checked by executing a well-formed
    /// receiver: with it missing every assertion above still passes, and what
    /// breaks instead is a receiver of another class reading this one's slot 0.
    #[test]
    pub(in crate::ir_lower::tests) fn every_unbox_family_emits_its_guards_and_the_layout_branch() {
        use crate::ir::UnboxOp as U;
        for op in [
            U::LongValue,
            U::IntValue,
            U::AtomicLongGet,
            U::AtomicIntGet,
            U::AtomicIntGetAndIncrement,
            U::AtomicIntIncrementAndGet,
            U::AtomicIntDecrementAndGet,
            U::AtomicLongGetAndAdd,
        ] {
            let Some((cm, _, _)) = compile_unbox(op) else {
                continue;
            };
            // SAFETY: as above.
            let code =
                unsafe { std::slice::from_raw_parts(cm.entry_ptr() as *const u8, cm.code_len()) };
            assert!(
                code.windows(3).any(|w| w == [0x48, 0x85, 0xC0]),
                "{} must null-check its receiver (TEST RAX, RAX)",
                op.as_str(),
            );
            let mut cid = vec![0x81, 0x78, 0x00];
            cid.extend_from_slice(&UNBOX_TEST_CLASS_ID.to_le_bytes());
            assert!(
                code.windows(cid.len()).any(|w| w == cid.as_slice()),
                "{} must compare the receiver's header class id against the \
                 constant the planner resolved",
                op.as_str(),
            );
            let mut flags = vec![0xF6, 0x80];
            flags.extend_from_slice(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
            flags.push(cratonvm_types::GC_FLAG_COMPACT);
            assert!(
                code.windows(flags.len()).any(|w| w == flags.as_slice()),
                "{} must branch on the receiver's own COMPACT flag",
                op.as_str(),
            );
        }
    }

    /// The proven heap access keeps the element address (`LEA RDX, [RDX +
    /// R11]`) and loses the primitive-array screen (`MOVZX EAX, BYTE [RDX +
    /// tags]`), the payload bounds (`LEA R10, [R11 - 16]`) and the receiver's
    /// and base's null tests; an unfiled one keeps all of them.
    #[test]
    pub(in crate::ir_lower::tests) fn r23w8_a_proven_ffm_heap_access_emits_no_screen_and_no_bounds() {
        let full = r23w8_compile_scoped_heap_get(false).expect("lowers");
        let proven = r23w8_compile_scoped_heap_get(true).expect("lowers");
        let screen = [
            0x0F,
            0xB6,
            0x42,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: a small layout constant
        ];
        let payload_start = [0x4D, 0x8D, 0x53, 0xF0];
        let element = [0x4A, 0x8D, 0x14, 0x1A];
        assert!(contains_seq(full.code_bytes(), &screen), "unfiled: screened");
        assert!(contains_seq(full.code_bytes(), &payload_start), "unfiled: bounded");
        assert!(!contains_seq(proven.code_bytes(), &screen), "proven: no screen");
        assert!(!contains_seq(proven.code_bytes(), &payload_start), "proven: no bounds");
        assert!(contains_seq(proven.code_bytes(), &element));
        assert!(proven.code_bytes().len() < full.code_bytes().len());
    }

    /// The never-closing session needs no test at all: no `TEST RDX, RDX`
    /// (the proven access tests neither `sma` nor its base), while an unfiled
    /// session keeps its null test and deopt.
    #[test]
    pub(in crate::ir_lower::tests) fn r23w9_a_proven_never_closing_session_emits_no_session_check() {
        let test_rdx = [0x48, 0x85, 0xD2];
        let checked = r23w9_compile_scoped_get_with_session(false).expect("lowers");
        let proven = r23w9_compile_scoped_get_with_session(true).expect("lowers");
        assert!(contains_seq(checked.code_bytes(), &test_rdx), "unfiled: the session test");
        assert!(!contains_seq(proven.code_bytes(), &test_rdx), "filed: no session test");
        assert!(proven.code_bytes().len() < checked.code_bytes().len());
    }

    /// The cached access tests its entry (`TEST R8, R8`) and the slot's key
    /// (`CMP R9, [R8]`) ahead of the full path, which it keeps; the uncached
    /// one has neither.
    #[test]
    pub(in crate::ir_lower::tests) fn r24_b2_a_cached_native_access_compares_the_slot_key_before_its_full_path() {
        let test_r8 = [0x4D, 0x85, 0xC0];
        let cmp_key = [0x4D, 0x3B, 0x08];
        let full = r24_b2_compile_native_get(false).expect("lowers");
        let cached = r24_b2_compile_native_get(true).expect("lowers");
        assert!(!contains_seq(full.code_bytes(), &cmp_key), "uncached: no slot-key compare");
        assert!(contains_seq(cached.code_bytes(), &test_r8));
        assert!(contains_seq(cached.code_bytes(), &cmp_key));
        assert!(cached.code_bytes().len() > full.code_bytes().len(), "the full path is kept");
    }




    /// The resolver: the compact arm at the registered body offset (when the
    /// storage is exactly the read's kind and width), the legacy arm at the
    /// cell payload, and a mismatched kind refused.
    #[test]
    pub(in crate::ir_lower::tests) fn r23w4_ffm_element_read_offsets_follow_the_layout() {
        use crate::ir::UnboxOp as U;
        r23w4_ffm_read_layout();
        let cid = R23W4_FFM_READ_CLASS_ID;
        let compact_on = cratonvm_types::compact_ref_fields_enabled();
        let bool_offs = crate::ir::ffm_element_read_offsets(U::FieldBool(1), 1, cid);
        assert_eq!(
            bool_offs,
            Some((if compact_on { 16 } else { 36 }, 36)),
            "slot 1: compact body 16; legacy cell 16+16, Int payload +4",
        );
        let ref_offs = crate::ir::ffm_element_read_offsets(U::FieldRef(0), 0, cid);
        if cratonvm_types::FieldStorageKind::Reference.size_runtime() == 8 {
            assert_eq!(
                ref_offs,
                Some((if compact_on { 8 } else { 24 }, 24)),
                "slot 0: compact body 8; legacy cell 16, pointer payload +8",
            );
        } else if compact_on {
            assert_eq!(ref_offs, None, "a narrow oop is refused");
        }
        assert_eq!(crate::ir::ffm_element_ref_legacy_tag_offset(24), 16);
        if compact_on {
            // Kind mismatches: a reference slot read as boolean, and back.
            assert_eq!(crate::ir::ffm_element_read_offsets(U::FieldBool(0), 0, cid), None);
            assert_eq!(crate::ir::ffm_element_read_offsets(U::FieldRef(1), 1, cid), None);
        }
        assert_eq!(crate::ir::ffm_element_read_offsets(U::FieldBool(1), 1, 0), None);
        assert_eq!(U::FieldRef(0).result_type(), IrType::Ref);
        assert_eq!(U::FieldBool(0).result_type(), IrType::Int);
        assert!(U::FieldRef(3).is_load() && U::FieldBool(3).is_load());
        assert_eq!(U::FieldRef(3).field_slot(), 3);
        assert_eq!(U::FieldRef(3).arity(), 0);
    }



    /// The emitted sequences, EXECUTED, against the answers the JLS specifies.
    ///
    /// The edges are the point. `Math.abs(Integer.MIN_VALUE)` is
    /// `Integer.MIN_VALUE` (JLS 15.15.4) because the negation wraps, and the
    /// branchless `(x ^ (x>>31)) - (x>>31)` reproduces that exactly -- a reader
    /// who "fixes" it to saturate would be introducing a bug, not removing one.
    /// `Math.min`/`max` are SIGNED, so a pair straddling zero separates them
    /// from an unsigned compare, which is the mistake a `CMOVA`/`CMOVB` typo
    /// would make and which no same-sign test could catch.
    #[test]
    pub(in crate::ir_lower::tests) fn the_scalar_intrinsic_sequences_execute_to_the_specified_answers() {
        let cases_i: &[(ScalarOp, i64, i64, i64)] = &[
            (ScalarOp::MinI, 3, 7, 3),
            (ScalarOp::MinI, 7, 3, 3),
            (ScalarOp::MinI, -5, 5, -5),
            (ScalarOp::MinI, 5, -5, -5),
            (
                ScalarOp::MinI,
                i32::MIN as i64,
                i32::MAX as i64,
                i32::MIN as i64,
            ),
            (ScalarOp::MaxI, 3, 7, 7),
            (ScalarOp::MaxI, 7, 3, 7),
            (ScalarOp::MaxI, -5, 5, 5),
            (ScalarOp::MaxI, 5, -5, 5),
            (
                ScalarOp::MaxI,
                i32::MIN as i64,
                i32::MAX as i64,
                i32::MAX as i64,
            ),
            (ScalarOp::CompareI, 3, 7, -1),
            (ScalarOp::CompareI, 7, 3, 1),
            (ScalarOp::CompareI, 7, 7, 0),
            // The pair that separates a SIGNED compare from an unsigned one:
            // as unsigned 32-bit values, -1 is the largest there is.
            (ScalarOp::CompareI, -1, 1, -1),
            (ScalarOp::CompareI, i32::MIN as i64, i32::MAX as i64, -1),
        ];
        for &(sop, a, b, want) in cases_i {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: the body takes two integer arguments, builds and tears
            // down its own frame, and calls nothing.
            let got = unsafe { cm.try_call(&[a, b]) }.expect("the body runs");
            // The `Int` slot convention zero-extends, so compare the low half.
            assert_eq!(got as i32, want as i32, "{}({a}, {b})", sop.as_str(),);
        }

        let cases_l: &[(ScalarOp, i64, i64, i64)] = &[
            (ScalarOp::MinL, 3, 7, 3),
            (ScalarOp::MinL, -5, 5, -5),
            (ScalarOp::MinL, i64::MIN, i64::MAX, i64::MIN),
            (ScalarOp::MaxL, 3, 7, 7),
            (ScalarOp::MaxL, 5, -5, 5),
            (ScalarOp::MaxL, i64::MIN, i64::MAX, i64::MAX),
        ];
        for &(sop, a, b, want) in cases_l {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: as above.
            let got = unsafe { cm.try_call(&[a, b]) }.expect("the body runs");
            assert_eq!(got, want, "{}({a}, {b})", sop.as_str());
        }

        // `Long.compare` returns an INT from LONG operands -- the case that
        // catches sizing the CMP from the result type, which would compare the
        // low halves and answer 0 for these.
        for &(a, b, want) in &[
            (1i64 << 40, 1i64, 1i64),
            (1i64, 1i64 << 40, -1),
            (i64::MIN, i64::MAX, -1),
            (0x1_0000_0000i64, 0i64, 1),
        ] {
            let cm = compile_scalar_intrinsic(ScalarOp::CompareL);
            // SAFETY: as above.
            let got = unsafe { cm.try_call(&[a, b]) }.expect("the body runs");
            assert_eq!(got as i32, want as i32, "Long.compare({a}, {b})");
        }

        for &(sop, x, want) in &[
            (ScalarOp::AbsI, 5i64, 5i64),
            (ScalarOp::AbsI, -5, 5),
            (ScalarOp::AbsI, 0, 0),
            // JLS 15.15.4: abs(MIN_VALUE) == MIN_VALUE.
            (ScalarOp::AbsI, i32::MIN as i64, i32::MIN as i64),
            (ScalarOp::AbsI, i32::MAX as i64, i32::MAX as i64),
        ] {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: one integer argument; as above.
            let got = unsafe { cm.try_call(&[x, 0]) }.expect("the body runs");
            assert_eq!(got as i32, want as i32, "Math.abs({x})");
        }

        for &(x, want) in &[
            (5i64, 5i64),
            (-5, 5),
            (0, 0),
            (i64::MIN, i64::MIN),
            (i64::MAX, i64::MAX),
        ] {
            let cm = compile_scalar_intrinsic(ScalarOp::AbsL);
            // SAFETY: as above.
            let got = unsafe { cm.try_call(&[x, 0]) }.expect("the body runs");
            assert_eq!(got, want, "Math.abs({x}L)");
        }
    }

    /// JIT round 19 (lane irops19, R15B-31): `Math.sqrt`, FP `Math.min`/`max`
    /// and `Math.multiplyHigh`/`unsignedMultiplyHigh`, EXECUTED, against a
    /// reference written from the JDK 25 bodies.
    ///
    /// The edges are the point: `MINSD`/`MAXSD` alone return the SECOND
    /// operand for NaN and for two zeros, while Java returns the NaN ARGUMENT
    /// (`a` first, payload kept) and orders -0.0 below +0.0. Every pair below
    /// is run both ways round and compared BIT FOR BIT.
    #[test]
    pub(in crate::ir_lower::tests) fn r19_fp_math_scalar_sequences_match_the_jdk_bodies() {
        // `java.lang.Math.min(double,double)` as JDK 25 writes it.
        fn jmin(a: f64, b: f64) -> f64 {
            if a.is_nan() {
                return a;
            }
            if a == 0.0 && b == 0.0 && b.to_bits() == (-0.0f64).to_bits() {
                return b;
            }
            if a <= b {
                a
            } else {
                b
            }
        }
        fn jmax(a: f64, b: f64) -> f64 {
            if a.is_nan() {
                return a;
            }
            if a == 0.0 && b == 0.0 && a.to_bits() == (-0.0f64).to_bits() {
                return b;
            }
            if a >= b {
                a
            } else {
                b
            }
        }
        fn jminf(a: f32, b: f32) -> f32 {
            if a.is_nan() {
                return a;
            }
            if a == 0.0 && b == 0.0 && b.to_bits() == (-0.0f32).to_bits() {
                return b;
            }
            if a <= b {
                a
            } else {
                b
            }
        }
        fn jmaxf(a: f32, b: f32) -> f32 {
            if a.is_nan() {
                return a;
            }
            if a == 0.0 && b == 0.0 && a.to_bits() == (-0.0f32).to_bits() {
                return b;
            }
            if a >= b {
                a
            } else {
                b
            }
        }
        // Two distinguishable quiet NaNs, so "which NaN came back" is visible.
        let nan1 = f64::from_bits(0x7FF8_0000_0000_0001);
        let nan2 = f64::from_bits(0xFFF8_0000_0000_0BAD);
        let ds = [
            0.0f64,
            -0.0,
            1.0,
            -1.0,
            2.5,
            -2.5,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::MIN,
            5e-324,
            nan1,
            nan2,
        ];
        let min_d = compile_scalar_intrinsic(ScalarOp::MinD);
        let max_d = compile_scalar_intrinsic(ScalarOp::MaxD);
        for &a in &ds {
            for &b in &ds {
                let args = [a.to_bits() as i64, b.to_bits() as i64];
                // SAFETY: two FP arguments as raw bits in integer slots, the
                // all-GPR entry ABI; the body calls nothing.
                let got = unsafe { min_d.try_call(&args) }.expect("runs") as u64;
                assert_eq!(got, jmin(a, b).to_bits(), "Math.min({a:?}, {b:?})");
                // SAFETY: as above.
                let got = unsafe { max_d.try_call(&args) }.expect("runs") as u64;
                assert_eq!(got, jmax(a, b).to_bits(), "Math.max({a:?}, {b:?})");
            }
        }

        let fnan1 = f32::from_bits(0x7FC0_0001);
        let fnan2 = f32::from_bits(0xFFC0_0BAD);
        let fs = [
            0.0f32,
            -0.0,
            1.0,
            -1.0,
            3.25,
            -3.25,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE,
            f32::MAX,
            f32::MIN,
            1e-45,
            fnan1,
            fnan2,
        ];
        let min_f = compile_scalar_intrinsic(ScalarOp::MinF);
        let max_f = compile_scalar_intrinsic(ScalarOp::MaxF);
        for &a in &fs {
            for &b in &fs {
                // A float slot is its 32-bit pattern, zero-extended.
                let args = [i64::from(a.to_bits()), i64::from(b.to_bits())];
                // SAFETY: as above.
                let got = unsafe { min_f.try_call(&args) }.expect("runs") as u32;
                assert_eq!(got, jminf(a, b).to_bits(), "Math.min({a:?}f, {b:?}f)");
                // SAFETY: as above.
                let got = unsafe { max_f.try_call(&args) }.expect("runs") as u32;
                assert_eq!(got, jmaxf(a, b).to_bits(), "Math.max({a:?}f, {b:?}f)");
            }
        }

        // `Math.sqrt`: IEEE 754 correctly rounded, which Rust's `sqrt` is too.
        // sqrt(-0.0) is -0.0, sqrt of a negative is NaN, sqrt(+inf) is +inf.
        let sqrt = compile_scalar_intrinsic(ScalarOp::SqrtD);
        for &x in &[
            0.0f64,
            -0.0,
            1.0,
            2.0,
            4.0,
            1e-310,
            f64::MAX,
            f64::INFINITY,
            -1.0,
            f64::NEG_INFINITY,
        ] {
            // SAFETY: one FP argument; as above.
            let got = unsafe { sqrt.try_call(&[x.to_bits() as i64, 0]) }.expect("runs") as u64;
            let want = x.sqrt();
            if want.is_nan() {
                assert!(f64::from_bits(got).is_nan(), "Math.sqrt({x:?}) must be NaN");
            } else {
                assert_eq!(got, want.to_bits(), "Math.sqrt({x:?})");
            }
        }
        // A NaN operand propagates (SQRTSD quiets and keeps the payload).
        // SAFETY: as above.
        let got = unsafe { sqrt.try_call(&[nan1.to_bits() as i64, 0]) }.expect("runs") as u64;
        assert!(f64::from_bits(got).is_nan(), "Math.sqrt(NaN)");

        // `Math.multiplyHigh` / `unsignedMultiplyHigh`: the high 64 bits of
        // the 128-bit product, against Rust's own 128-bit arithmetic.
        let mh = compile_scalar_intrinsic(ScalarOp::MultiplyHighL);
        let umh = compile_scalar_intrinsic(ScalarOp::UnsignedMultiplyHighL);
        let ls = [
            0i64,
            1,
            -1,
            2,
            -2,
            i64::MAX,
            i64::MIN,
            0x1234_5678_9ABC_DEF0,
            -0x1234_5678_9ABC_DEF0,
            1i64 << 32,
            (1i64 << 62) - 7,
        ];
        for &a in &ls {
            for &b in &ls {
                let want = ((i128::from(a) * i128::from(b)) >> 64) as i64;
                // SAFETY: two integer arguments; as above.
                let got = unsafe { mh.try_call(&[a, b]) }.expect("runs");
                assert_eq!(got, want, "Math.multiplyHigh({a}, {b})");
                let uwant = ((u128::from(a as u64) * u128::from(b as u64)) >> 64) as i64;
                // SAFETY: as above.
                let got = unsafe { umh.try_call(&[a, b]) }.expect("runs");
                assert_eq!(got, uwant, "Math.unsignedMultiplyHigh({a}, {b})");
            }
        }
    }


    /// The bit-scan sequences, EXECUTED, against the answers `java.lang.Integer`
    /// and `java.lang.Long` specify.
    ///
    /// ZERO is the case every one of these has and the one a reader is most
    /// likely to break: `BSR`/`BSF` leave their destination UNDEFINED on a zero
    /// source, so a sequence that reads the destination without first
    /// consulting ZF returns whatever the register happened to hold. Each
    /// family is therefore tested at zero, and `highestOneBit` — whose shift
    /// runs on that undefined count and is masked away afterwards — is tested
    /// at zero on both widths.
    ///
    /// The other trap is width. `Long.numberOfLeadingZeros` takes a `long` and
    /// returns an `int`, so a sequence that sized its scan from the RESULT type
    /// would scan the low half and answer 32 for every value above 2^32; the
    /// `1 << 40` cases catch exactly that.
    #[test]
    pub(in crate::ir_lower::tests) fn the_bit_scan_sequences_execute_to_the_specified_answers() {
        // (op, input, expected) — 32-bit families, compared on the low half.
        let unary_i: &[(ScalarOp, i64, i64)] = &[
            (ScalarOp::NlzI, 0, 32),
            (ScalarOp::NlzI, 1, 31),
            (ScalarOp::NlzI, -1, 0),
            (ScalarOp::NlzI, i32::MIN as i64, 0),
            (ScalarOp::NlzI, 0x0000_FFFF, 16),
            (ScalarOp::NtzI, 0, 32),
            (ScalarOp::NtzI, 1, 0),
            (ScalarOp::NtzI, -1, 0),
            (ScalarOp::NtzI, i32::MIN as i64, 31),
            (ScalarOp::NtzI, 0x0001_0000, 16),
            (ScalarOp::ReverseBytesI, 0x0102_0304, 0x0403_0201),
            (ScalarOp::ReverseBytesI, 0, 0),
            (ScalarOp::LowestOneBitI, 0, 0),
            (ScalarOp::LowestOneBitI, 0x0000_00FF, 1),
            (ScalarOp::LowestOneBitI, i32::MIN as i64, i32::MIN as i64),
            (ScalarOp::HighestOneBitI, 0, 0),
            (ScalarOp::HighestOneBitI, 0x0000_00FF, 0x80),
            (ScalarOp::HighestOneBitI, -1, i32::MIN as i64),
            (ScalarOp::HighestOneBitI, 1, 1),
            // nio20 wave 6, lane L3: the surrogate range tests, at both ends of
            // each range, just outside them, and for operands the bytecode's
            // signed compares also answer `false` (negative, above 0xFFFF).
            (ScalarOp::IsHighSurrogate, 0xD7FF, 0),
            (ScalarOp::IsHighSurrogate, 0xD800, 1),
            (ScalarOp::IsHighSurrogate, 0xDBFF, 1),
            (ScalarOp::IsHighSurrogate, 0xDC00, 0),
            (ScalarOp::IsHighSurrogate, 0x41, 0),
            (ScalarOp::IsHighSurrogate, -1, 0),
            (ScalarOp::IsHighSurrogate, 0x1_D800, 0),
            (ScalarOp::IsLowSurrogate, 0xDBFF, 0),
            (ScalarOp::IsLowSurrogate, 0xDC00, 1),
            (ScalarOp::IsLowSurrogate, 0xDFFF, 1),
            (ScalarOp::IsLowSurrogate, 0xE000, 0),
            (ScalarOp::IsLowSurrogate, 0, 0),
            (ScalarOp::IsLowSurrogate, i32::MIN as i64, 0),
            // JIT round 20 wave 11, lane pIR: the code-point tests at each
            // boundary, negative and huge operands included.
            (ScalarOp::CharCount, 0, 1),
            (ScalarOp::CharCount, 0xFFFF, 1),
            (ScalarOp::CharCount, 0x1_0000, 2),
            (ScalarOp::CharCount, 0x10_FFFF, 2),
            (ScalarOp::CharCount, 0x11_0000, 2),
            (ScalarOp::CharCount, -1, 1),
            (ScalarOp::CharCount, i32::MIN as i64, 1),
            (ScalarOp::CharCount, i32::MAX as i64, 2),
            (ScalarOp::IsBmpCodePoint, 0, 1),
            (ScalarOp::IsBmpCodePoint, 0xFFFF, 1),
            (ScalarOp::IsBmpCodePoint, 0x1_0000, 0),
            (ScalarOp::IsBmpCodePoint, -1, 0),
            (ScalarOp::IsBmpCodePoint, i32::MIN as i64, 0),
            (ScalarOp::IsValidCodePoint, 0, 1),
            (ScalarOp::IsValidCodePoint, 0x10_FFFF, 1),
            (ScalarOp::IsValidCodePoint, 0x11_0000, 0),
            (ScalarOp::IsValidCodePoint, -1, 0),
            (ScalarOp::IsValidCodePoint, i32::MIN as i64, 0),
            (ScalarOp::IsValidCodePoint, i32::MAX as i64, 0),
        ];
        for &(sop, x, want) in unary_i {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: one integer argument; the body builds and tears down its
            // own frame and calls nothing.
            let got = unsafe { cm.try_call(&[x, 0]) }.expect("the body runs");
            assert_eq!(got as i32, want as i32, "{}({x})", sop.as_str());
        }

        // 64-bit input families whose ANSWER is an `int`.
        let unary_l_to_i: &[(ScalarOp, i64, i64)] = &[
            (ScalarOp::NlzL, 0, 64),
            (ScalarOp::NlzL, 1, 63),
            (ScalarOp::NlzL, -1, 0),
            (ScalarOp::NlzL, i64::MIN, 0),
            // The case that separates a 64-bit scan from a 32-bit one.
            (ScalarOp::NlzL, 1i64 << 40, 23),
            (ScalarOp::NtzL, 0, 64),
            (ScalarOp::NtzL, 1, 0),
            (ScalarOp::NtzL, i64::MIN, 63),
            (ScalarOp::NtzL, 1i64 << 40, 40),
        ];
        for &(sop, x, want) in unary_l_to_i {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: as above.
            let got = unsafe { cm.try_call(&[x, 0]) }.expect("the body runs");
            assert_eq!(got as i32, want as i32, "{}({x})", sop.as_str());
        }

        // 64-bit input AND answer.
        let unary_l: &[(ScalarOp, i64, i64)] = &[
            (
                ScalarOp::ReverseBytesL,
                0x0102_0304_0506_0708,
                0x0807_0605_0403_0201,
            ),
            (ScalarOp::ReverseBytesL, 0, 0),
            (ScalarOp::LowestOneBitL, 0, 0),
            (ScalarOp::LowestOneBitL, 0x00FF_0000_0000_0000, 1i64 << 48),
            (ScalarOp::LowestOneBitL, i64::MIN, i64::MIN),
            (ScalarOp::HighestOneBitL, 0, 0),
            (ScalarOp::HighestOneBitL, 0x0000_00FF, 0x80),
            (ScalarOp::HighestOneBitL, -1, i64::MIN),
            (ScalarOp::HighestOneBitL, 1i64 << 40, 1i64 << 40),
        ];
        for &(sop, x, want) in unary_l {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: as above.
            let got = unsafe { cm.try_call(&[x, 0]) }.expect("the body runs");
            assert_eq!(got, want, "{}({x})", sop.as_str());
        }

        // Rotates. A NEGATIVE distance is the case the JLS defines by masking
        // and x86 implements by masking, so it is the one that proves the two
        // agree rather than merely both being plausible.
        let rot_i: &[(ScalarOp, i64, i64, i64)] = &[
            (ScalarOp::RotateLeftI, 1, 1, 2),
            (ScalarOp::RotateLeftI, i32::MIN as i64, 1, 1),
            (ScalarOp::RotateLeftI, 1, 32, 1),
            (ScalarOp::RotateLeftI, 1, -1, i32::MIN as i64),
            (ScalarOp::RotateRightI, 1, 1, i32::MIN as i64),
            (ScalarOp::RotateRightI, 1, -1, 2),
            (
                ScalarOp::RotateRightI,
                0x0000_00FF,
                4,
                0xF000_000F_u32 as i32 as i64,
            ),
        ];
        for &(sop, x, d, want) in rot_i {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: two integer arguments; as above.
            let got = unsafe { cm.try_call(&[x, d]) }.expect("the body runs");
            assert_eq!(got as i32, want as i32, "{}({x}, {d})", sop.as_str());
        }

        let rot_l: &[(ScalarOp, i64, i64, i64)] = &[
            (ScalarOp::RotateLeftL, 1, 1, 2),
            (ScalarOp::RotateLeftL, i64::MIN, 1, 1),
            (ScalarOp::RotateLeftL, 1, 64, 1),
            (ScalarOp::RotateLeftL, 1, -1, i64::MIN),
            (ScalarOp::RotateRightL, 1, 1, i64::MIN),
            (ScalarOp::RotateRightL, 1, -1, 2),
            // Distances 32 and 40 separate a 64-bit rotate from a 32-bit one.
            (ScalarOp::RotateLeftL, 1, 40, 1i64 << 40),
            (ScalarOp::RotateLeftL, 1, 32, 1i64 << 32),
        ];
        for &(sop, x, d, want) in rot_l {
            let cm = compile_scalar_intrinsic(sop);
            // SAFETY: as above.
            let got = unsafe { cm.try_call(&[x, d]) }.expect("the body runs");
            assert_eq!(got, want, "{}({x}, {d})", sop.as_str());
        }
    }

    /// Round 15 wave 6 (lane irosr2, IRO-1): the four raw FP-bits families,
    /// EXECUTED. Each is a bit-for-bit identity across the two register files,
    /// so every case expects its own input back: NaNs with payloads (quiet and
    /// SIGNALLING -- an arithmetic move or a `CVT` would quiet the second),
    /// both zeros, a subnormal, an infinity and an ordinary value. A bank
    /// mix-up (the operand read from the wrong file) or a canonicalising
    /// sequence fails here, which is the whole risk of the change.
    ///
    /// The ABI the harness relies on: a `double`/`float` argument arrives as
    /// its raw bits in an integer register (`Op::Param`), and a result leaves
    /// through RAX from its home word (`Op::Return`'s `gp_load_value`). A
    /// 32-bit result is compared on its low half.
    #[test]
    pub(in crate::ir_lower::tests) fn r15w6_irosr2_the_raw_fp_bits_moves_keep_every_bit() {
        let doubles: [u64; 9] = [
            0x7FF8_0000_0000_0000, // the canonical quiet NaN
            0x7FF8_DEAD_BEEF_0001, // a quiet NaN with a payload
            0x7FF0_0000_0000_0001, // a SIGNALLING NaN
            0xFFF4_0000_0000_00FF, // a negative signalling NaN
            0x8000_0000_0000_0000, // -0.0
            0x0000_0000_0000_0000, // +0.0
            0x0000_0000_0000_0001, // the smallest subnormal
            0xFFF0_0000_0000_0000, // -Infinity
            0x3FF8_0000_0000_0000, // 1.5
        ];
        for sop in [ScalarOp::DoubleToRawLongBits, ScalarOp::LongBitsToDouble] {
            let cm = compile_scalar_intrinsic(sop);
            for &bits in &doubles {
                // Cast: a raw 64-bit pattern into the integer argument slot.
                let arg = bits as i64;
                // SAFETY: one argument in an integer register, as `Op::Param`
                // expects for either operand type; the body calls nothing.
                let got = unsafe { cm.try_call(&[arg]) }.expect("the body runs");
                // Cast: compared as the raw pattern.
                assert_eq!(got as u64, bits, "{}({bits:#018x})", sop.as_str());
            }
        }
        let floats: [u32; 8] = [
            0x7FC0_0000, // the canonical quiet NaN
            0x7FC0_1234, // a quiet NaN with a payload
            0x7F80_0001, // a SIGNALLING NaN
            0xFFA0_0001, // a negative signalling NaN
            0x8000_0000, // -0.0f
            0x0000_0001, // the smallest subnormal
            0x7F80_0000, // +Infinity
            0x3FC0_0000, // 1.5f
        ];
        for sop in [ScalarOp::FloatToRawIntBits, ScalarOp::IntBitsToFloat] {
            let cm = compile_scalar_intrinsic(sop);
            for &bits in &floats {
                // Cast: the 32-bit pattern, sign-extended as an `int` argument
                // would arrive (the high half must not leak into the result).
                let arg = i64::from(bits as i32);
                // SAFETY: as above.
                let got = unsafe { cm.try_call(&[arg]) }.expect("the body runs");
                // Cast: the low 32 bits are the result.
                assert_eq!(got as u32, bits, "{}({bits:#010x})", sop.as_str());
            }
        }
    }

    /// JIT round 22 wave 1, lane intrinsics (INTRINSICS21-7): `Math.round`
    /// EXECUTED against a transcription of the JDK 25 bodies, over the
    /// JDK-6430675 corners (the largest double below 0.5, the halves, the
    /// exponent where the shift window ends, both saturations, NaN, both
    /// zeros, subnormals, infinities) and a sweep of ordinary values. The
    /// harness passes a `double`/`float` as its raw bits in an integer
    /// register; the `int` result is compared sign-extended.
    #[test]
    pub(in crate::ir_lower::tests) fn r22_math_round_answers_the_jdk_bodies() {
        fn jdk_round_d(a: f64) -> i64 {
            let bits = a.to_bits() as i64;
            let biased_exp = (bits & 0x7FF0_0000_0000_0000) >> 52;
            let shift = (53 - 2 + 1023) - biased_exp;
            if (shift & -64) == 0 {
                let mut r = (bits & 0x000F_FFFF_FFFF_FFFF) | 0x0010_0000_0000_0000;
                if bits < 0 {
                    r = -r;
                }
                ((r >> shift) + 1) >> 1
            } else if a.is_nan() {
                0
            } else {
                // Rust's `as` saturates exactly as the JVM's `d2l` does.
                a as i64
            }
        }
        fn jdk_round_f(a: f32) -> i32 {
            let bits = a.to_bits() as i32;
            let biased_exp = (bits & 0x7F80_0000) >> 23;
            let shift = (24 - 2 + 127) - biased_exp;
            if (shift & -32) == 0 {
                let mut r = (bits & 0x007F_FFFF) | 0x0080_0000;
                if bits < 0 {
                    r = -r;
                }
                ((r >> shift) + 1) >> 1
            } else if a.is_nan() {
                0
            } else {
                a as i32
            }
        }
        let mut doubles: Vec<f64> = vec![
            0.0,
            -0.0,
            0.49999999999999994,
            -0.49999999999999994,
            0.5,
            -0.5,
            1.5,
            -1.5,
            2.5,
            -2.5,
            4_503_599_627_370_495.5,
            -4_503_599_627_370_495.5,
            4_503_599_627_370_496.0,
            9_007_199_254_740_992.0,
            9.223_372_036_854_776e18,
            -9.223_372_036_854_776e18,
            1e300,
            -1e300,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            -f64::from_bits(1),
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::from_bits(0x7FF0_0000_0000_0001),
            f64::from_bits(0xFFF8_0000_0000_0000),
        ];
        for i in -200..=200 {
            doubles.push(f64::from(i) * 0.25 + 0.125);
            doubles.push(f64::from(i) / 3.0);
        }
        let cm = compile_scalar_intrinsic(ScalarOp::RoundD);
        for &a in &doubles {
            // Cast: the raw 64-bit pattern into the integer argument slot.
            let arg = a.to_bits() as i64;
            // SAFETY: one argument in an integer register, as `Op::Param`
            // expects for an FP operand; the body calls nothing.
            let got = unsafe { cm.try_call(&[arg]) }.expect("the body runs");
            assert_eq!(got, jdk_round_d(a), "Math.round({a:e}) [{:#018x}]", a.to_bits());
        }
        let mut floats: Vec<f32> = vec![
            0.0,
            -0.0,
            0.499_999_97,
            -0.499_999_97,
            0.5,
            -0.5,
            2.5,
            -2.5,
            8_388_607.5,
            -8_388_607.5,
            8_388_608.0,
            16_777_216.0,
            2_147_483_648.0,
            -2_147_483_648.0,
            3e38,
            -3e38,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::from_bits(0x7F80_0001),
        ];
        for i in -200..=200 {
            floats.push(i as f32 * 0.25 + 0.125);
            floats.push(i as f32 / 3.0);
        }
        let cm = compile_scalar_intrinsic(ScalarOp::RoundF);
        for &a in &floats {
            // Cast: the 32-bit pattern, sign-extended as an `int` argument would arrive.
            let arg = i64::from(a.to_bits() as i32);
            // SAFETY: as above.
            let got = unsafe { cm.try_call(&[arg]) }.expect("the body runs");
            assert_eq!(got, i64::from(jdk_round_f(a)), "Math.round({a:e}f) [{:#010x}]", a.to_bits());
        }
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_add_params() {
        // int f(int a, int b) { return a + b; }
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 2, 2);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let result = unsafe { method.try_call(&[10, 20]).expect("test JIT call") };
        assert_eq!(result, 30, "10 + 20 = 30");
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_sub_params() {
        // int f(int a, int b) { return a - b; }
        let code = [0x1a, 0x1b, 0x64, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 2, 2);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let result = unsafe { method.try_call(&[30, 12]).expect("test JIT call") };
        assert_eq!(result, 18, "30 - 12 = 18");
    }

    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_mul_params() {
        // int f(int a, int b) { return a * b; }
        let code = [0x1a, 0x1b, 0x68, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 2, 2);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let result = unsafe { method.try_call(&[6, 7]).expect("test JIT call") };
        assert_eq!(result, 42, "6 * 7 = 42");
    }

    // ── Regression: BUG FIX [jit-irlower #1 + #3] — Op::Cmp SETcc ────────
    //
    // Hand-build a single-block graph whose Return value is a bare
    // `Op::Cmp(Lt)`, so the SETcc store is the only thing producing the
    // result. Two bugs had to be fixed: #1 the opcode (`+ 0x10` → `0F 9C` =
    // SETL, not the old `- 0x10` MMX byte), and #3 the MISSING ModRM byte —
    // SETcc is a /digit form, so `0F 9C` without `0xC0` desynced the stream
    // (the next `0F B6` MOVZX was partly consumed as the ModRM) and AL/RAX
    // were never written, so the Cmp returned the first operand (`a`) instead
    // of the 0/1 boolean. With `0F 9C C0` the boolean is correct.
    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_cmp_lt_setcc() {
        use crate::ir::{CmpOp, Graph, IrType, Op};

        // Mirror IrBuilder::new node layout: Start, Proj(0)=ctrl, Proj(1)=mem,
        // Param(0), Param(1).
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        // cmp = (a < b) ? 1 : 0
        let cmp = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], None);
        // Return [ctrl, cmp]
        let ret = graph.add(Op::Return, IrType::Void, vec![ctrl, cmp], None);
        graph.exit = ret;

        let schedule = ir_schedule::schedule(&graph);
        let method = lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lower cmp graph");

        // a < b  → 1
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let r_true = unsafe { method.try_call(&[3, 7]).expect("test JIT call") };
        assert_eq!(r_true, 1, "3 < 7 should set the boolean to 1");
        // a >= b → 0
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let r_false = unsafe { method.try_call(&[7, 3]).expect("test JIT call") };
        assert_eq!(r_false, 0, "7 < 3 is false → 0");
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let r_eq = unsafe { method.try_call(&[5, 5]).expect("test JIT call") };
        assert_eq!(r_eq, 0, "5 < 5 is false → 0");
    }

    // ── Regression: BUG FIX [jit-irlower #1 + #2] — ternary via IR path ──
    //
    // int f(int a, int b) { return a < b ? 1 : 0; }
    //
    // javac lowers the ternary to a compare-and-branch joined by a phi:
    //   iload_0; iload_1; if_icmpge else; iconst_1; goto end;
    //   else: iconst_0; end: ireturn
    // This exercises the fixes end-to-end: the `Op::Cmp(Ge)` feeding the `If`
    // (#1 + #3 SETcc) and the edge-split parallel copy that materialises the
    // phi value 1/0 at each branch edge (#2). The earlier "always resolves to
    // the else value" symptom was a downstream effect of the SETcc desync (#3):
    // the `If` branched on the first operand instead of the comparison, so it
    // always took the same edge. With the SETcc ModRM fix the phi/branch path
    // is correct.

    // ── Regression: the canonical production SIGSEGV shape — a pure,
    // call-free branchy predicate (e.g. `Modifier.isStatic`). Before the
    // SETcc ModRM fix (#3) this exact shape emitted a stray `SETcc [rdi]`
    // (write through a near-null base) → SIGSEGV, which is why
    // `jit/src/lib.rs` declined branchy call-free methods from the IR path.
    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_and_predicate_isstatic_shape() {
        // static boolean f(int m) { return (m & 8) != 0; }
        //  0: iload_0     1a
        //  1: bipush 8    10 08
        //  3: iand        7e
        //  4: ifeq 11     99 00 07   (m&8 == 0 → pc 11)
        //  7: iconst_1    04
        //  8: goto 12     a7 00 04
        // 11: iconst_0    03
        // 12: ireturn     ac
        let code = [
            0x1a, 0x10, 0x08, 0x7e, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x03, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 13, 1, 1).expect("branchy predicate compiles via IR");
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let f = |m: i64| unsafe { cm.try_call(&[m]).expect("call") };
        assert_eq!(f(8), 1, "8 & 8 != 0 → true");
        assert_eq!(f(0), 0, "0 & 8 == 0 → false");
        assert_eq!(f(7), 0, "7 & 8 == 0 → false");
        assert_eq!(f(15), 1, "15 & 8 != 0 → true");
    }

    // ── Regression: a phi that merges non-constant operand values (param a
    // vs param b) across the two branch edges, not just 0/1 constants —
    // exercises the edge-split parallel copy with live values.
    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_max_phi_merges_params() {
        // int max(int a, int b) { return a >= b ? a : b; }
        //  0: iload_0     1a
        //  1: iload_1     1b
        //  2: if_icmplt 9 a1 00 07   (a < b → pc 9, return b)
        //  5: iload_0     1a
        //  6: goto 10     a7 00 04
        //  9: iload_1     1b
        // 10: ireturn     ac
        let code = [
            0x1a, 0x1b, 0xa1, 0x00, 0x07, 0x1a, 0xa7, 0x00, 0x04, 0x1b, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 11, 2, 2).expect("max compiles via IR");
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let max = |a: i64, b: i64| unsafe { cm.try_call(&[a, b]).expect("call") };
        assert_eq!(max(3, 7), 7, "max(3,7)=7");
        assert_eq!(max(7, 3), 7, "max(7,3)=7");
        assert_eq!(max(5, 5), 5, "max(5,5)=5");
        assert_eq!(max(-2, -9), -2, "max(-2,-9)=-2");
    }

    // ── Gate-flip readiness probe: a COUNTED LOOP (loop-carried phi over a
    // back-edge). If this fails, the IR builder/lowerer does not yet handle
    // loop phis, and branchy call-free methods (which include loops) must NOT
    // be routed to the IR path — i.e. the lib.rs gate cannot be flipped on the
    // SETcc fix alone.
    #[test]
    pub(in crate::ir_lower::tests) fn test_lower_counted_loop_sum() {
        // int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; }
        //  0: iconst_0  1: istore_1  2: iconst_0  3: istore_2
        //  4: iload_2   5: iload_0   6: if_icmpge 19
        //  9: iload_1  10: iload_2  11: iadd  12: istore_1
        // 13: iinc 2,1 16: goto 4   19: iload_1 20: ireturn
        let code = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84,
            0x02, 0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 21, 1, 3).expect("loop compiles via IR");
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let sum = |n: i64| unsafe { cm.try_call(&[n]).expect("call") };
        assert_eq!(sum(5), 10, "0+1+2+3+4 = 10");
        assert_eq!(sum(0), 0, "empty loop = 0");
        assert_eq!(sum(10), 45, "sum 0..9 = 45");
    }


    /// **The one that matters: enter the loop part-way and finish it.**
    ///
    /// `int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; }`, whose
    /// loop header is bci 4. The stub is called the way an OSR trigger would
    /// call it — with an interpreter locals array, not with the method's
    /// arguments — carrying a state the method could never have reached from
    /// its own entry: `n=10, s=100, i=7`. A correct entry finishes the loop
    /// from there and returns `100 + 7 + 8 + 9 = 124`.
    ///
    /// Nothing else proves this. The frame build, the callee-saved saves, the
    /// clean-frame zeroing, the ORDER of that zeroing against the seeding
    /// (`zero_ref_phi_slots` would erase a seeded reference), the parked
    /// locals pointer surviving `emit_frame_record`'s call, the seeding
    /// itself and the jump are six independent chances to be wrong, and every
    /// one of them fails by returning a plausible number.
    #[test]
    pub(in crate::ir_lower::tests) fn an_osr_entry_finishes_a_loop_it_did_not_start() {
        let _osr = OsrEntryForce::on();
        //  0: iconst_0  1: istore_1  2: iconst_0  3: istore_2
        //  4: iload_2   5: iload_0   6: if_icmpge 19
        //  9: iload_1  10: iload_2  11: iadd  12: istore_1
        // 13: iinc 2,1 16: goto 4   19: iload_1 20: ireturn
        let code = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84,
            0x02, 0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 21, 1, 3).expect("loop compiles via IR");
        // The ordinary entry still works, unchanged.
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        assert_eq!(unsafe { cm.try_call(&[10]).expect("call") }, 45);

        let Some((_addr, _needed)) = cm.ir_osr_entry_addr(4) else {
            // Not every configuration admits the header (the refusal census
            // says which clause declined). Assert the shape that IS guaranteed:
            // a refusal is silent and total, never a half-emitted stub.
            assert!(
                cm.ir_osr_entries.is_empty(),
                "an entry for some other bci while the loop header was refused \
                 means the eligibility test and the emitter disagree",
            );
            return;
        };
        // The interpreter's locals at bci 4: local 0 = n, 1 = s, 2 = i.
        let locals: [i64; 3] = [10, 100, 7];
        // Through the artifact's own entry point, which is what the VM calls:
        // it re-checks the bci and refuses a locals slice shorter than the stub
        // reads, so the test exercises the admission as well as the code.
        //
        // SAFETY: the stub builds and tears down its own frame, reads exactly
        // `locals[0..3]`, and returns the method's `int` result in RAX.
        let entered = |l: &[i64]| unsafe { cm.ir_osr_enter(4, 0, l) };
        assert_eq!(
            entered(&locals),
            Some(124),
            "entering at the header with s=100 i=7 n=10 must run i=7,8,9 and \
             return 100+7+8+9",
        );
        // A second entry, to catch a stub that works once because it left the
        // frame or a callee-saved register in a state the next entry inherits.
        let again: [i64; 3] = [5, 0, 2];
        assert_eq!(entered(&again), Some(9), "i=2,3,4 from s=0 is 2+3+4");
        // And the admission: a locals slice shorter than the stub reads is
        // refused rather than read past.
        assert_eq!(
            entered(&again[..1]),
            None,
            "a short locals snapshot must be refused, not read past",
        );
    }



    /// RB25-1 (`CRATONVM_JIT_IR_OSR_FLAG_IF_FOLD`): the loop of
    /// `r11w17_tier_an_opaque_arm_is_entered_and_publishes_its_bci`, with its
    /// opaque arm, lowered with the switch off and on. On: the arm's `If` is a
    /// plain jump to the method-entry edge and the flag's `XOR; store` is left
    /// out (step 2), so the body is shorter; both doors still answer exactly.







    /// Round 16 (lane osrdoor): the decision of
    /// `osr_entry_value_needs_no_seed`, rule by rule, with each switch alone.
    #[test]
    pub(in crate::ir_lower::tests) fn r16_osrdoor_which_unnamed_values_an_osr_entry_owes_no_seed() {
        let mut g = Graph::empty(8);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let merge = g.add(Op::Merge, IrType::Control, vec![start], Some(4));
        let mem_phi = g.add(Op::Phi, IrType::Memory, vec![merge, start], Some(4));
        let long_phi = g.add(Op::Phi, IrType::Long, vec![merge, start], Some(4));
        let ref_phi = g.add(Op::Phi, IrType::Ref, vec![merge, start], Some(4));
        let add = g.add(Op::Add, IrType::Int, vec![], Some(9));
        // `f(value, in the entry block, dataflow answer, tokens switch, dataflow switch)`.
        let f = |id: NodeId, own: bool, live_in: fn() -> Option<bool>, tok: bool, flow: bool| {
            Lowerer::osr_entry_value_needs_no_seed_by(
                &g.nodes[id as usize],
                own,
                live_in,
                tok,
                flow,
            )
        };
        let dead: fn() -> Option<bool> = || Some(false);
        let live: fn() -> Option<bool> = || Some(true);
        let unknown: fn() -> Option<bool> = || None;
        // Both switches off: nothing is excused, whatever the dataflow says.
        for id in [mem_phi, long_phi, ref_phi, add] {
            assert!(!f(id, false, dead, false, false));
        }
        // Memory tokens: a memory φ, even the entry block's own and even
        // "live"; never a value.
        assert!(f(mem_phi, true, live, true, false));
        assert!(!f(long_phi, false, dead, true, false));
        assert!(!f(add, false, dead, true, false));
        // Dataflow: a primitive the dataflow calls dead, and only that.
        assert!(f(long_phi, false, dead, false, true));
        assert!(f(add, false, dead, false, true));
        assert!(!f(add, false, live, false, true));
        assert!(
            !f(add, false, unknown, false, true),
            "an unconverged model is not a verdict of death",
        );
        assert!(
            !f(long_phi, true, dead, false, true),
            "a φ of the entry block is written by the edge copies the stub skips",
        );
        assert!(
            !f(ref_phi, false, dead, false, true),
            "a reference is never excused: a safepoint map may publish its word",
        );
        assert!(
            !f(mem_phi, false, dead, false, true),
            "a memory φ is the token rule's, not the dataflow rule's",
        );
    }



    /// **The entered body may not read an XMM register nobody wrote.**
    ///
    /// The constants defect above, in the other register file, and reached
    /// without a merge, a literal arm or a splice:
    ///
    /// ```java
    /// int f(int n) { double a = (double) n; int s = 0;
    ///                for (int i = 0; i < n; i++) {
    ///                    s += (int) (a + i);
    ///                    s += (int) (a + s);
    ///                }
    ///                return s; }
    /// ```
    ///
    /// `a` is an ordinary JVM local. It is not a phi -- nothing reassigns it
    /// in the loop -- so at the header the snapshot names the `Op::I2D` that
    /// computed it, the eligibility test is satisfied, and the stub seeds it.
    /// Seeded into its HOME WORD, which was the whole of the seeding: and an
    /// FP definition does not stop at the home word. `fp_store_value` copies
    /// the result into the value's assigned XMM and marks it resident, after
    /// which every reader in the body takes the REGISTER (`fp_load_value` ->
    /// `resident_xmm`) and never looks at the frame again. The stub jumps past
    /// that definition, so the body read whatever floating-point register the
    /// previous frame happened to leave behind.
    ///
    /// **Observable on Windows only, and that is a property of the defect
    /// rather than of the test.** A value is register-resident across a
    /// safepoint only if its register survives one, and `IR_PROLOGUE_SAVED` --
    /// the XMMs this prologue saves -- is `[6, 7]` on Win64 and EMPTY on
    /// System V, where the ABI makes every XMM volatile. So on Linux every
    /// register in `IR_LINEAR_SCAN` is caller-saved, an FP value live across
    /// the safepoint at a loop header is split rather than promoted, and there
    /// is no stale register for the entry to leave behind. The byte scan below
    /// is what establishes which of the two a given run is, instead of letting
    /// System V report a pass it did not earn.
    ///
    /// Three details of the fixture are load-bearing, and every one of them
    /// was found by watching an earlier fixture fail to reproduce:
    ///
    /// * `a` read TWICE, because residency is `static_uses >= 2`
    ///   (`ir_residency_pays_here`, whose loop weighting is default-off). A
    ///   value read once per trip gets no register, stays in its frame word,
    ///   and is seeded correctly by the unfixed emitter.
    /// * the two reads adding `i` and `s`, not `i` twice: GVN merges two
    ///   `(double) i` conversions into ONE node with two uses of its own, and
    ///   that one -- defined in the loop, with the shorter range -- takes the
    ///   register instead of `a`.
    /// * neither read loop-invariant, because LICM hoists an invariant
    ///   `(int) a` out of the loop and the hoisted `Op::D2I` is then an
    ///   unnamed integer live across the header, which the eligibility test
    ///   refuses outright -- a test that never enters, passing for the wrong
    ///   reason.
    ///
    /// The byte scan below is what turned each of those from a false pass into
    /// a failure, and it is why this test is worth more than its assertion.
    #[test]
    pub(in crate::ir_lower::tests) fn an_osr_entry_seeds_the_fp_registers_its_body_reads() {
        let _osr = OsrEntryForce::on();
        //  0: iload_0  1: i2d  2: dstore_1        (a = (double) n)
        //  3: iconst_0 4: istore_3                (s = 0)
        //  5: iconst_0 6: istore 4                (i = 0)
        //  8: iload 4 10: iload_0 11: if_icmpge 37   <- header, bci 8
        // 14: iload_3 15: dload_1 16: iload 4 18: i2d 19: dadd 20: d2i
        // 21: iadd    22: istore_3
        // 23: iload_3 24: dload_1 25: iload_3 26: i2d 27: dadd 28: d2i
        // 29: iadd    30: istore_3
        // 31: iinc 4,1 34: goto 8  37: iload_3  38: ireturn
        let code = [
            0x1a, 0x87, 0x48, 0x03, 0x3e, 0x03, 0x36, 0x04, 0x15, 0x04, 0x1a, 0xa2, 0x00, 0x1a,
            0x1d, 0x27, 0x15, 0x04, 0x87, 0x63, 0x8e, 0x60, 0x3e, 0x1d, 0x27, 0x1d, 0x87, 0x63,
            0x8e, 0x60, 0x3e, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xe6, 0x1d, 0xac, 0, 0,
        ];
        // The bytecode's own arithmetic, in Rust, so the expected answers are
        // derived rather than hand-computed: `d2i` truncates toward zero and
        // `iadd` is 32-bit, which `as i32` and `wrapping_add` reproduce.
        let expect = |n: i32, a: f64, s0: i32, i0: i32| -> i32 {
            let (mut s, mut i) = (s0, i0);
            while i < n {
                s = s.wrapping_add((a + f64::from(i)) as i32);
                s = s.wrapping_add((a + f64::from(s)) as i32);
                i += 1;
            }
            s
        };

        let cm = compile_via_ir(&code, 39, 1, 5).expect("the FP-invariant loop compiles via IR");
        // The ordinary entry, which never skipped the definition and was always
        // right. `a` is `(double) n` here, because that is what the method's
        // own prologue computes.
        // Cast: both doors hand back RAX, whose low 32 bits are this method's
        // `int` result; the upper half is not sign-extended, so the comparison
        // is made on the 32 bits the body actually wrote.
        assert_eq!(
            // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
            unsafe { cm.try_call(&[5]).expect("test JIT call") } as i32,
            expect(5, 5.0, 0, 0),
            "the method-entry door",
        );

        let Some((_addr, _needed)) = cm.ir_osr_entry_addr(8) else {
            assert!(
                cm.ir_osr_entries.is_empty(),
                "an entry for some other bci while the loop header was refused \
                 means the eligibility test and the emitter disagree",
            );
            return;
        };
        // Cast: as above.
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let entered = |l: &[i64]| unsafe { cm.ir_osr_enter(8, 0, l) }.map(|v| v as i32);
        // Enter at the header in a state the method could not have reached
        // from its own entry -- `a` is 2.5, which `(double) n` never produces,
        // so a stub that leaves the register alone cannot accidentally hold
        // the right bits. The fraction also makes `d2i`'s truncation part of
        // the answer, so a register holding SOME double is not enough either.
        //
        // SAFETY: the stub builds and tears down its own frame, reads exactly
        // `locals[0..5]`, and returns the method's `int` result in RAX.
        // Cast: an IEEE-754 bit pattern into the locals array's word.
        let locals: [i64; 5] = [6, 2.5f64.to_bits() as i64, 0, 3, 2];
        assert_eq!(
            entered(&locals),
            Some(expect(6, 2.5, 3, 2)),
            "`a` must come from the XMM this stub seeded, not from whatever \
             floating-point register the previous frame left behind",
        );
        // A second entry, with a DIFFERENT `a`, from a frame the first entry
        // has just used: a stub that seeds the register once, or that passes
        // because the right value happened to still be there, fails here.
        // Cast: as above.
        let again: [i64; 5] = [4, (-1.75f64).to_bits() as i64, 0, 0, 0];
        assert_eq!(
            entered(&again),
            Some(expect(4, -1.75, 0, 0)),
            "a second entry must re-seed the register rather than inherit it",
        );

        // The vacuity guard, LAST: the two entries above are the claim, and
        // this is the evidence that they were asked anything at all. If the
        // allocator gave `a` no register there is no stale register to read,
        // the body loads it from its frame word, and both entries pass on the
        // unfixed emitter too. This graph builds no `Op::ConstF` and the
        // stub's other work is integer stores, so a MOVSD inside the stub can
        // only be the FP publish this test exists to check.
        let bytes = cm.code_bytes();
        let (_, stub_off, _) = *cm
            .ir_osr_entries
            .first()
            .expect("the header entry was found above");
        assert_eq!(cm.ir_osr_entries.len(), 1, "one entry, so one stub to scan");
        let movsd = |b: &[u8]| b.windows(3).filter(|w| *w == [0xF2, 0x0F, 0x10]).count();
        let (in_body, in_stub) = (
            movsd(&bytes[..stub_off as usize]),
            movsd(&bytes[stub_off as usize..]),
        );
        if IR_LOWER_SAVED_XMMS.is_empty() && !ir_poll_preserves_fp() {
            // System V with the back-edge poll still clobbering the XMM bank:
            // every register in `IR_LINEAR_SCAN` is caller-saved, so an FP
            // value live across the header's poll is split rather than
            // promoted and there is no stale register for the entry to leave
            // behind.
            assert_eq!(
                in_stub, 0,
                "an FP value is register-resident across a clobbering back-edge \
                 poll ({in_body} MOVSD in the body)",
            );
            return;
        }
        if IR_LOWER_SAVED_XMMS.is_empty() && in_stub == 0 {
            // System V since round 11 wave 6: the poll reloads the caller-saved
            // file (`ir_poll_preserves_fp`), so `a` MAY now hold a register
            // across the header, but any call the loop makes still splits it.
            // Both are legal; the two entries above were asked either way.
            return;
        }
        assert!(
            in_stub > 0,
            "the stub publishes no XMM register ({in_body} MOVSD in the body, \
             {in_stub} in the stub): the allocator gave `a` none, so this \
             fixture no longer reproduces the defect it was written for and \
             must be repaired rather than believed",
        );
    }

