use super::*;
use crate::runtime::offload::input_cache;

// ------------------------------------------------------------------
// `descriptor_could_ever_dispatch` — the screen the compiled dispatch
// helper puts in front of the gate query, so that asking about a
// forward-referenced target does not cost every compiled static call
// site in the program an analyzer pass.
//
// It is a NECESSARY condition, so the direction that matters is the
// false NEGATIVE: a descriptor it rejects never gets asked about, and
// a kernel rejected here is a kernel that stays dark. Each case below
// is one the dispatcher itself insists on, checked against
// `target_can_ever_dispatch`'s two gates.
// ------------------------------------------------------------------

#[test]
fn the_descriptor_screen_admits_the_three_shapes_try_dispatch_launches() {
    // void, the ordinary kernel shape.
    assert!(descriptor_could_ever_dispatch("([I[I)V", 1));
    // `)I` and `)J`, the reduction shapes.
    assert!(descriptor_could_ever_dispatch("([I[I)I", 1));
    assert!(descriptor_could_ever_dispatch("([J)J", 1));
}

#[test]
fn the_descriptor_screen_rejects_a_return_type_try_dispatch_cannot_hand_back() {
    // `try_dispatch` has an arm for void, `)I` and `)J` and nothing
    // else, so a `float`/`double`/reference return can never dispatch
    // however eligible the body is.
    assert!(!descriptor_could_ever_dispatch("([F[F)F", 1));
    assert!(!descriptor_could_ever_dispatch("([D)D", 1));
    assert!(!descriptor_could_ever_dispatch("([I)Ljava/lang/String;", 1));
}

#[test]
fn the_descriptor_screen_needs_an_array_only_while_min_work_is_armed() {
    // With `--gpu-min-work` above zero the work estimate is
    // structurally zero unless a parameter can be an array, so a
    // scalar-only signature can never clear the threshold...
    assert!(!descriptor_could_ever_dispatch("(II)V", 1));
    // ...but `--gpu-min-work 0` disables the threshold, and then the
    // same signature is admissible. Screening it out at zero would
    // hide a kernel the dispatcher would have launched.
    assert!(descriptor_could_ever_dispatch("(II)V", 0));
    // A reference array still counts as an array here: this screen is
    // a necessary condition, and the element-type refusal belongs to
    // the analyzer, which the caller asks next.
    assert!(descriptor_could_ever_dispatch("([Ljava/lang/Object;)V", 1));
}


#[test]
fn the_descriptor_screen_survives_a_descriptor_it_cannot_parse() {
    // `descriptor_has_array_parameter` returns false rather than
    // panicking on a malformed descriptor, and the screen must not
    // turn that into an admission.
    assert!(!descriptor_could_ever_dispatch("", 1));
    assert!(!descriptor_could_ever_dispatch("not a descriptor", 1));
    // No parameter list at all, but the right return shape: still no
    // array, so still refused while min-work is armed.
    assert!(!descriptor_could_ever_dispatch("()V", 1));
}

// ------------------------------------------------------------------
// `scan_invokestatic_cp_indices` — the novel logic this module
// introduces. `caller_blocks_jit`/`compute` are not exercised here:
// both need a real `SharedVm` with a populated `ClassManager`, which
// (per `crate::runtime::offload`'s own test-module comment) needs
// "~30 fields of unrelated bookkeeping" to construct — exactly the
// "existing test fixtures make that hard" case the task anticipated.
// The pure bytecode scanner and constant-pool resolver below are the
// parts that can go wrong independent of any VM state, so they get
// the thorough hand-rolled-bytecode coverage.
// ------------------------------------------------------------------

// ------------------------------------------------------------------
// The compiled-tier barrier, decoded rather than re-asserted.
//
// `gpu_barrier`'s own tests check the encoder against the same
// arithmetic that produced it, which cannot catch a wrong ModRM or a
// REX bit naming the wrong register -- the sequence would still be
// 51 bytes with the jumps landing correctly and would still pass.
// `iced-x86` is an independent decoder, and it lives in this crate,
// so the check lives here.
// ------------------------------------------------------------------

/// Decode the emitted barrier and assert it is the eleven
/// instructions the design says it is, on the registers it says.
///
/// A wrong register here is not a crash, it is silent corruption in
/// one direction (clobbering a live value) or a silently dead
/// barrier in the other (testing a bit of the wrong word, so a
/// compiled write never marks its bucket and the device keeps
/// serving stale data).
#[test]
fn the_emitted_barrier_decodes_to_the_documented_sequence() {
    use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};

    const BASE: u64 = 0x0000_7FF0_0010_0000;
    const FILTER: usize = 0x0000_7FF0_0020_0000;
    const DIRTY: usize = 0x0000_7FF0_0020_0040;

    // Encode from an explicit pair. This used to arm the process
    // globals and restore them, with a comment saying save-and-restore
    // kept the rest of the binary safe. It does not: cargo runs these
    // tests as parallel THREADS, so this and `the_barrier_never_writes_rax`
    // interleave -- and `armed_before` was itself the race, asserting
    // "no test in this binary should have armed the barrier" while the
    // sibling test had. Meanwhile `array_writer_policy` below reads
    // `is_armed()` in PRODUCTION, so an arming window here silently
    // moves the policy any concurrent test observes.
    //
    // `barrier_bytes_for` is the same encoder without the globals; see
    // `gpu_barrier::arm`'s doc, and `jit/tests/gpu_barrier_arming.rs`
    // for what covers the publishing itself.
    let bytes =
        cratonvm_jit::gpu_barrier::barrier_bytes_for(FILTER, DIRTY).expect("armed pair");

    let mut decoder = Decoder::with_ip(64, &bytes, BASE, DecoderOptions::NONE);
    let decoded: Vec<_> = decoder.iter().collect();

    // Eleven instructions, and the decoder must consume the sequence
    // exactly: a trailing partial instruction would mean the length
    // constant and the encoding disagree.
    assert_eq!(decoded.len(), 11, "{decoded:?}");
    let consumed: usize = decoded.iter().map(|i| i.len()).sum();
    assert_eq!(consumed, cratonvm_jit::gpu_barrier::BARRIER_LEN);
    let end = BASE + cratonvm_jit::gpu_barrier::BARRIER_LEN as u64;

    // MOV R11, &ADDR_FILTER
    assert_eq!(decoded[0].mnemonic(), Mnemonic::Mov);
    assert_eq!(decoded[0].op0_register(), Register::R11);
    assert_eq!(decoded[0].immediate64(), FILTER as u64);

    // CMP QWORD [R11], 0 -- the whole fast path rests on this reading
    // the FILTER WORD, not the pointer to it.
    assert_eq!(decoded[1].mnemonic(), Mnemonic::Cmp);
    assert_eq!(decoded[1].op0_kind(), OpKind::Memory);
    assert_eq!(decoded[1].memory_base(), Register::R11);
    assert_eq!(decoded[1].memory_displacement64(), 0);
    assert_eq!(decoded[1].immediate32(), 0);

    // JZ .skip
    assert_eq!(decoded[2].mnemonic(), Mnemonic::Je);
    assert_eq!(decoded[2].near_branch_target(), end);

    // MOV R11, [R11] -- the filter word itself
    assert_eq!(decoded[3].mnemonic(), Mnemonic::Mov);
    assert_eq!(decoded[3].op0_register(), Register::R11);
    assert_eq!(decoded[3].memory_base(), Register::R11);

    // MOV R10, RAX -- RAX is the array pointer at every store site.
    assert_eq!(decoded[4].mnemonic(), Mnemonic::Mov);
    assert_eq!(decoded[4].op0_register(), Register::R10);
    assert_eq!(decoded[4].op1_register(), Register::RAX);

    // SHR R10, 3 ; AND R10, 63 -- the `(addr >> 3) & 63` bucket,
    // identical to `input_cache::addr_bit`.
    assert_eq!(decoded[5].mnemonic(), Mnemonic::Shr);
    assert_eq!(decoded[5].op0_register(), Register::R10);
    assert_eq!(decoded[5].immediate8(), 3);
    assert_eq!(decoded[6].mnemonic(), Mnemonic::And);
    assert_eq!(decoded[6].op0_register(), Register::R10);
    assert_eq!(decoded[6].immediate8to64(), 63);

    // BT R11, R10 -- bit R10 of the filter word. Register
    // destination, so the offset is mod 64 and the AND above is
    // belt-and-braces rather than load-bearing.
    assert_eq!(decoded[7].mnemonic(), Mnemonic::Bt);
    assert_eq!(decoded[7].op0_register(), Register::R11);
    assert_eq!(decoded[7].op1_register(), Register::R10);

    // JNC .skip
    assert_eq!(decoded[8].mnemonic(), Mnemonic::Jae);
    assert_eq!(decoded[8].near_branch_target(), end);

    // MOV R11, &DIRTY ; MOV BYTE [R11 + R10*1], 1
    assert_eq!(decoded[9].mnemonic(), Mnemonic::Mov);
    assert_eq!(decoded[9].op0_register(), Register::R11);
    assert_eq!(decoded[9].immediate64(), DIRTY as u64);
    assert_eq!(decoded[10].mnemonic(), Mnemonic::Mov);
    assert_eq!(decoded[10].op0_kind(), OpKind::Memory);
    assert_eq!(decoded[10].memory_base(), Register::R11);
    assert_eq!(decoded[10].memory_index(), Register::R10);
    assert_eq!(decoded[10].memory_index_scale(), 1);
    assert_eq!(decoded[10].memory_displacement64(), 0);
    assert_eq!(decoded[10].immediate8(), 1);

    // Nothing outside R10, R11 and the flags is written. This is the
    // property that lets the sequence be dropped after a store in
    // both backends without spilling anything first.
    for insn in &decoded {
        for i in 0..insn.op_count() {
            if insn.op_kind(i) == OpKind::Register {
                let r = insn.op_register(i);
                assert!(
                    matches!(r, Register::R10 | Register::R11 | Register::RAX),
                    "barrier touches {r:?}, which no store site guarantees is dead"
                );
            }
        }
    }
}

/// RAX is read and never written: the array pointer is still needed
/// by nothing here, but writing it would corrupt a backend that
/// keeps using it (the single-pass `fastore` arm re-reads nothing,
/// but the contract is what the next store site will rely on).
#[test]
fn the_barrier_never_writes_rax() {
    use iced_x86::{Decoder, DecoderOptions, OpKind, Register};

    // No arming: see the sibling test above for why this binary must
    // not publish to the process globals.
    let bytes =
        cratonvm_jit::gpu_barrier::barrier_bytes_for(0x1000, 0x2000).expect("armed pair");

    let mut decoder = Decoder::with_ip(64, &bytes, 0x1_0000, DecoderOptions::NONE);
    for insn in decoder.iter() {
        if insn.op_count() > 0 && insn.op0_kind() == OpKind::Register {
            assert_ne!(
                insn.op0_register(),
                Register::RAX,
                "the barrier must not clobber the array pointer"
            );
        }
    }
}

#[test]
fn scan_empty_code_finds_nothing() {
    assert!(scan_invokestatic_cp_indices(&[]).is_empty());
}

// ------------------------------------------------------------------
// `method_writes_primitive_array` — the JIT half of Phase 10 #2.
// ------------------------------------------------------------------

/// The residency cache stands down for the JIT, and stays down.
///
/// AUDIT 2026-09-02. This pins the mechanism behind
/// [`ArrayWriterPolicy::AllowJit`]: when that policy is selected,
/// admitting a method that writes a primitive array disables the
/// cache instead of refusing the method. The DEFAULT does not reach
/// it — see the module docs for the measurement that put it behind a
/// flag — so this test calls the mechanism directly rather than
/// going through `compute`.
///
/// # What this can and cannot reach without a device
///
/// Every real cache entry owns an `Arc<DeviceBuffer<T>>`, and a
/// `DeviceBuffer` cannot be constructed without a CUDA driver — a
/// stub build's constructors all return `NoDriver`. So the parts a
/// unit test can observe are the switch, the teardown of whatever
/// the table held, and the address filter that guards `invalidate`'s
/// hot path. The refusal of FUTURE entries is a single early return
/// at the top of `insert`, which is the only function that inserts;
/// it is checked here by asserting the predicate that return reads,
/// and end-to-end by the hardware gate in `gpu-selfhosted.yml`.
///
/// The switch is process-wide and one-way by design, which is also
/// why this is one test rather than three: a later test could not
/// observe the "before" state.
#[test]
fn disabling_the_input_cache_drops_what_it_holds_and_refuses_more() {
    assert!(
        input_cache::is_enabled(),
        "the cache must start enabled, or the rest of this proves nothing"
    );
    assert!(
        input_cache::table_len_for_test() == 0,
        "no test in this binary should have populated the cache"
    );

    input_cache::disable_for_jit_array_writer();

    assert!(
        !input_cache::is_enabled(),
        "the switch did not flip; `insert` would keep accepting entries \
         that nothing can invalidate once compiled code writes the array"
    );
    assert_eq!(
        input_cache::table_len_for_test(),
        0,
        "every entry must be gone: one cached a moment ago mirrors an \
         array the about-to-run compiled code may write"
    );
    assert_eq!(
        input_cache::addr_filter_for_test(),
        0,
        "the address filter must be rebuilt from the emptied table, or \
         `invalidate` keeps paying for a lock and a failed lookup on \
         every array store in the VM"
    );

    // Idempotent: the gate calls this once per admitted method, which
    // for a large program is thousands of times.
    input_cache::disable_for_jit_array_writer();
    assert!(!input_cache::is_enabled());
    assert_eq!(input_cache::table_len_for_test(), 0);
}

#[test]
fn array_store_scan_finds_each_cached_element_type() {
    for (op, name) in [
        (0x4fu8, "iastore"),
        (0x50, "lastore"),
        (0x51, "fastore"),
        (0x52, "dastore"),
        // Added 2026-09-02 with the offload path that made
        // `short[]`/`byte[]` marshallable, and therefore cacheable.
        // Until then this scan was right to ignore them.
        (0x54, "bastore"),
        (0x56, "sastore"),
        // Added 2026-09-03 with char[] marshalling.
        (0x55, "castore"),
    ] {
        // aload_0; iconst_0; iconst_1; <astore>; return
        let code = [0x2a, 0x03, 0x04, op, 0xb1];
        assert!(
            method_writes_primitive_array(&code),
            "{name} must block JIT admission while offload is live"
        );
    }
}

#[test]
fn array_store_scan_ignores_reference_stores() {
    // What is left out, and why each one.
    //
    // `aastore` (0x53) stores references; the input cache holds only
    // primitive buffers.
    //
    // `castore` (0x55) USED to be here for the same reason, and moved
    // to the blocking set on 2026-09-03 when `char[]` became
    // marshallable. That is the whole pattern: this list is not a
    // fixed fact about opcodes, it is a shadow of
    // `is_marshallable_array_element`, and it has to move whenever
    // that does.
    //
    // This test used to also assert 0x54 and 0x56, on the reasoning
    // that "bastore/castore/sastore never reach the input cache,
    // which only holds int/long/float/double buffers -- bastore has
    // a helper hook anyway". Both halves stopped being true on
    // 2026-09-02: `short[]`/`byte[]` became marshallable and so
    // cacheable, and `jit_bastore` in fact had NO invalidation hook
    // (only `jit_iastore` did). A JIT-compiled `short[]`/`byte[]`
    // writer then left the residency cache stale, reproducibly --
    // `test_classes/gpu/GpuJitWriterStale.java` diverges from
    // HotSpot on the short and byte checksums at n=8192 over 2000
    // rounds while the int one, whose writer this scan does refuse,
    // stays correct.
    for op in [0x53u8] {
        let code = [0x2a, 0x03, 0x04, op, 0xb1];
        assert!(!method_writes_primitive_array(&code), "opcode {op:#x}");
    }
}

#[test]
fn array_store_scan_is_not_fooled_by_an_operand_byte() {
    // `bipush 0x4f` — the 0x4f is an immediate, not an iastore. A
    // naive byte scan would block every method containing the
    // constant 79, so this is the property that keeps the gate from
    // disabling the JIT across the whole program.
    let code = [0x10, 0x4f, 0xb1];
    assert!(!method_writes_primitive_array(&code));
}

#[test]
fn array_store_scan_sees_a_store_after_a_switch_table() {
    // A lookupswitch body full of arbitrary bytes, then a real
    // iastore: the walk must resync on the instruction boundary.
    let mut code = vec![0x2a, 0xab, 0x00, 0x00]; // aload_0; lookupswitch; pad to 4
    code.extend_from_slice(&[0, 0, 0, 8]); // default
    code.extend_from_slice(&[0, 0, 0, 0]); // npairs = 0
    code.push(0x4f); // iastore
    assert!(method_writes_primitive_array(&code));
}

#[test]
fn scan_code_reports_both_facts_from_one_walk() {
    // aload_0; iconst_0; iconst_1; iastore; invokestatic #7; return
    let code = [0x2a, 0x03, 0x04, 0x4f, 0xb8, 0x00, 0x07, 0xb1];
    let (indices, writes) = scan_code(&code);
    assert_eq!(indices, vec![7]);
    assert!(writes);
}

#[test]
fn scan_finds_single_invokestatic() {
    // invokestatic #7, then return.
    let code = [0xb8, 0x00, 0x07, 0xb1];
    assert_eq!(scan_invokestatic_cp_indices(&code), vec![7]);
}

#[test]
fn scan_finds_multiple_invokestatic_in_order() {
    // invokestatic #1; pop; invokestatic #300; return.
    let code = [0xb8, 0x00, 0x01, 0x57, 0xb8, 0x01, 0x2c, 0xb1];
    assert_eq!(scan_invokestatic_cp_indices(&code), vec![1, 300]);
}

#[test]
fn scan_ignores_stray_0xb8_inside_an_operand() {
    // bipush 0xB8 (i.e. -72 as a signed byte immediate) must NOT be
    // misread as an invokestatic at the operand byte's offset. Real
    // invokestatic (#42) follows.
    let code = [
        0x10, 0xb8, // bipush -72
        0xb8, 0x00, 0x2a, // invokestatic #42
        0xb1, // return
    ];
    assert_eq!(scan_invokestatic_cp_indices(&code), vec![42]);
}

#[test]
fn scan_skips_tableswitch_body_correctly() {
    // A tableswitch whose padding/jump-offset bytes are chosen to
    // contain 0xB8 byte values, followed by a real invokestatic.
    // pc=0: tableswitch opcode.
    let mut code = vec![0xaa];
    // pad to next 4-byte boundary from pc=1: (4 - (1 % 4)) % 4 = 3.
    code.extend_from_slice(&[0xb8, 0xb8, 0xb8]); // padding bytes (garbage)
    code.extend_from_slice(&0i32.to_be_bytes()); // default offset
    code.extend_from_slice(&0i32.to_be_bytes()); // low = 0
    code.extend_from_slice(&1i32.to_be_bytes()); // high = 1
    code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // jump entry 0 (garbage, looks like invokestatic bytes)
    code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // jump entry 1 (garbage)
                                                          // Now a real invokestatic #99.
    code.extend_from_slice(&[0xb8, 0x00, 0x63]);
    assert_eq!(scan_invokestatic_cp_indices(&code), vec![99]);
}

#[test]
fn scan_skips_lookupswitch_body_correctly() {
    // pc=0: lookupswitch opcode.
    let mut code = vec![0xab];
    code.extend_from_slice(&[0xb8, 0xb8, 0xb8]); // padding (garbage)
    code.extend_from_slice(&0i32.to_be_bytes()); // default offset
    code.extend_from_slice(&1i32.to_be_bytes()); // npairs = 1
    code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // match (garbage)
    code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // offset (garbage)
    code.extend_from_slice(&[0xb8, 0x00, 0x05]); // real invokestatic #5
    assert_eq!(scan_invokestatic_cp_indices(&code), vec![5]);
}

#[test]
fn scan_skips_wide_iinc_correctly() {
    // wide iinc idx=0x0001 const=0x00b8 (6 bytes total), then a real
    // invokestatic. The wide operand bytes deliberately contain 0xB8.
    let code = [
        0xc4, 0x84, 0x00, 0x01, 0x00, 0xb8, // wide iinc
        0xb8, 0x00, 0x0a, // invokestatic #10
    ];
    assert_eq!(scan_invokestatic_cp_indices(&code), vec![10]);
}

#[test]
fn scan_truncated_invokestatic_operand_does_not_panic() {
    // invokestatic with only one operand byte present.
    let code = [0xb8, 0x00];
    assert!(scan_invokestatic_cp_indices(&code).is_empty());
}

#[test]
fn scan_no_invokestatic_present() {
    let code = [0x2a, 0xb1]; // aload_0; return
    assert!(scan_invokestatic_cp_indices(&code).is_empty());
}

// ------------------------------------------------------------------
// `resolve_method_ref`
// ------------------------------------------------------------------

/// Build a tiny constant pool:
///   1: Utf8 "com/example/Kernel"
///   2: ClassReference -> 1
///   3: Utf8 "vectorAdd"
///   4: Utf8 "([I[I[I)V"
///   5: NameAndType(3, 4)
///   6: MethodReference(2, 5)
///   7: InterfaceMethodReference(2, 5)
///   8: Integer(42) — a non-method-ref entry for the negative test.
fn sample_cp() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,                         // 0 (unused)
        ConstantPoolEntry::Utf8("com/example/Kernel".into()), // 1
        ConstantPoolEntry::ClassReference { name_index: 1 },  // 2
        ConstantPoolEntry::Utf8("vectorAdd".into()),          // 3
        ConstantPoolEntry::Utf8("([I[I[I)V".into()),          // 4
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        ConstantPoolEntry::InterfaceMethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 7
        ConstantPoolEntry::Integer(42),                       // 8
    ])
}

#[test]
fn resolve_method_reference() {
    let cp = sample_cp();
    let resolved = resolve_method_ref(&cp, 6).expect("MethodReference must resolve");
    assert_eq!(resolved, ("com/example/Kernel", "vectorAdd", "([I[I[I)V"));
}

#[test]
fn resolve_interface_method_reference() {
    let cp = sample_cp();
    let resolved = resolve_method_ref(&cp, 7).expect("InterfaceMethodReference must resolve");
    assert_eq!(resolved, ("com/example/Kernel", "vectorAdd", "([I[I[I)V"));
}

#[test]
fn resolve_non_method_ref_entry_returns_none() {
    let cp = sample_cp();
    assert!(resolve_method_ref(&cp, 8).is_none());
}

#[test]
fn resolve_out_of_range_index_returns_none() {
    let cp = sample_cp();
    assert!(resolve_method_ref(&cp, 999).is_none());
}

#[test]
fn evict_vm_removes_only_matching_vm_entries() {
    let vm_a: usize = 0xDEAD_BEEF_0001;
    let vm_b: usize = 0xDEAD_BEEF_0002;
    let cid_a = ClassId::new(11);
    let cid_b = ClassId::new(22);

    {
        let mut guard = cache().write();
        guard.insert((vm_a, cid_a, 1), true);
        guard.insert((vm_a, cid_b, 2), false);
        guard.insert((vm_b, cid_a, 1), true);
    }

    evict_vm(vm_a);

    let guard = cache().read();
    assert!(!guard.contains_key(&(vm_a, cid_a, 1)));
    assert!(!guard.contains_key(&(vm_a, cid_b, 2)));
    assert_eq!(guard.get(&(vm_b, cid_a, 1)).copied(), Some(true));
    drop(guard);

    evict_vm(vm_b);
    assert!(!cache().read().contains_key(&(vm_b, cid_a, 1)));
}
