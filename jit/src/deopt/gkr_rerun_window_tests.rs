    use super::rerun_window_closed_at;

    /// `static Object f() { return g(new Object[] { null }); }`: `iconst_1;
    /// anewarray #1; dup; iconst_0; aconst_null; aastore; invokestatic #2;
    /// areturn`. The `aastore` commits into an array the prefix allocated, so
    /// the owned-arguments rule re-runs the caller from entry at the
    /// `invokestatic` (bci 8): the window must stay open there, or a GC frees
    /// the argument copies that re-run reads.
    #[test]
    fn an_owned_args_fresh_array_call_keeps_the_window_open() {
        let code = [0x04, 0xbd, 0, 1, 0x59, 0x03, 0x01, 0x53, 0xb8, 0, 2, 0xb0];
        let closed = rerun_window_closed_at(&code, code.len(), &[], &[]).expect("analysed");
        assert!(!closed[8], "the owned-args re-run point is open");
        assert!(!closed[0] && !closed[7], "everything before it reaches it");
        assert!(closed[11], "after the call the window is closed");
    }

    /// `static boolean drop(Object x) { sink = x; x = null; return f(); }`:
    /// `aload_0; putstatic #1; aconst_null; astore_0; invokestatic #2;
    /// ireturn`. The re-run is exact up to the `putstatic` (bci 1, which has
    /// not run yet when a trap there re-executes it) and never after it.
/// A loop whose body commits (`putstatic`) keeps an exact point only before
    /// it: `iconst_0; istore_1; iload_1; bipush 9; if_icmpge +13; iconst_1;
    /// putstatic #1; iinc 1 1; goto -13; iconst_0; ireturn`. A point at the
    /// loop head may be a later iteration, so it is closed, and so is
    /// everything after the loop; bci 0-1 still precede every commit.
    #[test]
    fn a_committing_loop_closes_the_window_at_its_head() {
        let code = [
            0x03, 0x3c, // 0: iconst_0; 1: istore_1
            0x1b, 0x10, 9, // 2: iload_1; 3: bipush 9
            0xa2, 0, 13, // 5: if_icmpge -> 18
            0x04, 0xb3, 0, 1, // 8: iconst_1; 9: putstatic #1
            0x84, 1, 1, // 12: iinc 1 1
            0xa7, 0xff, 0xf3, // 15: goto -> 2
            0x03, 0xac, // 18: iconst_0; 19: ireturn
        ];
        let closed = rerun_window_closed_at(&code, code.len(), &[], &[]).expect("analysed");
        assert!(!closed[0] && !closed[1], "before the loop nothing has committed");
        for p in [2, 3, 5, 8, 9, 12, 15, 18, 19] {
            assert!(closed[p], "bci {p} is in or after the committing loop");
        }
        // An IR site trap at the loop head fires on its FIRST arrival, before
        // any iteration ran: exact there, so the whole loop reaches it.
        let site = rerun_window_closed_at(&code, code.len(), &[], &[2]).expect("analysed");
        for p in [2, 3, 5, 8, 9, 12, 15] {
            assert!(!site[p], "bci {p} can still reach the first-arrival point");
        }
        assert!(site[18] && site[19], "past the loop it cannot");
    }

    /// A method that commits nothing re-runs exactly from anywhere, and the
    /// refusals answer `None`.
    #[test]
    fn a_pure_method_never_closes_and_refusals_claim_nothing() {
        let pure = [0x1a, 0x04, 0x60, 0xac]; // iload_0; iconst_1; iadd; ireturn
        let closed = rerun_window_closed_at(&pure, pure.len(), &[], &[]).expect("analysed");
        assert!(closed.iter().all(|&c| !c));
        assert_eq!(rerun_window_closed_at(&[], 0, &[], &[]), None);
        let jsr = [0xa8, 0, 3, 0xb1]; // jsr +3; return
        assert_eq!(rerun_window_closed_at(&jsr, jsr.len(), &[], &[]), None);
    }

    /// A handler is reachable from its range: `iconst_1; putstatic #1;
    /// return` with a row covering bci 0-4 whose handler at 5 is
    /// `iconst_0; pop; return` (commits nothing past the range). Bci 5 is a
    /// point after the putstatic in program order, so the handler's own
    /// prefix includes the commit: closed. Without the row bci 5 would be
    /// unreachable code, and nothing reaches it.
    #[test]
    fn handlers_are_followed() {
        let code = [0x04, 0xb3, 0, 1, 0xb1, 0x03, 0x57, 0xb1];
        let closed =
            rerun_window_closed_at(&code, code.len(), &[(0, 5, 5)], &[]).expect("analysed");
        assert!(!closed[0] && !closed[1]);
        assert!(closed[4] && closed[5] && closed[6] && closed[7]);
    }
