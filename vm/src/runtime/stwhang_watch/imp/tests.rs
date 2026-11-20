    /// gci i3/hd: the snapshot captures a parked thread (context, a stack
    /// copy holding its `rsp` word) and RESUMES it: the thread then wakes
    /// and exits, so the join returns.
    #[test]
    fn gci_i3hd_snapshot_captures_and_resumes_a_parked_thread() {
        let (tid_tx, tid_rx) = std::sync::mpsc::channel::<u32>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            // SAFETY: a documented Win32 call with no arguments.
            tid_tx.send(unsafe { super::GetCurrentThreadId() }).unwrap();
            go_rx.recv().unwrap();
            7u32
        });
        let tid = tid_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // SAFETY: `tid` is a live thread of this process that is not the
        // caller.
        let (caps, span_us) =
            unsafe { super::snapshot(&[tid], std::time::Instant::now()) };
        assert_eq!(caps.len(), 1);
        let c = &caps[0];
        assert_eq!(c.tid, tid);
        assert!(c.ctx_ok, "context read while suspended");
        assert!(c.rsp != 0 && c.rip != 0);
        assert!(c.copied >= 8, "the page holding rsp is readable");
        assert!(super::super::copied_word(c.base, &c.stack[..c.copied], c.base).is_some());
        assert!(c.at_us <= span_us.saturating_add(1_000_000));
        drop(caps);
        go_tx.send(()).unwrap();
        assert_eq!(t.join().unwrap(), 7, "the thread was resumed");
    }
