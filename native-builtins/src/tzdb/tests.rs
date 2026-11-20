use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn test_tzdb_path() -> std::path::PathBuf {
    let mut candidates = [
        cratonvm_types::flags::runtime_var("JAVA_HOME_FOR_TZDB_TEST").ok(),
        cratonvm_types::flags::runtime_var("JAVA_HOME").ok(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    if let Ok(output) = std::process::Command::new("java")
        .args(["-XshowSettings:properties", "-version"])
        .output()
    {
        let settings = String::from_utf8_lossy(&output.stderr);
        if let Some(java_home) = settings
            .lines()
            .find_map(|line| line.trim().strip_prefix("java.home = ").map(str::to_owned))
        {
            candidates.push(java_home);
        }
    }

    let path = candidates
        .into_iter()
        .map(|java_home| {
            std::path::PathBuf::from(java_home)
                .join("lib")
                .join("tzdb.dat")
        })
        .find(|path| path.is_file())
        .expect(
            "tzdb.dat not found: set JAVA_HOME_FOR_TZDB_TEST or JAVA_HOME to a complete JDK",
        );
    path
}

fn test_catalog() -> Arc<TzdbCatalog> {
    let path = test_tzdb_path();
    let data = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    Arc::new(parse_catalog(&data).expect("failed to parse tzdb.dat"))
}

fn rules_for(cat: &TzdbCatalog, id: &str) -> ZoneRulesData {
    let resolved = cat
        .aliases
        .get(id)
        .cloned()
        .unwrap_or_else(|| id.to_string());
    let idx = *cat.region_to_rule_idx.get(&resolved).unwrap();
    parse_zone_rules(&cat.rule_bytes[idx]).unwrap()
}

#[test]
fn dst_correctness_matches_hotspot_reference_points() {
    let cat = test_catalog();
    let cases: &[(&str, i64, i32)] = &[
        ("America/New_York", 1500120000, -4 * 3600), // 2017-07-15 summer (DST)
        ("America/New_York", 1484481600, -5 * 3600), // 2017-01-15 winter
        ("Europe/Paris", 1500120000, 2 * 3600),
        ("Europe/Paris", 1484481600, 1 * 3600),
        ("Australia/Sydney", 1500120000, 10 * 3600), // S. hemisphere winter
        ("Australia/Sydney", 1484481600, 11 * 3600), // S. hemisphere summer
        ("UTC", 1500120000, 0),
    ];
    for (zone, epoch, expected) in cases {
        let rules = rules_for(&cat, zone);
        assert_eq!(
            offset_at_instant(&rules, *epoch),
            *expected,
            "zone={zone} epoch={epoch}"
        );
    }
}

#[test]
fn all_zones_parse_without_error() {
    let cat = test_catalog();
    for (id, &idx) in cat.region_to_rule_idx.iter() {
        assert!(
            parse_zone_rules(&cat.rule_bytes[idx]).is_some(),
            "failed to parse zone {id}"
        );
    }
}

// TESTDATE8-1H regression tests — synthetic "GMT+HH:MM" custom-offset
// zone ids used to resolve to a 0 offset everywhere (getOffset,
// getRawOffset, ...) because `get_zone_rules` only consulted the real
// tzdb.dat catalog, which has no entry for these synthetic ids.

#[test]
fn parse_fixed_gmt_offset_seconds_normalised_forms() {
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT+01:00"), Some(3600));
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT-05:00"), Some(-5 * 3600));
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT+00:00"), Some(0));
    assert_eq!(
        parse_fixed_gmt_offset_seconds("GMT+05:30"),
        Some(5 * 3600 + 1800)
    );
    assert_eq!(parse_fixed_gmt_offset_seconds("UTC+02:00"), Some(2 * 3600));
}

#[test]
fn parse_fixed_gmt_offset_seconds_short_forms() {
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT+1"), Some(3600));
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT+2"), Some(2 * 3600));
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT+0800"), Some(8 * 3600));
    assert_eq!(
        parse_fixed_gmt_offset_seconds("GMT-0530"),
        Some(-(5 * 3600 + 1800))
    );
}

#[test]
fn parse_fixed_gmt_offset_seconds_rejects_non_offset_ids() {
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT"), None);
    assert_eq!(parse_fixed_gmt_offset_seconds("UTC"), None);
    assert_eq!(parse_fixed_gmt_offset_seconds("America/New_York"), None);
    assert_eq!(parse_fixed_gmt_offset_seconds("Europe/Paris"), None);
    // Out-of-range hour/minute must not silently clamp.
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT+24:00"), None);
    assert_eq!(parse_fixed_gmt_offset_seconds("GMT+01:60"), None);
}

#[test]
fn fixed_offset_rules_constant_across_all_queries() {
    let rules = fixed_offset_rules(3600);
    // 1582-09-25T00:00:00Z-ish epoch second (the testDate8 regression
    // date) and a modern one — a fixed-offset zone has no DST, so the
    // answer must be identical (and non-zero) at both.
    for epoch_sec in [-12_220_156_800i64, 0, 1_500_120_000] {
        assert_eq!(offset_at_instant(&rules, epoch_sec), 3600);
        assert_eq!(offset_at_local(&rules, epoch_sec), 3600);
        assert_eq!(standard_offset_at_instant(&rules, epoch_sec), 3600);
    }
    assert_eq!(raw_offset(&rules), 3600);
}

// ---------------------------------------------------------------------
// G28-1 — the daylight-saving rule layer.
//
// Every expectation below is TRANSCRIBED from HotSpot 25.0.3+9-LTS
// (`scratchpad/g28/G28Values.java`), never derived from this module. The
// rule as a whole is separately validated on 632 zones x 15 accessors
// (`scratchpad/g28/G28Model.java`, 9,480 rows, 0 mismatches); these tests
// pin the rows a future edit is most likely to break.
// ---------------------------------------------------------------------

/// `2026-08-17T04:21:54Z`, the instant `G28Values` ran. `observesDaylight
/// Time` is the one member of this family that reads the wall clock, so the
/// only way to assert it deterministically is to fix the "now" the rule is
/// asked about. The three `use=false obs=true` zones below hold that status
/// for the whole of 2026 in this JDK's tzdb.
const G28_NOW_MS: i64 = 1_786_942_914_079;

#[test]
fn dst_savings_matches_hotspot_get_dst_savings() {
    let cat = test_catalog();
    // (zone, HotSpot getDSTSavings() in ms)
    let cases: &[(&str, i32)] = &[
        ("America/New_York", 3_600_000),
        ("Europe/London", 3_600_000),
        // Thirty minutes: the row a hard-coded hour gets wrong.
        ("Australia/Lord_Howe", 1_800_000),
        // Two hours, on a zone whose standard offset is 0.
        ("Antarctica/Troll", 7_200_000),
        ("Pacific/Chatham", 3_600_000),
        // Never observed daylight saving at all.
        ("Asia/Kolkata", 0),
        // A dense DST HISTORY and no current rule -- abolished 2019/2022.
        ("America/Sao_Paulo", 0),
        ("America/Mexico_City", 0),
        ("Asia/Tehran", 0),
        // A STANDING daylight offset with no recurring rule: 0 savings,
        // even though the zone is in daylight time at almost every instant.
        ("Africa/Casablanca", 0),
        ("Africa/Windhoek", 0),
        // The southern-hemisphere pair, where `lastRules` runs fall-then-
        // spring and `ZoneInfoFile` swaps them. Without the swap these
        // answer -3600000.
        ("Pacific/Auckland", 3_600_000),
        ("America/Santiago", 3_600_000),
    ];
    for (zone, expected) in cases {
        let rules = rules_for(&cat, zone);
        assert_eq!(
            dst_savings_seconds(&rules).saturating_mul(1000),
            *expected,
            "getDSTSavings zone={zone}"
        );
    }
}

#[test]
fn uses_daylight_time_matches_hotspot() {
    let cat = test_catalog();
    let cases: &[(&str, bool)] = &[
        ("America/New_York", true),
        ("Europe/London", true),
        ("Australia/Lord_Howe", true),
        ("Pacific/Chatham", true),
        ("Antarctica/Troll", true),
        ("Asia/Jerusalem", true),
        ("Africa/Cairo", true),
        ("Asia/Kolkata", false),
        ("Asia/Tokyo", false),
        ("America/Sao_Paulo", false),
        ("Asia/Tehran", false),
        ("America/Mexico_City", false),
        // The three ids that separate `use` from `obs`.
        ("Africa/Casablanca", false),
        ("Africa/El_Aaiun", false),
        ("Africa/Windhoek", false),
        ("UTC", false),
    ];
    for (zone, expected) in cases {
        let rules = rules_for(&cat, zone);
        assert_eq!(
            uses_daylight_time(&rules),
            *expected,
            "useDaylightTime zone={zone}"
        );
    }
}

#[test]
fn observes_daylight_time_separates_itself_from_uses() {
    let cat = test_catalog();
    // The whole point of this member: three zones answer `false` to
    // `useDaylightTime()` and `true` to `observesDaylightTime()`, because
    // tzdb models their permanent shift as a standing daylight offset with
    // no recurring rule.
    for zone in ["Africa/Casablanca", "Africa/El_Aaiun", "Africa/Windhoek"] {
        let rules = rules_for(&cat, zone);
        assert!(!uses_daylight_time(&rules), "useDaylightTime zone={zone}");
        assert!(
            observes_daylight_time_of(&rules, G28_NOW_MS),
            "observesDaylightTime zone={zone}"
        );
    }
    for zone in ["America/New_York", "Pacific/Chatham", "Antarctica/Troll"] {
        let rules = rules_for(&cat, zone);
        assert!(
            observes_daylight_time_of(&rules, G28_NOW_MS),
            "observesDaylightTime zone={zone}"
        );
    }
    for zone in [
        "Asia/Kolkata",
        "Asia/Tokyo",
        "America/Sao_Paulo",
        "America/Mexico_City",
        "Asia/Tehran",
        "UTC",
    ] {
        let rules = rules_for(&cat, zone);
        assert!(
            !observes_daylight_time_of(&rules, G28_NOW_MS),
            "observesDaylightTime zone={zone}"
        );
    }
}

#[test]
fn in_daylight_time_matches_hotspot_at_the_fixture_instants() {
    let cat = test_catalog();
    // 2021-01-15T12:00Z and 2021-07-15T12:00Z, the two instants
    // `RSimpleTimeZoneRaw` pins.
    const JAN: i64 = 1_610_712_000_000;
    const JUL: i64 = 1_626_350_400_000;
    // 1850-01-15T00:00Z -- below the 1900 floor, where `ZoneInfo`'s table
    // does not start and the answer is `false` for every zone.
    const OLD: i64 = -3_786_825_600_000;
    // 2045-03-15T00:00Z -- past the end of every stored transition table,
    // answered by the recurring rules.
    const FAR: i64 = 2_373_062_400_000;
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("CasablancaDaylight.java");
    std::fs::write(&probe, r#"class CasablancaDaylight { public static void main(String[] args) {
        System.out.print(java.util.TimeZone.getTimeZone("Africa/Casablanca").inDaylightTime(new java.util.Date(2373062400000L)));
    }}"#).unwrap();
    let java = test_tzdb_path().parent().unwrap().parent().unwrap().join("bin").join("java");
    let output = std::process::Command::new(java).arg(probe).output().expect("run JDK timezone probe");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let casablanca_far = match output.stdout.as_slice() {
        b"true" => true,
        b"false" => false,
        other => panic!("unexpected JDK timezone answer: {other:?}"),
    };
    // (zone, inJan, inJul, inOld, inFar)
    let cases: &[(&str, bool, bool, bool, bool)] = &[
        ("America/New_York", false, true, false, true),
        ("Europe/London", false, true, false, false),
        // Southern hemisphere: the seasons invert.
        ("Australia/Lord_Howe", true, false, false, true),
        ("Pacific/Chatham", true, false, false, true),
        ("Pacific/Auckland", true, false, false, true),
        ("America/Santiago", true, false, false, true),
        ("Asia/Kolkata", false, false, false, false),
        ("America/Sao_Paulo", false, false, false, false),
        // No RULE, so `getDSTSavings` is 0 -- but IN daylight time in July
        // 2021, because the explicit table still carried one.
        ("Asia/Tehran", false, true, false, false),
        ("America/Mexico_City", false, true, false, false),
        // Standing daylight offset: in daylight time at both instants.
        ("Africa/Casablanca", true, true, false, casablanca_far),
        ("Africa/Windhoek", true, true, false, true),
        // A recurring rule whose 2021 dates put both probes in standard
        // time -- the row that catches "assume July means daylight".
        ("Africa/Cairo", false, false, false, false),
        ("UTC", false, false, false, false),
    ];
    for (zone, in_jan, in_jul, in_old, in_far) in cases {
        let rules = rules_for(&cat, zone);
        assert_eq!(
            in_daylight_time_of(&rules, JAN),
            *in_jan,
            "inDaylightTime(JAN) zone={zone}"
        );
        assert_eq!(
            in_daylight_time_of(&rules, JUL),
            *in_jul,
            "inDaylightTime(JUL) zone={zone}"
        );
        assert_eq!(
            in_daylight_time_of(&rules, OLD),
            *in_old,
            "inDaylightTime(1850) zone={zone}"
        );
        assert_eq!(
            in_daylight_time_of(&rules, FAR),
            *in_far,
            "inDaylightTime(2045) zone={zone}"
        );
    }
}

#[test]
fn in_daylight_time_is_exact_at_a_transition_boundary() {
    let cat = test_catalog();
    // (zone, transition ms, offset before, offset at, in-dst before, in-dst at)
    let edge = |zone: &str, t: i64, before: i32, at: i32, in_before: bool, in_at: bool| {
        let rules = rules_for(&cat, zone);
        assert_eq!(
            legacy_offsets_ms_of(&rules, t - 1).0,
            before,
            "getOffset(t-1) zone={zone} t={t}"
        );
        assert_eq!(
            legacy_offsets_ms_of(&rules, t).0,
            at,
            "getOffset(t) zone={zone} t={t}"
        );
        assert_eq!(
            in_daylight_time_of(&rules, t - 1),
            in_before,
            "inDaylightTime(t-1) zone={zone} t={t}"
        );
        assert_eq!(
            in_daylight_time_of(&rules, t),
            in_at,
            "inDaylightTime(t) zone={zone} t={t}"
        );
        assert_eq!(
            in_daylight_time_of(&rules, t + 1),
            in_at,
            "inDaylightTime(t+1) zone={zone} t={t}"
        );
    };
    let ny = "America/New_York";
    edge(ny, 1_615_705_200_000, -18_000_000, -14_400_000, false, true);
    edge(ny, 1_636_264_800_000, -14_400_000, -18_000_000, true, false);
    let ldn = "Europe/London";
    edge(ldn, 1_616_893_200_000, 0, 3_600_000, false, true);
    edge(ldn, 1_635_642_000_000, 3_600_000, 0, true, false);
    // Southern hemisphere, half-hour step.
    let lhi = "Australia/Lord_Howe";
    edge(lhi, 1_617_462_000_000, 39_600_000, 37_800_000, true, false);
    edge(lhi, 1_633_188_600_000, 37_800_000, 39_600_000, false, true);
    let cha = "Pacific/Chatham";
    edge(cha, 1_617_458_400_000, 49_500_000, 45_900_000, true, false);
    edge(cha, 1_632_578_400_000, 45_900_000, 49_500_000, false, true);
}

/// The local-standard-time millis the six-arg native builds from
/// `(year, month0, day, millisInDay)`.
fn local_standard_ms(year: i32, month0: i32, day: i32, millis: i32) -> i64 {
    to_epoch_day(year, month0 + 1, day) * 86_400_000 + millis as i64
}

#[test]
fn six_arg_offset_reads_local_standard_time() {
    let cat = test_catalog();
    // 2021-01-15 12:00 and 2021-07-15 12:00 STANDARD local, the arguments
    // `RSimpleTimeZoneRaw` passes.
    let jan = local_standard_ms(2021, 0, 15, 43_200_000);
    let jul = local_standard_ms(2021, 6, 15, 43_200_000);
    let cases: &[(&str, i32, i32)] = &[
        ("America/New_York", -18_000_000, -14_400_000),
        ("Europe/London", 0, 3_600_000),
        // Both half-hour zones: a rule that reads a whole-hour field
        // passes every other row here and fails these two.
        ("Australia/Lord_Howe", 39_600_000, 37_800_000),
        ("Pacific/Chatham", 49_500_000, 45_900_000),
        ("Asia/Kolkata", 19_800_000, 19_800_000),
        ("America/Sao_Paulo", -10_800_000, -10_800_000),
        ("Asia/Tehran", 12_600_000, 16_200_000),
        ("America/St_Johns", -12_600_000, -9_000_000),
        ("Africa/Windhoek", 7_200_000, 7_200_000),
        ("UTC", 0, 0),
    ];
    for (zone, expect_jan, expect_jul) in cases {
        let rules = rules_for(&cat, zone);
        assert_eq!(
            offset_ms_at_local_standard_of(&rules, jan),
            *expect_jan,
            "six-arg JAN zone={zone}"
        );
        assert_eq!(
            offset_ms_at_local_standard_of(&rules, jul),
            *expect_jul,
            "six-arg JUL zone={zone}"
        );
    }

    // Inside the spring-forward gap and the fall-back overlap, both
    // expressed in STANDARD local time, where the conversion must not fold
    // daylight saving into the wall reading.
    let ny = rules_for(&cat, "America/New_York");
    assert_eq!(
        offset_ms_at_local_standard_of(&ny, local_standard_ms(2021, 2, 14, 7_200_000)),
        -14_400_000
    );
    assert_eq!(
        offset_ms_at_local_standard_of(&ny, local_standard_ms(2021, 10, 7, 3_600_000)),
        -18_000_000
    );
    // The half-hour zone across its own transitions.
    let lhi = rules_for(&cat, "Australia/Lord_Howe");
    assert_eq!(
        offset_ms_at_local_standard_of(&lhi, local_standard_ms(2021, 9, 15, 43_200_000)),
        39_600_000
    );
    assert_eq!(
        offset_ms_at_local_standard_of(&lhi, local_standard_ms(2021, 3, 15, 43_200_000)),
        37_800_000
    );
}

#[test]
fn six_arg_offset_honours_the_1900_floor() {
    let cat = test_catalog();
    // 1850 is below `ZoneInfo`'s transition table, so every zone answers
    // its MODERN raw offset -- including the two whose modern offset is a
    // standing daylight one.
    let old = local_standard_ms(1850, 6, 15, 43_200_000);
    let cases: &[(&str, i32)] = &[
        ("America/New_York", -18_000_000),
        ("Europe/London", 0),
        ("Australia/Lord_Howe", 37_800_000),
        ("Pacific/Chatham", 45_900_000),
        ("Africa/Casablanca", 0),
        ("Africa/Windhoek", 3_600_000),
    ];
    for (zone, expected) in cases {
        let rules = rules_for(&cat, zone);
        assert_eq!(
            offset_ms_at_local_standard_of(&rules, old),
            *expected,
            "six-arg 1850 zone={zone}"
        );
        assert!(!in_daylight_time_of(&rules, -3_786_825_600_000));
    }
    // era = BC, year = 100 -> proleptic year -99, still below the floor.
    let bc = local_standard_ms(1 - 100, 6, 15, 43_200_000);
    let ny = rules_for(&cat, "America/New_York");
    assert_eq!(offset_ms_at_local_standard_of(&ny, bc), -18_000_000);
}

#[test]
fn gregorian_epoch_day_anchors_and_the_six_arg_date_validation() {
    assert_eq!(to_epoch_day(1970, 1, 1), 0);
    assert_eq!(to_epoch_day(1969, 12, 31), -1);
    assert_eq!(to_epoch_day(2000, 3, 1), 11017);
    assert_eq!(to_epoch_day(2021, 7, 15), 18823);

    // The four rows `ZoneInfo`'s six-arg rejects with a message-less
    // IllegalArgumentException, and the ones it accepts.
    assert!(gregorian_date_is_valid(2021, 7, 15));
    assert!(gregorian_date_is_valid(2020, 2, 29));
    assert!(gregorian_date_is_valid(2021, 12, 31));
    assert!(!gregorian_date_is_valid(2021, 2, 29));
    assert!(!gregorian_date_is_valid(2021, 2, 30));
    assert!(!gregorian_date_is_valid(2021, 13, 15));
    assert!(!gregorian_date_is_valid(2021, 0, 15));
    assert!(!gregorian_date_is_valid(2021, 7, 0));
    assert!(!gregorian_date_is_valid(2021, 7, 32));
}

#[test]
fn last_savings_transition_year_pins_to_lastyear_beyond_utc2100() {
    // 1_000_000_000 is 2001-09-09T01:46:40Z; `ZoneInfoFile` reads the year
    // at `wallOffsets[n-1]`, the offset in force BEFORE that transition.
    let below = ZoneRulesData {
        standard_transitions: Vec::new(),
        standard_offsets: vec![0],
        savings_instant_transitions: vec![0, 1_000_000_000],
        wall_offsets: vec![0, 3600, 0],
        last_rules: Vec::new(),
    };
    assert_eq!(last_savings_transition_year(&below), Some(2001));

    let beyond = ZoneRulesData {
        standard_transitions: Vec::new(),
        standard_offsets: vec![0],
        savings_instant_transitions: vec![0, ZONEINFO_UTC2100_EPOCH_SEC + 1],
        wall_offsets: vec![0, 3600, 0],
        last_rules: Vec::new(),
    };
    assert_eq!(
        last_savings_transition_year(&beyond),
        Some(ZONEINFO_LASTYEAR)
    );

    // A zone with no savings transitions never reaches the block that
    // computes it.
    assert_eq!(
        last_savings_transition_year(&fixed_offset_rules(3600)),
        None
    );
}

#[test]
fn a_fixed_offset_zone_never_observes_daylight_saving() {
    // The `GMT+HH:MM` fallback and, by the same arms, any id the catalog
    // cannot resolve: no savings transitions, so every member of the family
    // answers "no rule" without touching `last_rules`.
    let rules = fixed_offset_rules(5 * 3600 + 1800);
    assert_eq!(dst_savings_seconds(&rules), 0);
    assert!(!uses_daylight_time(&rules));
    assert!(!observes_daylight_time_of(&rules, G28_NOW_MS));
    assert!(!in_daylight_time_of(&rules, 1_626_350_400_000));
    assert_eq!(
        offset_ms_at_local_standard_of(&rules, local_standard_ms(2021, 6, 15, 43_200_000)),
        19_800_000
    );
    // ...and the pre-1900 arm answers the same constant rather than 0.
    assert_eq!(
        offset_ms_at_local_standard_of(&rules, local_standard_ms(1850, 6, 15, 43_200_000)),
        19_800_000
    );
}

#[test]
fn legacy_offsets_split_total_from_saving() {
    let cat = test_catalog();
    let ny = rules_for(&cat, "America/New_York");
    // In daylight time: total -04:00 = raw -05:00 plus one hour of saving.
    assert_eq!(
        legacy_offsets_ms_of(&ny, 1_626_350_400_000),
        (-14_400_000, 3_600_000)
    );
    // Standard time: no saving.
    assert_eq!(
        legacy_offsets_ms_of(&ny, 1_610_712_000_000),
        (-18_000_000, 0)
    );
    // Below the 1900 floor the legacy rule reports the modern raw offset
    // with a zero saving, bug-for-bug with `ZoneInfoFile`.
    assert_eq!(
        legacy_offsets_ms_of(&ny, -3_786_825_600_000),
        (-18_000_000, 0)
    );
    // A half-hour saving must survive the split.
    let lhi = rules_for(&cat, "Australia/Lord_Howe");
    assert_eq!(
        legacy_offsets_ms_of(&lhi, 1_610_712_000_000),
        (39_600_000, 1_800_000)
    );
}

#[test]
fn the_first_legacy_transition_is_searched_in_wall_time() {
    // HotSpot `getOffsetsByWall` at 1900-01-01: the cut-over is UTC1900 plus
    // the zone's 1900 offset, measured against HotSpot 25 over every id.
    let cat = test_catalog();
    let paris = rules_for(&cat, "Europe/Paris"); // +0:09:21 at UTC1900
    let floor = ZONEINFO_LEGACY_FLOOR_EPOCH_SEC;
    assert!(wall_before_first_legacy_transition(&paris, floor + 560));
    assert!(!wall_before_first_legacy_transition(&paris, floor + 561));
    let abidjan = rules_for(&cat, "Africa/Abidjan"); // -0:16:08 at UTC1900
    assert!(wall_before_first_legacy_transition(&abidjan, floor - 969));
    assert!(!wall_before_first_legacy_transition(&abidjan, floor - 968));
}

#[test]
fn get_zone_rules_falls_back_to_fixed_gmt_offset() {
    // No NativeContext is exercised here (parse_fixed_gmt_offset_seconds
    // + fixed_offset_rules are the pure, ctx-free half of the fix that
    // `get_zone_rules` delegates to once the tzdb catalog lookup misses);
    // the full ctx-driven path is covered by the Java-level repro in
    // bug-h2-suite-residual-fail-triage-FIXED.md.
    let rules = parse_fixed_gmt_offset_seconds("GMT+01:00")
        .map(fixed_offset_rules)
        .expect("GMT+01:00 must resolve to a fixed-offset rule set");
    assert_eq!(raw_offset(&rules), 3600);
    assert_eq!(offset_at_instant(&rules, -12_220_156_800), 3600);
}


#[test]
fn wall_offsets_match_hotspot_in_gaps_and_overlaps() {
    let cat = test_catalog();
    let ny = rules_for(&cat, "America/New_York");
    // 2024-03-10T02:30:00 (spring-forward gap) -> EST (-5h, dst=0)
    assert_eq!(
        legacy_wall_offsets_ms_of(&ny, 1_710_037_800_000),
        (-18_000_000, 0)
    );
    // 2024-11-03T01:30:00 (fall-back overlap) -> EST (-5h, dst=0)
    assert_eq!(
        legacy_wall_offsets_ms_of(&ny, 1_730_597_400_000),
        (-18_000_000, 0)
    );
    // 1990-04-01T02:30:00 -> EST (-5h, dst=0)
    assert_eq!(
        legacy_wall_offsets_ms_of(&ny, 638_937_000_000),
        (-18_000_000, 0)
    );
    // 1990-10-28T01:30:00 -> EST (-5h, dst=0)
    assert_eq!(
        legacy_wall_offsets_ms_of(&ny, 657_077_400_000),
        (-18_000_000, 0)
    );
    // 2045-03-10T02:30:00 (beyond explicit tzdb table, uses last_rules) -> EST
    assert_eq!(
        legacy_wall_offsets_ms_of(&ny, 2_372_783_400_000),
        (-18_000_000, 0)
    );
    // Pacific/Apia 2011-12-30T12:00:00 skip -> (-36000_000, 3600_000)
    let apia = rules_for(&cat, "Pacific/Apia");
    assert_eq!(
        legacy_wall_offsets_ms_of(&apia, 1_325_246_400_000),
        (-36_000_000, 3_600_000)
    );
    // Australia/Sydney 2024-04-07T02:30 overlap & 2024-10-06T02:30 gap -> std (36000_000, 0)
    let syd = rules_for(&cat, "Australia/Sydney");
    assert_eq!(
        legacy_wall_offsets_ms_of(&syd, 1_712_457_000_000),
        (36_000_000, 0)
    );
    assert_eq!(
        legacy_wall_offsets_ms_of(&syd, 1_728_181_800_000),
        (36_000_000, 0)
    );
}
