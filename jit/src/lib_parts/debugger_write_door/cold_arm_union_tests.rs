    use super::*;

    // JDK 25 `AccessibleObject.verifyAccess(Class,Class,Class,int)Z`.
    const VERIFY_ACCESS: [u8; 55] = [
        0x2b, 0x2c, 0xa6, 0x00, 0x05, 0x04, 0xac, // 0: caller == memberClass -> true
        0x2d, 0xc6, 0x00, 0x1b, // 7: targetClass == null -> 35
        0x15, 0x04, 0xb8, 0x00, 0x5a, 0x99, 0x00, 0x13, // 11: !isProtected -> 35
        0x2d, 0x2c, 0xa5, 0x00, 0x0e, // 19: targetClass == memberClass -> 35
        0x2a, 0x2b, 0x2d, 0xb6, 0x00, 0xe4, 0x99, 0x00, 0x0f, 0x04, 0xac, // 24: protected cache
        0x2a, 0x2b, 0xb6, 0x00, 0xe7, 0x99, 0x00, 0x05, 0x04, 0xac, // 35: isAccessChecked(caller)
        0x2a, 0x2b, 0x2c, 0x2d, 0x15, 0x04, 0xb6, 0x00, 0xe9, 0xac, // 45: slowVerifyAccess
    ];

    fn public_member_profile(misses: u32) -> profile::MethodProfile {
        let n = 100_000;
        let mut p = profile::MethodProfile::default();
        let mut row = |pc: usize, taken: u32, not_taken: u32| {
            p.branches.insert(pc, profile::BranchCounts { taken, not_taken });
        };
        row(2, n, 0); // caller != memberClass
        row(8, 0, n); // an instance member
        row(16, n, 0); // not protected
        row(40, misses, n - misses); // the access cache misses
        p
    }

    #[test]
    fn the_slow_path_reached_by_two_rare_edges_is_priced() {
        let rows = splice_cold_arm_ranges(&public_member_profile(1), &VERIFY_ACCESS, 55);
        let at = |pc: usize| rows.iter().filter(|&&(s, e, _)| s <= pc && pc < e).map(|r| r.2).min();
        // The `slowVerifyAccess` call: the four rare edges' floors together.
        assert_eq!(at(51), Some(4 * ir_evidence::COLD_ARM_WEIGHT_FLOOR_PERMILLE));
        // The protected-member arm too; the hot path stays unpriced.
        assert!(at(27).is_some());
        for hot in [0, 7, 16, 35, 37, 40, 43] {
            assert_eq!(at(hot), None, "pc {hot}");
        }
    }

    #[test]
    fn a_rare_fall_through_arm_is_priced() {
        // 0: iload_1  1: ifne 9  4: aload_0  5: invokevirtual #1  8: athrow  9: return
        const CHECK: [u8; 10] = [0x1b, 0x9a, 0x00, 0x08, 0x2a, 0xb6, 0x00, 0x01, 0xbf, 0xb1];
        let mut p = profile::MethodProfile::default();
        p.branches.insert(1, profile::BranchCounts { taken: 100_000, not_taken: 0 });
        let rows = splice_cold_arm_ranges(&p, &CHECK, 10);
        let at = |pc: usize| rows.iter().filter(|&&(s, e, _)| s <= pc && pc < e).map(|r| r.2).min();
        assert_eq!(at(5), Some(ir_evidence::COLD_ARM_WEIGHT_FLOOR_PERMILLE));
        assert_eq!(at(9), None);
    }

    #[test]
    fn a_slow_path_that_is_not_rare_stays_unpriced() {
        // Every fifth call misses: the miss edge is not rare, so the call it
        // reaches is charged in full.
        let rows = splice_cold_arm_ranges(&public_member_profile(20_000), &VERIFY_ACCESS, 55);
        assert!(!rows.iter().any(|&(s, e, _)| s <= 51 && 51 < e));
    }
