use super::*;

#[test]
fn compiles_the_juli_pattern() {
    let p = compile_pattern("dd-MMM-yyyy HH:mm:ss").expect("supported");
    assert_eq!(
        p,
        vec![
            Piece::Field {
                letter: 'd',
                count: 2
            },
            Piece::Literal("-".into()),
            Piece::Field {
                letter: 'M',
                count: 3
            },
            Piece::Literal("-".into()),
            Piece::Field {
                letter: 'y',
                count: 4
            },
            Piece::Literal(" ".into()),
            Piece::Field {
                letter: 'H',
                count: 2
            },
            Piece::Literal(":".into()),
            Piece::Field {
                letter: 'm',
                count: 2
            },
            Piece::Literal(":".into()),
            Piece::Field {
                letter: 's',
                count: 2
            },
        ]
    );
}

#[test]
fn quoting_matches_the_jdk_rules() {
    assert_eq!(
        compile_pattern("'at' HH''mm").unwrap(),
        vec![
            Piece::Literal("at ".into()),
            Piece::Field {
                letter: 'H',
                count: 2
            },
            Piece::Literal("'".into()),
            Piece::Field {
                letter: 'm',
                count: 2
            },
        ]
    );
    // An unterminated quote is an IllegalArgumentException in the JDK — we
    // decline so the bytecode raises it.
    assert!(compile_pattern("HH'mm").is_none());
}

#[test]
fn declines_unsupported_and_unknown_letters() {
    assert!(compile_pattern("yyyy-MM-dd z").is_some());
    for p in ["yyyy 'w' w", "LLLL", "YYYY", "u", "F"] {
        assert!(compile_pattern(p).is_none(), "{p} must decline");
    }
    // Not a pattern letter at all: the JDK throws, so we decline too.
    assert!(compile_pattern("qqq").is_none());
}

#[test]
fn zero_padding_matches_both_jdk_arms() {
    let mut s = String::new();
    zero_padding_number(&mut s, 5, 2, usize::MAX);
    assert_eq!(s, "05");
    s.clear();
    zero_padding_number(&mut s, 5, 1, usize::MAX);
    assert_eq!(s, "5");
    s.clear();
    zero_padding_number(&mut s, 2023, 4, usize::MAX);
    assert_eq!(s, "2023");
    s.clear();
    // `yy` clips 2023 to 23 — min 2, max 2.
    zero_padding_number(&mut s, 2023, 2, 2);
    assert_eq!(s, "23");
    s.clear();
    // A year wider than the count is NOT truncated when max is unbounded.
    zero_padding_number(&mut s, 12345, 4, usize::MAX);
    assert_eq!(s, "12345");
    s.clear();
    zero_padding_number(&mut s, 7, 3, usize::MAX);
    assert_eq!(s, "007");
}

#[test]
fn civil_dates_match_known_points() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(-1), (1969, 12, 31));
    // 2023-11-14, the fixed instant the probes use.
    assert_eq!(civil_from_days(19675), (2023, 11, 14));
    // Leap day.
    assert_eq!(civil_from_days(19782), (2024, 2, 29));
}

#[test]
fn day_of_week_anchors_on_a_known_thursday() {
    // 1970-01-01 was a Thursday; Calendar.THURSDAY == 5.
    assert_eq!(fields_from_local_millis(0).day_of_week, 5);
    // …so the following Sunday is 1.
    assert_eq!(fields_from_local_millis(3 * MS_PER_DAY).day_of_week, 1);
}

#[test]
fn day_of_year_handles_leap_years() {
    assert_eq!(day_of_year(2023, 1, 1), 1);
    assert_eq!(day_of_year(2023, 12, 31), 365);
    assert_eq!(day_of_year(2024, 12, 31), 366);
    assert_eq!(day_of_year(2024, 3, 1), 61);
}

#[test]
fn fields_split_the_day_correctly() {
    let f = fields_from_local_millis(1_700_000_000_000 - 8 * 3_600_000);
    assert_eq!((f.year, f.month0 + 1, f.day), (2023, 11, 14));
    assert_eq!((f.hour_of_day, f.minute, f.second), (14, 13, 20));
    assert_eq!(f.millis, 0);
}

/// gc-common w19-g: the plan cache is bounded. A new formatter identity
/// past the bound starts a fresh map; an existing one never clears it.
/// (Runs on a local map, not the process-wide one, so it is independent
/// of every other test in the binary.)
#[test]
fn w19g_plan_cache_rows_are_bounded() {
    let mut cache: FxHashMap<PlanKey, Entry> = FxHashMap::default();
    for id in 0..PLAN_CACHE_MAX_ROWS as i32 {
        plan_cache_row(&mut cache, (1, id))
            .verified
            .insert((0, 0, 0, 0, false, false, false));
    }
    assert_eq!(cache.len(), PLAN_CACHE_MAX_ROWS);
    // A row already present is returned as it is, without a clear.
    assert!(!plan_cache_row(&mut cache, (1, 7)).verified.is_empty());
    assert_eq!(cache.len(), PLAN_CACHE_MAX_ROWS);
    // One more identity: the map starts over with just that row, empty.
    let fresh = plan_cache_row(&mut cache, (1, PLAN_CACHE_MAX_ROWS as i32));
    assert!(fresh.plan.is_none() && !fresh.poisoned && fresh.verified.is_empty());
    assert_eq!(cache.len(), 1);
}

// ---- gc-common w34-a: plans and zone memos name OBJECTS, not hashes ----
// (`common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`
// rows 1 and 2). All on local maps / private VM identities.

const W34A_SHAPE: ShapeKey = (10, 3, 0, 1, false, false, false);

fn w34a_stamp(pattern_addr: usize, symbols_addr: usize, epoch: u64) -> PlanStamp {
    PlanStamp {
        pattern_addr,
        symbols_addr,
        epoch,
    }
}

fn w34a_symbols(january: &str) -> Symbols {
    Symbols {
        months: vec![january.to_string()],
        ..Default::default()
    }
}

/// Row 1: `applyPattern(new String("dd/MM/yyyy"))` on a formatter whose
/// plan is built and one shape verified. The new pattern String sits 4 GiB
/// from the old one -- the mock's same-hash pair, and under the 8-byte
/// header a real one every 2^20 mints. The old code compared the two
/// hashes, found them equal and kept rendering "yyyy-MM-dd" unchecked;
/// the stamp names the object, so it is a miss, the row is re-proven by
/// content, and the new plan starts with nothing verified.
#[test]
fn w34a_a_replaced_pattern_misses_even_with_a_colliding_hash() {
    let mut cache: FxHashMap<PlanKey, Entry> = FxHashMap::default();
    let key: PlanKey = (0x34A0_0001, 7);
    let old = w34a_stamp(0x1_34A0_1000, 0x1_34A0_2000, 3);
    plan_row_refresh(
        &mut cache,
        key,
        old,
        "yyyy-MM-dd".into(),
        w34a_symbols("January"),
    );
    assert!(
        plan_row_note_shape(&mut cache, key, old, W34A_SHAPE),
        "first sight: check it"
    );
    assert!(
        !plan_row_note_shape(&mut cache, key, old, W34A_SHAPE),
        "then verified"
    );
    assert_eq!(
        plan_row_current(&mut cache, key, old).unwrap().1,
        "yyyy-MM-dd"
    );

    let replaced = w34a_stamp(0x2_34A0_1000, 0x1_34A0_2000, 3);
    assert_eq!(
        replaced.pattern_addr as i32, old.pattern_addr as i32,
        "premise: the mock would hash the two pattern Strings alike"
    );
    assert!(
        plan_row_current(&mut cache, key, replaced).is_none(),
        "another pattern object is a miss, whatever its hash"
    );
    plan_row_refresh(
        &mut cache,
        key,
        replaced,
        "dd/MM/yyyy".into(),
        w34a_symbols("January"),
    );
    let (pieces, pattern, _) = plan_row_current(&mut cache, key, replaced).unwrap();
    assert_eq!(pattern, "dd/MM/yyyy");
    assert_eq!(*pieces, compile_pattern("dd/MM/yyyy").unwrap());
    assert!(
        plan_row_note_shape(&mut cache, key, replaced, W34A_SHAPE),
        "the new plan's shapes are unverified: cross-check again"
    );
    assert!(
        plan_row_verified(&cache, key, old).is_none(),
        "the old stamp names no plan"
    );
    assert!(plan_row_verified(&cache, key, replaced).is_some());
}

/// A collection moves the pattern and the symbols (new addresses, new
/// epoch): a miss, re-proven by CONTENT. Equal text and symbol strings
/// keep the plan and its verified shapes -- one decode per formatter per
/// collection, no re-verification against the bytecode. A symbols clone
/// with a different month name rebuilds it with nothing verified.
#[test]
fn w34a_a_a_collection_re_proves_the_plan_by_content() {
    let mut cache: FxHashMap<PlanKey, Entry> = FxHashMap::default();
    let key: PlanKey = (0x34A0_0003, 9);
    let before = w34a_stamp(0x1_34A0_3000, 0x1_34A0_4000, 5);
    plan_row_refresh(
        &mut cache,
        key,
        before,
        "HH:mm".into(),
        w34a_symbols("January"),
    );
    assert!(plan_row_note_shape(&mut cache, key, before, W34A_SHAPE));

    let moved = w34a_stamp(0x1_34A0_5000, 0x1_34A0_6000, 6);
    assert!(
        plan_row_current(&mut cache, key, moved).is_none(),
        "a new epoch is a miss"
    );
    plan_row_refresh(
        &mut cache,
        key,
        moved,
        "HH:mm".into(),
        w34a_symbols("January"),
    );
    assert!(plan_row_current(&mut cache, key, moved).is_some());
    assert!(
        !plan_row_note_shape(&mut cache, key, moved, W34A_SHAPE),
        "same content: the verified shape survives the collection"
    );

    let reclone = w34a_stamp(0x1_34A0_7000, 0x1_34A0_8000, 6);
    plan_row_refresh(
        &mut cache,
        key,
        reclone,
        "HH:mm".into(),
        w34a_symbols("Janvier"),
    );
    assert_eq!(
        *plan_row_current(&mut cache, key, reclone).unwrap().2,
        w34a_symbols("Janvier")
    );
    assert!(
        plan_row_note_shape(&mut cache, key, reclone, W34A_SHAPE),
        "different symbols: nothing verified"
    );
}

/// The thread-local memo's key is exact: any other formatter, pattern,
/// symbols object, epoch or VM is another key.
#[test]
fn w34a_a_last_plan_key_names_objects() {
    let base = LastPlanKey {
        vm: 0x34A0_0004,
        this_addr: 0x1_34A0_9000,
        stamp: w34a_stamp(0x1_34A0_A000, 0x1_34A0_B000, 2),
    };
    assert_eq!(base, base);
    let mut other = base;
    other.this_addr = 0x2_34A0_9000;
    assert_ne!(base, other, "a colliding formatter");
    let mut other = base;
    other.stamp.pattern_addr = 0x2_34A0_A000;
    assert_ne!(base, other, "a colliding pattern String");
    let mut other = base;
    other.stamp.symbols_addr = 0x2_34A0_B000;
    assert_ne!(base, other, "a colliding symbols object");
    let mut other = base;
    other.stamp.epoch = 3;
    assert_ne!(base, other, "a collection since");
    let mut other = base;
    other.vm = 0x34A0_0005;
    assert_ne!(base, other, "another VM");
}

mod w34a_zone_memo {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    thread_local! {
        static RESOLVES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// Counts catalog lookups and resolves nothing (the real catalog is
    /// a process-wide `OnceLock` a mock must not initialise).
    fn counting_resolver(
        _ctx: &mut dyn NativeContext,
        _id: &str,
    ) -> Option<std::sync::Arc<crate::tzdb::ZoneRulesData>> {
        RESOLVES.with(|c| c.set(c.get() + 1));
        None
    }

    fn resolves() -> u32 {
        RESOLVES.with(|c| c.get())
    }

    /// Row 2: `setID` installs a new id String. The memo is keyed by that
    /// String's address and epoch, so a new String is re-read (the old
    /// code compared the two Strings' HASHES, and a collision served the
    /// old zone's id and rules); an equal-text copy keeps the rules it
    /// already resolved; the same String again is a hit.
    #[test]
    fn w34a_b_a_replaced_zone_id_is_re_read() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x34A0_0006);
        ZONE_RULES.with(|c| *c.borrow_mut() = None);
        RESOLVES.with(|c| c.set(0));

        let paris = ctx.create_string("Europe/Paris");
        let (_, id) = zone_rules_for_id(&mut ctx, paris, counting_resolver).unwrap();
        assert_eq!(&*id, "Europe/Paris");
        assert_eq!(resolves(), 1);
        let (_, id) = zone_rules_for_id(&mut ctx, paris, counting_resolver).unwrap();
        assert_eq!(&*id, "Europe/Paris");
        assert_eq!(resolves(), 1, "the same String: a hit");

        let new_york = ctx.create_string("America/New_York");
        let (_, id) = zone_rules_for_id(&mut ctx, new_york, counting_resolver).unwrap();
        assert_eq!(&*id, "America/New_York", "a new id String is re-read");
        assert_eq!(resolves(), 2);

        let copy = ctx.create_string("America/New_York");
        let (_, id) = zone_rules_for_id(&mut ctx, copy, counting_resolver).unwrap();
        assert_eq!(&*id, "America/New_York");
        assert_eq!(resolves(), 2, "equal text keeps the resolved rules");
        ZONE_RULES.with(|c| *c.borrow_mut() = None);
    }
}
