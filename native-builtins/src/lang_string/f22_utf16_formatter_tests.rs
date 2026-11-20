use super::*;

/// `[%5.1s]` of U+1F600 is SEVEN characters on HotSpot 25:
/// `005B 0020 0020 0020 0020 D83D 005D`. The truncation keeps the lone HIGH
/// surrogate, and the justifier then pads to 5 - 1 = 4.
///
/// Mutants this kills:
///   * a truncator that counts CODE POINTS keeps the whole pair -> 2 units,
///     pad 3, total 7 but the WRONG units (`D83D DE00` both present);
///   * a truncator that refuses to split a pair returns 0 units, pad 5,
///     total 7 again — which is why the assertion is on the UNITS and not
///     only on the length. Both wrong answers have length 7.
#[test]
fn truncate_then_justify_keeps_a_lone_high_surrogate() {
    let emoji: Vec<u16> = "\u{1F600}".encode_utf16().collect();
    assert_eq!(emoji, vec![0xD83D, 0xDE00]);

    let mut units = emoji.clone();
    fmt_truncate_units(&mut units, 1);
    assert_eq!(
        units,
        vec![0xD83D],
        "%.1s must keep the lone high surrogate"
    );

    let padded = fmt_pad_to_width(units, "", Some(5)).expect("width 5 is small");
    assert_eq!(padded, vec![0x0020, 0x0020, 0x0020, 0x0020, 0xD83D]);

    // The whole `[%5.1s]` row, brackets included.
    let mut row = vec![0x005Bu16];
    row.extend_from_slice(&padded);
    row.push(0x005D);
    assert_eq!(row.len(), 7, "HotSpot 25 measures 7 characters");
    assert_eq!(
        row,
        vec![0x005B, 0x0020, 0x0020, 0x0020, 0x0020, 0xD83D, 0x005D]
    );
}

/// `%.3s` of `"a<U+1F600>b"` is three code UNITS — `0061 D83D DE00` — and
/// `%.2s` of three lone high surrogates keeps two of them.
///
/// Kills a code-POINT truncator (which answers `a<U+1F600>b`, 4 units) and
/// a byte truncator (`str::len` would cut `a` plus 2 bytes of the emoji).
#[test]
fn truncate_counts_code_units_not_points_or_bytes() {
    let mut units: Vec<u16> = "a\u{1F600}b".encode_utf16().collect();
    assert_eq!(units.len(), 4);
    fmt_truncate_units(&mut units, 3);
    assert_eq!(units, vec![0x0061, 0xD83D, 0xDE00]);

    let mut lone = vec![0xD800u16, 0xD800, 0xD800];
    fmt_truncate_units(&mut lone, 2);
    assert_eq!(lone, vec![0xD800, 0xD800]);

    // A precision at or past the length is a no-op, not a pad.
    let mut short = vec![0x0061u16];
    fmt_truncate_units(&mut short, 9);
    assert_eq!(short, vec![0x0061]);
}

/// The justifier counts UTF-16 units, pads with U+0020, and honours '-'.
///
/// Kills a `str::len` (byte) justifier: `"é"` is one unit and two UTF-8
/// bytes, and `String.format("%5s|", "é")` needs FOUR spaces on HotSpot 25.
/// Also kills a justifier that treats a lone surrogate as zero-width.
#[test]
fn justifier_counts_units_and_honours_left_flag() {
    let e: Vec<u16> = "\u{00e9}".encode_utf16().collect();
    assert_eq!(e.len(), 1);
    let padded = fmt_pad_to_width(e.clone(), "", Some(5)).unwrap();
    assert_eq!(padded.len(), 5);
    assert_eq!(padded[..4], [0x0020, 0x0020, 0x0020, 0x0020]);

    let left = fmt_pad_to_width(e, "-", Some(5)).unwrap();
    assert_eq!(left[0], 0x00E9);
    assert_eq!(left[1..], [0x0020, 0x0020, 0x0020, 0x0020]);

    // A lone surrogate is ONE unit to the justifier, like any other char.
    let lone = fmt_pad_to_width(vec![0xD800], "", Some(3)).unwrap();
    assert_eq!(lone, vec![0x0020, 0x0020, 0xD800]);

    // No width, and a width no larger than the content, are both no-ops.
    assert_eq!(
        fmt_pad_to_width(vec![0x0061], "", None).unwrap(),
        vec![0x0061]
    );
    assert_eq!(
        fmt_pad_to_width(vec![0x0061, 0x0062], "", Some(2)).unwrap(),
        vec![0x0061, 0x0062]
    );
}

/// `Character.toChars` semantics for the `%c` conversion, as
/// [`fmt_general_units`] applies them: one unit for any BMP code point
/// INCLUDING a lone surrogate, a high+low pair for a supplementary one.
///
/// Measured: `String.format(ROOT, "%c", (int) 0xD800)` has `length() == 1`
/// and unit `D800` on HotSpot 25. Kills the
/// `char::from_u32(..).unwrap_or('?')` implementation, which answers `003F`.
#[test]
fn code_point_to_units_matches_character_to_chars() {
    fn to_units(cp: u32) -> Vec<u16> {
        if cp > 0xFFFF {
            let v = cp - 0x10000;
            vec![(0xD800 + (v >> 10)) as u16, (0xDC00 + (v & 0x3FF)) as u16]
        } else {
            vec![cp as u16]
        }
    }
    assert_eq!(
        to_units(0xD800),
        vec![0xD800],
        "lone HIGH surrogate, not '?'"
    );
    assert_eq!(
        to_units(0xDFFF),
        vec![0xDFFF],
        "lone LOW surrogate, not '?'"
    );
    assert_eq!(to_units(0x1F600), vec![0xD83D, 0xDE00]);
    assert_eq!(to_units(0x41), vec![0x0041]);
    assert_eq!(to_units(0x10FFFF), vec![0xDBFF, 0xDFFF]);
    // Agrees with Rust wherever Rust can answer at all.
    for cp in [0x41u32, 0xE9, 0x20AC, 0x1F600, 0x10FFFF] {
        let expect: Vec<u16> = char::from_u32(cp)
            .unwrap()
            .to_string()
            .encode_utf16()
            .collect();
        assert_eq!(to_units(cp), expect, "cp {cp:#x}");
    }
}

/// `TimeZone`'s `GMT±HH:MM` fallback, which is HotSpot's own `Locale.ROOT`
/// answer for `%tZ`. Measured rows: `America/New_York` in November is
/// `GMT-05:00` (offset -18000000 ms) and in July `GMT-04:00`;
/// `Europe/Berlin` is `GMT+01:00` / `GMT+02:00`; `Asia/Kolkata` is
/// `GMT+05:30`.
///
/// Kills an implementation that (a) drops the sign on a negative offset,
/// (b) divides by 3_600_000 and loses the half-hour minutes, or
/// (c) renders the minutes as a fraction of an hour.
#[test]
fn gmt_fallback_form_matches_hotspot_rows() {
    assert_eq!(fmt_zone_gmt_form(-18_000_000), "GMT-05:00");
    assert_eq!(fmt_zone_gmt_form(-14_400_000), "GMT-04:00");
    assert_eq!(fmt_zone_gmt_form(3_600_000), "GMT+01:00");
    assert_eq!(fmt_zone_gmt_form(7_200_000), "GMT+02:00");
    assert_eq!(fmt_zone_gmt_form(19_800_000), "GMT+05:30", "half-hour zone");
    assert_eq!(fmt_zone_gmt_form(0), "GMT+00:00");
    // Nepal, the 45-minute zone — the case an hours-only render would lose.
    assert_eq!(fmt_zone_gmt_form(20_700_000), "GMT+05:45");
}

/// A zone that was never resolved must keep the pre-existing UTC identity,
/// so a VM with no reachable `java.util.TimeZone` does not start printing
/// `GMT+00:00` where it used to print `UTC`, and does not shift any field.
#[test]
fn unknown_zone_is_utc_and_zero_offset() {
    assert_eq!(FmtZone::NONE.offset_ms, 0);
    assert!(!FmtZone::NONE.dst);
    assert!(!FmtZone::NONE.known);
    // A `NONE` zone must not claim to be a `java.time` one, or `%tZ` would
    // go looking for a `ZoneId` on a `Date`.
    assert!(!FmtZone::NONE.temporal);
}

/// Which `java.time` sources refuse which `%t` fields — the full measured
/// matrix from HotSpot 25 (`String.format(Locale.ROOT, "%t"+f, src)` for
/// each of the 31 fields against each source, under
/// `-Duser.timezone=Asia/Kolkata`). The six original sources are F28-1's;
/// `OffsetTime`, `YearMonth`, `MonthDay`, `Year` and `Month` are G2-1's,
/// measured 2026-08-16.
///
/// This is the check the lane exists for: every one of these used to be
/// ANSWERED here, from a fabricated 0, because `invoke_i32` returns 0 for
/// a method the class does not have.
///
/// Mutants this kills, each a specific wrong implementation:
///  * "only `%tZ` refuses" (the brief's framing) — the `%tH`/`%tY` rows;
///  * one flag for all clock fields — the `Instant` `%tL`/`%tN` rows,
///    which are ANSWERED while `%tH` is not;
///  * composites reporting their OWN character — `%tT` of a `LocalDate`
///    reports `H`, `%tc` of an `Instant` reports `a`;
///  * `%tF` reporting an inner character — it reports `F`;
///  * `%tc` reporting a fixed character — it is `a`, `H` or `Z`
///    depending on which group is missing;
///  * refusing anything on an epoch-shaped source.
#[test]
fn temporal_support_matrix_matches_hotspot() {
    const INSTANT: FmtSupport = FmtSupport {
        year: false,
        month: false,
        day: false,
        time: false,
        sub_second: true,
        instant: true,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    const LOCAL_DATE: FmtSupport = FmtSupport {
        year: true,
        month: true,
        day: true,
        time: false,
        sub_second: false,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    const LOCAL_TIME: FmtSupport = FmtSupport {
        year: false,
        month: false,
        day: false,
        time: true,
        sub_second: true,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    const LOCAL_DATE_TIME: FmtSupport = FmtSupport {
        year: true,
        month: true,
        day: true,
        time: true,
        sub_second: true,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    const ZONED: FmtSupport = FmtSupport {
        year: true,
        month: true,
        day: true,
        time: true,
        sub_second: true,
        instant: true,
        zone: true,
        calendar_printer: false,
        weekday_only: false,
    };
    // G2-1's four partial `java.time` sources, plus `OffsetTime`.
    const OFFSET_TIME: FmtSupport = FmtSupport {
        year: false,
        month: false,
        day: false,
        time: true,
        sub_second: true,
        instant: false,
        zone: true,
        calendar_printer: false,
        weekday_only: false,
    };
    const YEAR_MONTH: FmtSupport = FmtSupport {
        year: true,
        month: true,
        day: false,
        time: false,
        sub_second: false,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    const MONTH_DAY: FmtSupport = FmtSupport {
        year: false,
        month: true,
        day: true,
        time: false,
        sub_second: false,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    const YEAR_ONLY: FmtSupport = FmtSupport {
        year: true,
        month: false,
        day: false,
        time: false,
        sub_second: false,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    const MONTH_ONLY: FmtSupport = FmtSupport {
        year: false,
        month: true,
        day: false,
        time: false,
        sub_second: false,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: false,
    };
    // G2-1 residual §4.1, closed. The one measured source that answers
    // `%tA`/`%ta` with NO year, month or day at all.
    const DAY_OF_WEEK: FmtSupport = FmtSupport {
        year: false,
        month: false,
        day: false,
        time: false,
        sub_second: false,
        instant: false,
        zone: false,
        calendar_printer: false,
        weekday_only: true,
    };

    // `calendar_printer` is NOT derivable from the support flags:
    // `ZONED` sets every one of them and still takes the OTHER printer.
    // That is the whole reason it is a stored field, so assert it.
    assert!(FmtSupport::ALL.calendar_printer);
    assert!(!ZONED.calendar_printer);
    assert_eq!(
        (
            ZONED.year,
            ZONED.month,
            ZONED.day,
            ZONED.time,
            ZONED.sub_second,
            ZONED.instant,
            ZONED.zone
        ),
        (
            FmtSupport::ALL.year,
            FmtSupport::ALL.month,
            FmtSupport::ALL.day,
            FmtSupport::ALL.time,
            FmtSupport::ALL.sub_second,
            FmtSupport::ALL.instant,
            FmtSupport::ALL.zone
        ),
        "a ZonedDateTime supports exactly what a Calendar does; only the \
         PRINTER differs, so `calendar_printer` cannot be computed"
    );

    // G2-1, measured 2026-08-16 on HotSpot 25.0.3+9. Each row is the
    // source's complete ANSWERED set out of the 31 fields; everything not
    // listed refuses, with the character given below it. Kills a
    // "one `date` flag" implementation at every one of the five sources.
    for (name, sup, answers) in [
        ("OffsetTime", OFFSET_TIME, "HIklMSLNpzZRTr"),
        ("YearMonth", YEAR_MONTH, "CYymBbh"),
        ("MonthDay", MONTH_DAY, "mdeBbh"),
        ("Year", YEAR_ONLY, "CYy"),
        ("Month", MONTH_ONLY, "mBbh"),
    ] {
        for f in FMT_DATETIME_FIELDS.chars() {
            let got = fmt_temporal_fault_char(f, sup);
            if answers.contains(f) {
                assert_eq!(got, None, "%t{f} must be ANSWERED on {name}");
            } else {
                assert!(got.is_some(), "%t{f} must be REFUSED on {name}");
            }
        }
    }
    // The composites' refusal characters, which a per-type table cannot
    // produce and a single `date` flag cannot distinguish.
    for (name, sup, d, ff, c) in [
        ("OffsetTime", OFFSET_TIME, 'm', 'F', 'a'),
        ("YearMonth", YEAR_MONTH, 'd', 'd', 'a'),
        ("MonthDay", MONTH_DAY, 'y', 'F', 'a'),
        ("Year", YEAR_ONLY, 'm', 'm', 'a'),
        ("Month", MONTH_ONLY, 'd', 'F', 'a'),
    ] {
        assert_eq!(fmt_temporal_fault_char('D', sup), Some(d), "%tD on {name}");
        assert_eq!(fmt_temporal_fault_char('F', sup), Some(ff), "%tF on {name}");
        assert_eq!(fmt_temporal_fault_char('c', sup), Some(c), "%tc on {name}");
    }
    // `%tA`/`%ta`/`%tj` need a COMPLETE date, which is the derivation in
    // `FmtSupport::full_date`.
    for f in "Aaj".chars() {
        assert_eq!(fmt_temporal_fault_char(f, LOCAL_DATE), None);
        for sup in [YEAR_MONTH, MONTH_DAY, YEAR_ONLY, MONTH_ONLY, OFFSET_TIME] {
            assert_eq!(fmt_temporal_fault_char(f, sup), Some(f));
        }
    }
    // G2-1 residual §4.1, closed. `DayOfWeek` is the measured exception
    // `FmtSupport::full_date` cannot express: `%tA`/`%ta` are ANSWERED
    // with no year, month or day, `%tj` (DAY_OF_YEAR) still REFUSES
    // (needs a real date, which weekday-only does not derive), and the
    // other 29 fields refuse too. `%tc` reports `b` (the MONTH group),
    // not `a` — the weekday check passes for `DAY_OF_WEEK`, so it is the
    // first REAL gap, `%tc`'s answer under naive `full_date()`-only
    // derivation would wrongly be `a`.
    assert_eq!(fmt_temporal_fault_char('A', DAY_OF_WEEK), None);
    assert_eq!(fmt_temporal_fault_char('a', DAY_OF_WEEK), None);
    assert_eq!(fmt_temporal_fault_char('j', DAY_OF_WEEK), Some('j'));
    assert_eq!(fmt_temporal_fault_char('c', DAY_OF_WEEK), Some('b'));
    for f in FMT_DATETIME_FIELDS.chars() {
        if f == 'A' || f == 'a' {
            continue;
        }
        assert!(
            fmt_temporal_fault_char(f, DAY_OF_WEEK).is_some(),
            "%t{f} must be REFUSED on DayOfWeek"
        );
    }

    // An epoch-shaped argument takes the `Calendar` printer and refuses
    // NOTHING — all 31 fields.
    for f in FMT_DATETIME_FIELDS.chars() {
        assert_eq!(
            fmt_temporal_fault_char(f, FmtSupport::ALL),
            None,
            "%t{f} must not refuse a Date/Calendar/long"
        );
        // A fully-supported `java.time` source likewise.
        assert_eq!(fmt_temporal_fault_char(f, ZONED), None, "%t{f} on Zoned");
    }

    // Instant: FOUR answers, 27 refusals.
    for f in "LNsQ".chars() {
        assert_eq!(
            fmt_temporal_fault_char(f, INSTANT),
            None,
            "%t{f} on Instant"
        );
    }
    for (f, want) in [
        ('H', 'H'),
        ('I', 'I'),
        ('k', 'k'),
        ('l', 'l'),
        ('M', 'M'),
        ('S', 'S'),
        ('p', 'p'),
        ('z', 'z'),
        ('Z', 'Z'),
        ('B', 'B'),
        ('b', 'b'),
        ('h', 'h'),
        ('A', 'A'),
        ('a', 'a'),
        ('C', 'C'),
        ('Y', 'Y'),
        ('y', 'y'),
        ('j', 'j'),
        ('m', 'm'),
        ('d', 'd'),
        ('e', 'e'),
        ('R', 'H'),
        ('T', 'H'),
        ('r', 'I'),
        ('D', 'm'),
        ('F', 'F'),
        ('c', 'a'),
    ] {
        assert_eq!(
            fmt_temporal_fault_char(f, INSTANT),
            Some(want),
            "%t{f} on Instant"
        );
    }

    // LocalDate: the date group answers, the clock group does not.
    for f in "BbhAaCYyjmdeDF".chars() {
        assert_eq!(
            fmt_temporal_fault_char(f, LOCAL_DATE),
            None,
            "%t{f} on LocalDate"
        );
    }
    for (f, want) in [
        ('H', 'H'),
        ('L', 'L'),
        ('N', 'N'),
        ('s', 's'),
        ('Q', 'Q'),
        ('z', 'z'),
        ('Z', 'Z'),
        ('R', 'H'),
        ('T', 'H'),
        ('r', 'I'),
        ('c', 'H'),
    ] {
        assert_eq!(
            fmt_temporal_fault_char(f, LOCAL_DATE),
            Some(want),
            "%t{f} on LocalDate"
        );
    }

    // LocalTime: the mirror image.
    for f in "HIklMSpLNRTr".chars() {
        assert_eq!(
            fmt_temporal_fault_char(f, LOCAL_TIME),
            None,
            "%t{f} on LocalTime"
        );
    }
    for (f, want) in [
        ('Y', 'Y'),
        ('b', 'b'),
        ('a', 'a'),
        ('s', 's'),
        ('Q', 'Q'),
        ('Z', 'Z'),
        ('D', 'm'),
        ('F', 'F'),
        ('c', 'a'),
    ] {
        assert_eq!(
            fmt_temporal_fault_char(f, LOCAL_TIME),
            Some(want),
            "%t{f} on LocalTime"
        );
    }

    // LocalDateTime: everything but the zone and the epoch.
    for f in "HIklMSpLNBbhAaCYyjmdeRTrDF".chars() {
        assert_eq!(
            fmt_temporal_fault_char(f, LOCAL_DATE_TIME),
            None,
            "%t{f} on LocalDateTime"
        );
    }
    for (f, want) in [('z', 'z'), ('Z', 'Z'), ('s', 's'), ('Q', 'Q'), ('c', 'Z')] {
        assert_eq!(
            fmt_temporal_fault_char(f, LOCAL_DATE_TIME),
            Some(want),
            "%t{f} on LocalDateTime"
        );
    }
}

/// The `%t` numbers take the locale's zero digit and NOTHING else takes
/// anything. Rows measured on HotSpot 25 under `ar-EG`, whose zero digit
/// is U+0660.
///
/// Kills: (a) an implementation that leaves the digits ASCII, which is
/// what this file did; (b) one that reuses `fmt_localize` and so rewrites
/// `'.'` and `','` — `%tD` is `mm/dd/yy` and `ru`'s `%tb` is `нояб.`;
/// (c) one that shifts the `'+'` of a `%tz` or the `':'` of a `%tT`;
/// (d) one that shifts non-ASCII digits already present in a name.
#[test]
fn t_digits_take_the_locale_zero_and_nothing_else() {
    let ar = FmtSymbols {
        grouping: '\u{066C}',
        decimal: '\u{066B}',
        zero: '\u{0660}',
        // Measured: `ar-EG`'s `getMinusSign()` IS U+002D. It is spelled out
        // rather than defaulted so this row stays a MEASUREMENT of the
        // locale and not an inheritance from `FmtSymbols::default`.
        minus: '-',
    };
    assert_eq!(fmt_localize_digits("03", ar), "\u{0660}\u{0663}");
    assert_eq!(
        fmt_localize_digits("03:43:19", ar),
        "\u{0660}\u{0663}:\u{0664}\u{0663}:\u{0661}\u{0669}",
        "the ':' separators stay ASCII"
    );
    assert_eq!(
        fmt_localize_digits("+0530", ar),
        "+\u{0660}\u{0665}\u{0663}\u{0660}",
        "the %tz sign stays ASCII"
    );
    assert_eq!(
        fmt_localize_digits("11/15/23", ar),
        "\u{0661}\u{0661}/\u{0661}\u{0665}/\u{0662}\u{0663}",
        "'/' is not the grouping separator"
    );
    assert_eq!(
        fmt_localize_digits("-0044-03-15", ar),
        "-\u{0660}\u{0660}\u{0664}\u{0664}-\u{0660}\u{0663}-\u{0661}\u{0665}"
    );
    // A `DateFormatSymbols` name that ends in '.' must survive intact —
    // `fmt_localize` would have rewritten that '.' into U+066B.
    assert_eq!(
        fmt_localize_digits("\u{043D}\u{043E}\u{044F}\u{0431}.", ar),
        "\u{043D}\u{043E}\u{044F}\u{0431}."
    );
    // The overwhelmingly common case is a no-op, and must be exactly one.
    let root = FmtSymbols::default();
    assert_eq!(root.zero, '0');
    for s in ["03:43:19", "+0530", "1699999999", "нояб.", ""] {
        assert_eq!(fmt_localize_digits(s, root), s);
    }
}

/// `%tF`'s year: sign OUTSIDE a four-digit zero pad, and a '+' past 9999.
/// Measured on HotSpot 25 under `Locale.ROOT`.
///
/// Kills Rust's `{:04}`, which counts the sign inside the width
/// (`-044-03-15` for 45 BC) and never emits a '+'.
#[test]
fn iso_year_signs_sit_outside_the_zero_pad() {
    assert_eq!(fmt_iso_year(-44, '-'), "-0044");
    assert_eq!(fmt_iso_year(-1, '-'), "-0001");
    assert_eq!(fmt_iso_year(0, '-'), "0000");
    assert_eq!(fmt_iso_year(1, '-'), "0001");
    assert_eq!(fmt_iso_year(2023, '-'), "2023");
    assert_eq!(fmt_iso_year(9999, '-'), "9999", "no sign at the boundary");
    assert_eq!(fmt_iso_year(10000, '-'), "+10000", "the '+' starts here");
    assert_eq!(fmt_iso_year(12345, '-'), "+12345");
    // Rust's own render is the mutant, and it differs on three of these.
    assert_ne!(fmt_iso_year(-44, '-'), format!("{:04}", -44));
    assert_ne!(fmt_iso_year(12345, '-'), format!("{:04}", 12345));
}

/// F38 — `%tF`'s minus is the LOCALE'S, and it is the only localized minus
/// in `java.util.Formatter`.
///
/// Measured on HotSpot 25 under `lt-LT` (`getMinusSign()` = U+2212), dumped
/// as UTF-16 code units:
///
/// | conversion | argument | units |
/// |---|---|---|
/// | `%tF` | `LocalDate.of(-44,3,15)` | `2212 0030 0030 0034 0034 002D …` |
/// | `%tF` | `LocalDate.of(12345,3,4)` | `002B 0031 …` — ASCII '+' |
/// | `%tF` | same, `(Locale) null` | `002D 0030 0030 0034 0034 …` |
/// | `%d` | `-5` | `002D 0035` |
///
/// Mutants this kills:
///  * an ASCII `'-'` — the U+2212 row;
///  * localizing the `'+'` too — the 12345 row (the JDK writes that one as
///    a literal in the same block);
///  * localizing the DATE SEPARATORS as well, which a
///    `replace('-', minus)` over the composed field would do — the
///    `%tF`-shaped row below;
///  * routing the explicit-null locale through a lookup —
///    [`FmtSymbols::default`]'s minus must stay U+002D.
#[test]
fn iso_year_minus_is_the_locales_and_nothing_else_is() {
    assert_eq!(fmt_iso_year(-44, '\u{2212}'), "\u{2212}0044");
    assert_eq!(fmt_iso_year(-1, '\u{2212}'), "\u{2212}0001");
    // The '+' is a literal even when the minus is not.
    assert_eq!(fmt_iso_year(12345, '\u{2212}'), "+12345");
    // A non-negative year consults no symbol at all.
    assert_eq!(fmt_iso_year(2023, '\u{2212}'), "2023");
    assert_eq!(fmt_iso_year(0, '\u{2212}'), "0000");

    // The whole `%tF` field: exactly ONE unit changes, and the two date
    // separators stay ASCII.
    let field = format!("{}-{:02}-{:02}", fmt_iso_year(-44, '\u{2212}'), 3, 15);
    assert_eq!(field, "\u{2212}0044-03-15");
    assert_eq!(field.matches('-').count(), 2, "separators stay ASCII");

    // An explicit null locale is `getMinusSign(null) == '-'`.
    assert_eq!(FmtSymbols::default().minus, '-');
    // …and the digit substitution pass leaves the sign alone, whichever it
    // is. `ar-EG`'s zero digit with a U+2212 minus is not a real locale
    // pair, which is the point: the two symbols are independent.
    let ar = FmtSymbols {
        zero: '\u{0660}',
        ..FmtSymbols::default()
    };
    assert_eq!(
        fmt_localize_digits("\u{2212}0044-03-15", ar),
        "\u{2212}\u{0660}\u{0660}\u{0664}\u{0664}-\u{0660}\u{0663}-\u{0661}\u{0665}"
    );
}

/// F38 — `%tY`/`%ty`/`%tC`, and the year slots of `%tD` and `%tc`, render
/// the ERA-RELATIVE year; only `%tF`'s `TemporalAccessor` arm renders the
/// proleptic one.
///
/// Measured on HotSpot 25, `Locale.ROOT`, `-Duser.timezone=Asia/Kolkata`.
/// The `Calendar` rows are the SAME INSTANT as the `LocalDate` rows
/// (`-63549548877000L`), which is what makes the `%tF` column the only
/// disagreement between the two printers.
///
/// Mutants this kills:
///  * rendering the proleptic year — `%tY` of 45 BC would be `-044`;
///  * `-year` instead of `1 - year` — proleptic 0 is 1 BC, not 0 BC, so
///    `LocalDate.of(0,…)` is `0001` and `LocalDate.of(-1,…)` is `0002`;
///  * `rem_euclid(100)` on the proleptic year for `%ty`/`%tD` — that gives
///    `56`, not `45`;
///  * applying the era mapping to `%tF` as well — 45 BC is `-0044-03-15`
///    on the `java.time` side;
///  * applying `fmt_iso_year` to the `Calendar` printer — that puts a '+'
///    on `12345-03-04`, which the JDK does not write there, and a '-' on a
///    BC date whose `Calendar.YEAR` is positive.
#[test]
fn year_fields_are_era_relative_on_both_printers() {
    // The mapping itself, against `ChronoField.YEAR_OF_ERA` as measured.
    fn year_of_era(year: i64) -> i64 {
        if year <= 0 {
            1 - year
        } else {
            year
        }
    }
    for (proleptic, want) in [(-44i64, 45i64), (-1, 2), (0, 1), (1, 1), (12345, 12345)] {
        assert_eq!(year_of_era(proleptic), want, "YEAR_OF_ERA of {proleptic}");
    }
    assert_eq!(format!("{:04}", year_of_era(-44)), "0045", "%tY of 45 BC");
    assert_eq!(format!("{:02}", year_of_era(-44) % 100), "45", "%ty");
    assert_eq!(format!("{:02}", year_of_era(-44) / 100), "00", "%tC");
    assert_eq!(format!("{:02}", year_of_era(12345) / 100), "123", "%tC AD");
    assert_eq!(format!("{:02}", year_of_era(12345) % 100), "45", "%ty AD");

    // `%tD`'s year slot is `%ty`'s, not the proleptic remainder.
    assert_eq!(
        format!("{:02}/{:02}/{:02}", 3, 15, year_of_era(-44) % 100),
        "03/15/45"
    );
    assert_ne!(
        format!("{:02}", (-44i64).rem_euclid(100)),
        "45",
        "the old proleptic remainder was 56"
    );

    // `%tF`: the two printers, at the SAME instant.
    assert_eq!(
        format!("{}-{:02}-{:02}", fmt_iso_year(-44, '-'), 3, 15),
        "-0044-03-15",
        "TemporalAccessor arm: proleptic, signed"
    );
    assert_eq!(
        format!("{:04}-{:02}-{:02}", year_of_era(-44), 3, 15),
        "0045-03-15",
        "Calendar arm: era-relative, unsigned"
    );
    assert_eq!(
        format!("{:04}-{:02}-{:02}", year_of_era(12345), 3, 4),
        "12345-03-04",
        "Calendar arm writes no '+' past 9999"
    );
    assert_ne!(
        format!("{}-{:02}-{:02}", fmt_iso_year(12345, '-'), 3, 4),
        "12345-03-04",
        "…while the TemporalAccessor arm does"
    );
}

/// `%n` is the platform separator, and the two `%n` arms (the bare
/// short circuit and the decorated one, ~440 lines apart) must agree.
#[test]
fn line_separator_units_are_the_platform_separator() {
    let units = fmt_line_separator_units();
    if cfg!(windows) {
        assert_eq!(units, vec![0x000D, 0x000A]);
    } else {
        assert_eq!(units, vec![0x000A]);
    }
}

/// The parser's per-UNIT `char` view must be index-aligned with the format
/// string's units, and must not turn any character the parser tests for
/// into something else.
///
/// Kills the `str::chars()` collect this replaced: there a surrogate PAIR
/// is ONE `char`, so every index after it is off by one and the literal
/// push emits the wrong unit.
#[test]
fn parser_char_view_is_index_aligned_with_units() {
    // Rust cannot put a lone surrogate in a literal, so it is appended.
    let units: Vec<u16> = "%5.1s\u{1F600}x"
        .encode_utf16()
        .chain(std::iter::once(0xD800))
        .collect();
    let chars: Vec<char> = units
        .iter()
        .map(|&u| char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'))
        .collect();
    assert_eq!(chars.len(), units.len(), "one char per code UNIT");
    assert_eq!(chars[0], '%');
    assert_eq!(chars[1], '5');
    assert_eq!(chars[2], '.');
    assert_eq!(chars[3], '1');
    assert_eq!(chars[4], 's');
    // The emoji occupies TWO slots now, so 'x' sits at 7, not 6.
    assert_eq!(chars[7], 'x');
    assert_eq!(units[7], 0x0078);
    // A surrogate — half of a pair or lone — is none of the characters the
    // parser tests for, so it falls through to the literal branch.
    for (i, c) in chars.iter().enumerate() {
        if (0xD800..=0xDFFF).contains(&units[i]) {
            assert_eq!(*c, '\u{FFFD}');
            assert!(!c.is_ascii_digit() && !c.is_ascii_alphabetic() && *c != '%');
        }
    }
}

/// `has_unpaired_surrogate` is the gate that decides whether a result takes
/// the lossless units path, so a false negative silently restores the bug.
#[test]
fn unpaired_surrogate_gate_is_exact() {
    assert!(!has_unpaired_surrogate(&[0x0061, 0x0062]));
    let pair: Vec<u16> = "\u{1F600}".encode_utf16().collect();
    assert!(!has_unpaired_surrogate(&pair));
    assert!(has_unpaired_surrogate(&[0xD800]));
    assert!(has_unpaired_surrogate(&[0xDC00]));
    assert!(has_unpaired_surrogate(&[0xD800, 0x0061]));
    // A LOW followed by a HIGH is two lone surrogates, not a pair.
    assert!(has_unpaired_surrogate(&[0xDC00, 0xD800]));
    // A trailing high surrogate with nothing after it.
    assert!(has_unpaired_surrogate(&[0x0061, 0xD83D]));
}
