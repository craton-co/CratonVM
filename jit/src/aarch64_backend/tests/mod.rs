use super::*;
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
        assert_eq!(cm.oop_maps.len(), 1, "one allocation, one map");
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
        let cm =
            compile_with_dispatch("(II)I", 2, &[0x1a, 0x1b, 0xb8, 0x00, 0x01, 0xac], 2, b'I');
        reset_dispatch();
        DISP_RESULT.store(4242, Ordering::SeqCst);
        let vm = 0x5555_0000i64;
        // SAFETY: a context-taking static `(II)I`, entry
        // `fn(i64, i64, i64) -> i64`.
        let got = unsafe { cm.try_call_with_context(vm, &[7, 9]) }.expect("callable");
        assert_eq!(DISP_CALLS.load(Ordering::SeqCst), 1);
        // Cast: the context pointer and the site's info pointer.
        assert_eq!(
            DISP_VM.load(Ordering::SeqCst),
            vm as u64,
            "the VM pointer first"
        );
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
        let cm =
            compile_with_dispatch("(JF)J", 4, &[0x1e, 0x24, 0xb8, 0x00, 0x01, 0xad], 2, b'J');
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
        let got =
            unsafe { cm.try_call_with_context(0x5555_0000, &[11, 22, 33]) }.expect("callable");
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
        let cm =
            compile_with_dispatch("()J", 2, &[0xb8, 0x00, 0x01, 0x0a, 0x61, 0xad], 0, b'J');

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
        assert_eq!(
            got,
            i64::MIN,
            "a pending signal leaves through the epilogue"
        );
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
        let got =
            unsafe { cm.try_call_with_context(0x8888_0000, &[0x4000_1000]) }.expect("callable");
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
        let cm =
            compile_with_typecheck("(Ljava/lang/Object;)I", &[0x2a, 0xc1, 0x00, 0x01, 0xac]);
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
        let got =
            unsafe { cm.try_call_with_context(0x8888_0000, &[0x4000_1000]) }.expect("callable");
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

mod every_gpr_encoding_converts_and;
#[allow(unused_imports)]
pub use every_gpr_encoding_converts_and::*;
#[allow(unused_imports)]
use every_gpr_encoding_converts_and::*;
mod a_frame_homed_reference_local_is;
#[allow(unused_imports)]
pub use a_frame_homed_reference_local_is::*;
#[allow(unused_imports)]
use a_frame_homed_reference_local_is::*;
