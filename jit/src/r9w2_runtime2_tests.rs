    use super::*;
    use std::sync::atomic::Ordering;

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    // ── mic-empty-slot-class-zero-window ────────────────────────────────

    /// A fresh MIC slot holds a class id no receiver can have. `ClassId(0)`
    /// is `java/lang/Object`; with `0` as the empty value a class-0 receiver
    /// matched an empty slot's guard.
    #[test]
    fn a_fresh_mic_slot_holds_the_empty_sentinel_not_class_zero() {
        for mic in [
            JitMICSlot::new(),
            JitMICSlot::new_at(7),
            JitMICSlot::default(),
        ] {
            let cid = mic.cached_class_id.load(Ordering::Relaxed);
            assert_eq!(cid, JitMICSlot::EMPTY_CLASS_ID);
            assert_ne!(cid, 0, "class 0 would match an empty slot's guard");
            assert!(JitMICSlot::is_empty_class_id(cid));
            assert_eq!(mic.cached_entry(), (0, false));
        }
        assert!(JitMICSlot::is_empty_class_id(0), "0 is still read as empty");
        assert!(!JitMICSlot::is_empty_class_id(1));
        assert_ne!(JitMICSlot::EMPTY_CLASS_ID, JitMICSlot::INSTALLING_CLASS_ID);
        assert_ne!(JitMICSlot::EMPTY_CLASS_ID, JitPICSlot::RETIRED_WAY_CLASS_ID);
    }

    /// `update` still publishes into a fresh slot, into one some other path
    /// zeroed, and for a class-0 receiver.
    #[test]
    fn update_publishes_from_either_empty_value() {
        let fresh = JitMICSlot::new();
        fresh.update(5, "A", 0xDEAD_BEE0, true, false);
        assert_eq!(fresh.cached_class_id.load(Ordering::Acquire), 5);
        assert_eq!(fresh.cached_entry(), (0xDEAD_BEE0, true));

        let zeroed = JitMICSlot::new();
        zeroed.cached_class_id.store(0, Ordering::Relaxed);
        zeroed.update(6, "B", 0xDEAD_BEE0, false, false);
        assert_eq!(zeroed.cached_class_id.load(Ordering::Acquire), 6);
        assert_eq!(zeroed.cached_entry(), (0xDEAD_BEE0, false));

        let object = JitMICSlot::new();
        object.update(0, "java/lang/Object", 0xDEAD_BEE0, false, false);
        assert_eq!(object.cached_class_id.load(Ordering::Acquire), 0);
        assert_eq!(object.cached_entry(), (0xDEAD_BEE0, false));
        // Monomorphic for life: a second class does not retarget it.
        object.update(9, "C", 0xBEEF_0000, false, false);
        assert_eq!(object.cached_class_id.load(Ordering::Acquire), 0);
    }

    /// Seeding the guard with class 0 would re-open the window the sentinel
    /// closes, so it seeds the sentinel instead; the first `update` still
    /// installs the target for class 0.
    #[test]
    fn prepopulating_class_zero_leaves_the_slot_empty() {
        let mic = JitMICSlot::new();
        mic.prepopulate(0);
        assert_eq!(
            mic.cached_class_id.load(Ordering::Relaxed),
            JitMICSlot::EMPTY_CLASS_ID
        );
        mic.update(0, "java/lang/Object", 0xDEAD_BEE0, false, false);
        assert_eq!(mic.cached_class_id.load(Ordering::Acquire), 0);
        assert_eq!(mic.cached_entry(), (0xDEAD_BEE0, false));

        let seeded = JitMICSlot::new();
        seeded.prepopulate(42);
        assert_eq!(seeded.cached_class_id.load(Ordering::Relaxed), 42);
    }

    // ── string-indexof-char-screen-belongs-at-the-resolver ─────────────

    fn index_of_char() -> usize {
        JitIntrinsic::StringIndexOfChar.as_entry()
    }

    #[test]
    fn a_constant_bmp_needle_is_admitted() {
        // aload_1; bipush 97; invokevirtual #2
        let code = [0x2b, 0x10, 0x61, 0xb6, 0x00, 0x02, 0xac];
        assert!(string_index_of_char_site_admitted(
            index_of_char(),
            &code,
            code.len(),
            3
        ));
        // aload_1; sipush 0x7fff; invokevirtual #2
        let code = [0x2b, 0x11, 0x7f, 0xff, 0xb6, 0x00, 0x02, 0xac];
        assert!(string_index_of_char_site_admitted(
            index_of_char(),
            &code,
            code.len(),
            4
        ));
    }

/// `i2c` and `caload` produce a zero-extended `char`, so the needle is in
    /// range whatever it was.
    #[test]
    fn a_char_typed_needle_is_admitted() {
        // aload_1; iload_2; i2c; invokevirtual #2
        let code = [0x2b, 0x1c, 0x92, 0xb6, 0x00, 0x02, 0xac];
        assert!(string_index_of_char_site_admitted(
            index_of_char(),
            &code,
            code.len(),
            3
        ));
        // aload_1; aload_2; iconst_0; caload; invokevirtual #2
        let code = [0x2b, 0x2c, 0x03, 0x34, 0xb6, 0x00, 0x02, 0xac];
        assert!(string_index_of_char_site_admitted(
            index_of_char(),
            &code,
            code.len(),
            4
        ));
    }

    /// javac's `s.indexOf(f ? ch : 'a')`: the linear predecessor of the
    /// invoke is the constant, but the `goto` edge delivers the variable.
    #[test]
    fn a_join_at_the_invoke_is_declined_even_after_a_constant() {
        //  0: aload_1
        //  1: iload_3
        //  2: ifeq -> 9
        //  5: iload_2
        //  6: goto -> 11
        //  9: bipush 97
        // 11: invokevirtual #2
        // 14: ireturn
        let code = [
            0x2b, 0x1d, 0x99, 0x00, 0x07, 0x1c, 0xa7, 0x00, 0x05, 0x10, 0x61, 0xb6, 0x00, 0x02,
            0xac,
        ];
        assert_eq!(
            crate::x64::prev_insn_int_const_for_test(&code, code.len(), 11),
            Some(97),
            "the linear walk alone sees a constant here"
        );
        assert!(!string_index_of_char_site_admitted(
            index_of_char(),
            &code,
            code.len(),
            11
        ));
    }

    // ── lambda-adapter-built-for-retiring-impl ─────────────────────────

    /// A thunk built after the invalidation's first adapter sweep but before
    /// its `retired` mark passes both of `lambda_adapter_entry`'s checks. The
    /// second sweep must withdraw it and hand its entry to the inline-cache
    /// pass.
    #[test]
    fn a_thunk_built_between_the_two_sweeps_is_withdrawn_by_the_second() {
        let owner = Arc::new(ret_body());
        let impl_entry = owner.entry_ptr() as usize;
        let mut remove: std::collections::HashSet<usize> = [impl_entry].into_iter().collect();
        let adapters = crate::lambda_adapter::LambdaAdapterMap::new();
        // First sweep: nothing reaches the impl yet.
        assert!(crate::lambda_adapter::adapters_reaching(&adapters, &remove).is_empty());
        // A dispatch builds a thunk for it: the impl is not marked yet.
        let thunk =
            crate::lambda_adapter::lambda_adapter_entry(&adapters, 0x8000_0A01, &owner, b"", 1)
                .expect("a no-capture thunk is emitted");
        // The invalidation's mark.
        owner.retired.store(true, Ordering::SeqCst);
        let before = late_lambda_adapters_withdrawn();
        withdraw_late_lambda_adapters(&adapters, &mut remove);
        assert!(
            remove.contains(&thunk),
            "the late thunk must reach the inline-cache pass"
        );
        let only_impl: std::collections::HashSet<usize> = [impl_entry].into_iter().collect();
        assert!(
            crate::lambda_adapter::adapters_reaching(&adapters, &only_impl).is_empty(),
            "the late thunk must be forgotten"
        );
        assert!(late_lambda_adapters_withdrawn() > before);
        // With nothing late, the sweep is a no-op.
        let len = remove.len();
        withdraw_late_lambda_adapters(&adapters, &mut remove);
        assert_eq!(remove.len(), len);
    }

    // ── executable-buffer-finalize-aborts-the-process-on-protect-failure ─

    #[test]
    fn a_refused_protect_declines_the_artifact_instead_of_aborting() {
        let mut buf = ExecutableBuffer::new(64).expect("alloc");
        buf.emit(&[0xC3]);
        let before = code_buffer_protect_failures();
        crate::platform::fail_next_make_executable_for_test(13); // EACCES
        assert!(CompiledMethod::try_new_with_context(buf).is_none());
        assert!(code_buffer_protect_failures() > before);
        // EACCES is a policy denial, but an injected one in a shared test
        // process must not latch "JIT unavailable" (r9 wave 4): every later
        // compile test in this binary would be refused.
        assert!(!jit_unavailable_by_policy());
        assert!(!protect_failure_latches_jit_unavailable(
            &crate::platform::JitError::ProtectFailed(13)
        ));

        let mut buf = ExecutableBuffer::new(64).expect("alloc");
        buf.emit(&[0xC3]);
        let cm = CompiledMethod::try_new_with_context(buf).expect("an unrefused flip publishes");
        assert!(cm.needs_context());
        let mut buf = ExecutableBuffer::new(64).expect("alloc");
        buf.emit(&[0xC3]);
        assert!(!CompiledMethod::try_new(buf)
            .expect("publishes")
            .needs_context());
    }

    // ── direct-helper-engagement-counters-pay-a-shared-lock-xadd-per-call ─

    /// Exact across live threads, across exited ones (folded once, not lost
    /// and not doubled), and on the reading thread. Nothing in this crate
    /// increments `SCOPED_MEMORY_DECLINED` (only the VM's helpers do), so the
    /// delta is this test's own.
    #[test]
    fn per_thread_event_counters_are_exact_across_live_and_exited_threads() {
        let counter = &SCOPED_MEMORY_DECLINED;
        let before = counter.load(Ordering::Relaxed);
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        SCOPED_MEMORY_DECLINED.fetch_add(1, Ordering::Relaxed);
                    }
                    barrier.wait(); // counted, still alive
                    barrier.wait(); // released to exit
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            counter.load(Ordering::Relaxed) - before,
            4000,
            "live threads"
        );
        barrier.wait();
        for worker in workers {
            worker.join().expect("worker");
        }
        assert_eq!(
            counter.load(Ordering::Relaxed) - before,
            4000,
            "exited threads"
        );
        counter.fetch_add(3, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed) - before,
            4003,
            "this thread"
        );
    }

    // ── jit-cache-reader-guard-makes-the-last-drop-unauthorised ────────

    /// The shard maps hold `RetainedCode`; the cache's API still hands out
    /// the same `Arc`s, and a superseded body leaves the map.
    #[test]
    fn the_cache_serves_bodies_through_retained_code() {
        let cache = JitCache::new();
        let class: Arc<str> = Arc::from("r9w2/RetainedCodeMap");
        let method: Arc<str> = Arc::from("m");
        let desc: Arc<str> = Arc::from("()V");
        let cid = cratonvm_types::ClassId::new(1);
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        // Held as `RetainedCode`, as a real reader should: this test's own last
        // drops then go through the retirement queue too.
        let first = RetainedCode::new(cache.get(&class, &method, &desc, cid).expect("published"));
        assert!(
            Arc::strong_count(first.arc()) >= 2,
            "the map still owns its reference"
        );
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        let second =
            RetainedCode::new(cache.get(&class, &method, &desc, cid).expect("republished"));
        assert_ne!(first.artifact_id, second.artifact_id);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.clear_all(), 1);
        assert!(cache.get(&class, &method, &desc, cid).is_none());
    }

    // ── ea request 3: protected `arraylength` ──────────────────────────

    #[cfg(target_arch = "x86_64")]
    fn whole_body(len: u16) -> [cratonvm_reader::attribute::ExceptionTableEntry; 1] {
        [cratonvm_reader::attribute::ExceptionTableEntry {
            start_pc: 0,
            end_pc: len,
            handler_pc: len,
            catch_type: 0,
        }]
    }

    /// A protected `arraylength` publishes its NPE (`emit_precise_array_npe_
    /// check`), so a straight-line one no longer refuses the compile.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn a_straight_line_protected_arraylength_is_admitted() {
        // aload_0; arraylength; pop; nop
        let code = [0x2au8, 0xbe, 0x57, 0x00];
        assert_eq!(
            first_unsupported_precise_frame_site(&code, code.len(), &whole_body(4)),
            None
        );
        // invokestatic #0; arraylength; pop; goto 0 -- in a loop, but not the
        // `aload; arraylength` shape the length hoist can move.
        let code = [0xb8u8, 0x00, 0x00, 0xbe, 0x57, 0xa7, 0xff, 0xfb];
        assert_eq!(
            first_unsupported_precise_frame_site(&code, code.len(), &whole_body(8)),
            None
        );
    }

    /// The loop-invariant length hoist could move `aload; arraylength` into a
    /// pre-header whose null check is frame-less, but under precise frames
    /// `x64/driver.rs` drops any hoist with a protected site, so that shape
    /// inside a loop is admitted too.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn a_protected_arraylength_in_a_loop_is_admitted() {
        // 0: aload_0; 1: arraylength; 2: pop; 3: goto 0
        let code = [0x2au8, 0xbe, 0x57, 0xa7, 0xff, 0xfd];
        assert_eq!(
            first_unsupported_precise_frame_site(&code, code.len(), &whole_body(6)),
            None
        );
    }
