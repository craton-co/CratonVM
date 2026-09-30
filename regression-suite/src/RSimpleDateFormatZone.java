import java.text.DateFormat;
import java.text.FieldPosition;
import java.text.ParseException;
import java.text.SimpleDateFormat;
import java.util.Calendar;
import java.util.Date;
import java.util.Locale;
import java.util.SimpleTimeZone;
import java.util.TimeZone;

/**
 * {@code SimpleDateFormat} over an application-built {@code java.util.SimpleTimeZone}: the FORMAT
 * half of the defect {@code RSimpleTimeZoneRaw} covers on the accessor half.
 *
 * <h2>The defect this file is the executable form of</h2>
 *
 * <p>Diagnosed in
 * docs/known-issues/jdk-only/C12-1-simpletimezone-the-trap-and-the-second-site.md section 3.
 * {@code native-builtins/src/date_format_fast.rs} registers an intrinsic for
 * {@code java/text/DateFormat.format(Ljava/util/Date;)Ljava/lang/String;} and computes the zone
 * offset in Rust WITHOUT dispatching:
 *
 * <pre>
 *   let vm_implemented = Some(zone_class) == sl.zoneinfo_class
 *       || Some(zone_class) == sl.simple_tz_class      // &lt;- this arm
 *       || Some(zone_class) == sl.timezone_class;
 *   let offset_ms = if vm_implemented {
 *       zone_rules_cached(ctx, sl, zone)?              // reads the receiver's ID FIELD
 *   } else {
 *       ctx.invoke_virtual(zone, "getOffset", "(J)I", ...)     // the correct arm
 *   };
 * </pre>
 *
 * <p>{@code zone_rules_cached} resolves the receiver's {@code ID} field against tzdb. For a
 * {@code sun.util.calendar.ZoneInfo} that is right: its id IS the zone. For a
 * {@code java.util.SimpleTimeZone} it is wrong, because the id is by contract an opaque LABEL and
 * the offset is the {@code rawOffset} the constructor stored. So
 * {@code new SimpleTimeZone(0, "America/Sao_Paulo")} formats an instant at {@code -03:00} on
 * CratonVM and at {@code +00:00} on HotSpot.
 *
 * <p><b>It is the FORMAT path only.</b> {@code parse} is not intercepted, so it reaches
 * {@code GregorianCalendar} then {@code TimeZone.getOffset} on the receiver and finally the real
 * {@code SimpleTimeZone} bytecode. The two directions are therefore fixed by DIFFERENT changes
 * (C12-1 section 1 landed the parse half; C12-1 section 5 is the still-unapplied format half), and
 * every block below says which direction it targets so a half-landed fix cannot read as done.
 *
 * <h2>THE VACUITY TRAP -- read this before adding a row</h2>
 *
 * <p>A {@code rawOffset} that happens to EQUAL the named zone's true offset at the chosen instant
 * proves NOTHING: both implementations answer the same number and the row passes on a broken VM.
 * {@code (-18000000, "America/New_York")} looks non-zero and is vacuous; so is {@code (0, "UTC")}
 * and the near-universal real-world idiom {@code (0, "Z")}. This cost an earlier lane real time, so
 * the trap is closed MECHANICALLY rather than by care: every row-driven block asserts
 * {@code TimeZone.getTimeZone(id).getOffset(instant) != rawOffset} as its own first check. A row
 * that becomes vacuous, because someone edits a number or because a tzdb update moves a zone, fails
 * loudly instead of going quietly green.
 *
 * <p>Every zone id here is FIXED. The host default zone is never read, so the expected strings
 * reproduce on any machine.
 *
 * <h2>The measured anchor</h2>
 *
 * <p>The bc-java parse idiom that originally found this --
 * {@code SimpleDateFormat("yyyyMMddHHmmss")} with
 * {@code setTimeZone(new SimpleTimeZone(0, "America/Sao_Paulo"))} parsing {@code 20020122122220} --
 * is skewed by <b>7,200,000 ms</b> against the tzdb answer, NOT the 10,800,000 an earlier record
 * predicted: Sao Paulo was observing daylight saving on 2002-01-22, and the tzdb path applies the
 * transition in force AT THE INSTANT rather than the zone's standard offset. {@link #parse()}
 * asserts that number directly, so the corrected figure is pinned by a test and cannot regress to
 * the guess. The corollary is in that record too and matters for anyone adding rows: the skew is a
 * time series, not a constant, so a block asserting a fixed delta passes or fails on the date it
 * picks.
 *
 * <h2>Why the three routes are asked separately</h2>
 *
 * <p>The intrinsic is registered for ONE descriptor. {@code SimpleDateFormat} does not declare
 * {@code format(Date)} -- it is {@code DateFormat}'s -- so the receiver-class lookup misses, the
 * superclass climb runs, and the intrinsic wins. {@code format(Object)} and
 * {@code format(Date, StringBuffer, FieldPosition)} are different descriptors and run real
 * bytecode. {@link #routes()} asserts the three agree WITH EACH OTHER, which HotSpot satisfies
 * trivially and a VM with one wrong arm cannot: it is a route discriminator that needs no expected
 * value at all, so it stays sharp even if every string in this file is edited.
 *
 * <h2>Mode independence</h2>
 *
 * <p>The intrinsic is registered in both arms and the accessor half of this defect reproduced on
 * {@code --jdk-only} AND {@code --real-jdk} (C6-2 section 1), so this is a default-mode
 * compatibility concern and belongs in {@code CORE_CLASSES} beside {@code RSimpleTimeZoneRaw}, not
 * in the {@code RJdk*} policy corpus.
 */
public class RSimpleDateFormatZone {
    static int checks;

    static int mark;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Close a block and assert its own size. A block that silently loses rows to an edit still
     * prints {@code CK}, and a hard-coded number nobody re-derives is how a shrinking vector goes
     * unnoticed. Mismatch is a failure, not a warning.
     */
    static void sectionEnd(String name, int expected) {
        int n = checks - mark;
        mark = checks;
        if (n != expected) {
            throw new AssertionError(
                    "block " + name + " ran " + n + " checks, header says " + expected);
        }
        System.out.println("CK RSimpleDateFormatZone " + name + "=" + n);
    }

    /** An observable on a line the harness's extract() keeps, so it reaches the cross-VM diff. */
    static void ob(String key, String v) {
        System.out.println("CK RSimpleDateFormatZone " + key + "=" + v);
    }

    /**
     * THE CALL UNDER TEST, in one place.
     *
     * <p>Every format in this file goes through here, and the mutation check replaces exactly this
     * body with the defect's own computation: resolve the receiver's ID against tzdb and format
     * with THAT zone, which is what {@code zone_rules_cached} does one layer below dispatch. One
     * seam, so the mutant differs from the fixture by three lines and cannot accidentally test
     * something else.
     */
    static String fmt(SimpleDateFormat f, Date d) {
        return f.format(d);
    }

    /**
     * The same call from a {@code DateFormat}-typed call site. A SEPARATE seam because the static
     * type is the whole question it asks: dispatch must key on the receiver's runtime class, so
     * this must answer identically to {@link #fmt}. The mutation check replaces both bodies.
     */
    static String fmtBase(DateFormat f, Date d) {
        return f.format(d);
    }

    /** The parse half, likewise in one place. Not intercepted today; see the class comment. */
    static long prs(SimpleDateFormat f, String s) throws ParseException {
        return f.parse(s).getTime();
    }

    // Fixed instants. 2021-01-15T12:00:00Z and 2021-07-15T12:00:00Z, one in each hemisphere's
    // daylight-saving season, so no row can be right for the wrong reason.
    static final long JAN = 1610712000000L;
    static final long JUL = 1626350400000L;

    static final String PAT = "yyyy-MM-dd HH:mm:ss Z";
    /** No zone field in the text. See {@link #roundtrip()} for why that is the whole point. */
    static final String BARE = "yyyy-MM-dd HH:mm:ss";

    // Ten (rawOffset, id, instant) triples. Each pairs a rawOffset with a real zone id whose true
    // offset AT THAT INSTANT differs from it -- asserted, not assumed, in every block that uses
    // them. Measured on HotSpot 25.0.3+9-LTS; the "tzdb" column is what a VM that answers from the
    // ID field prints instead.
    //
    //   k  rawOffset   id                      instant  HotSpot                    tzdb-by-id
    //   0          0   America/Sao_Paulo       JAN      2021-01-15 12:00:00 +0000  ...09:00:00 -0300
    //   1          0   America/New_York        JAN      2021-01-15 12:00:00 +0000  ...07:00:00 -0500
    //   2   18000000   America/Sao_Paulo       JUL      2021-07-15 17:00:00 +0500  ...09:00:00 -0300
    //   3  -18000000   Europe/Berlin           JUL      2021-07-15 07:00:00 -0500  ...14:00:00 +0200
    //   4    3600000   Asia/Kolkata            JAN      2021-01-15 13:00:00 +0100  ...17:30:00 +0530
    //   5   -3600000   Asia/Tokyo              JUL      2021-07-15 11:00:00 -0100  ...21:00:00 +0900
    //   6   19800000   UTC                     JAN      2021-01-15 17:30:00 +0530  ...12:00:00 +0000
    //   7          0   Asia/Tokyo              JUL      2021-07-15 12:00:00 +0000  ...21:00:00 +0900
    //   8  -10800000   Pacific/Kiritimati      JAN      2021-01-15 09:00:00 -0300  2021-01-16 02:00 +1400
    //   9          0   Australia/Adelaide      JUL      2021-07-15 12:00:00 +0000  ...21:30:00 +0930
    //
    // Row 6 is deliberately the inverse of the classic vacuous idiom: `(0, "UTC")` is the row that
    // passes on a broken VM, so this file carries `(19800000, "UTC")` instead. Row 8's zone is
    // UTC+14, the largest offset in tzdb, so its wrong answer lands on a DIFFERENT CALENDAR DAY --
    // a formatter that only got the clock field wrong would still be caught by the date.
    static final int[] RAW = {
        0, 0, 18000000, -18000000, 3600000, -3600000, 19800000, 0, -10800000, 0,
    };
    static final String[] ID = {
        "America/Sao_Paulo", "America/New_York", "America/Sao_Paulo", "Europe/Berlin",
        "Asia/Kolkata", "Asia/Tokyo", "UTC", "Asia/Tokyo", "Pacific/Kiritimati",
        "Australia/Adelaide",
    };
    static final long[] AT = { JAN, JAN, JUL, JUL, JAN, JUL, JAN, JUL, JAN, JUL };

    /** MEASURED on HotSpot 25.0.3+9-LTS. Not remembered, not derived. */
    static final String[] FMT_EXPECT = {
        "2021-01-15 12:00:00 +0000",
        "2021-01-15 12:00:00 +0000",
        "2021-07-15 17:00:00 +0500",
        "2021-07-15 07:00:00 -0500",
        "2021-01-15 13:00:00 +0100",
        "2021-07-15 11:00:00 -0100",
        "2021-01-15 17:30:00 +0530",
        "2021-07-15 12:00:00 +0000",
        "2021-01-15 09:00:00 -0300",
        "2021-07-15 12:00:00 +0000",
    };

    /** The same instants under {@link #BARE}. MEASURED on HotSpot 25.0.3+9-LTS. */
    static final String[] BARE_EXPECT = {
        "2021-01-15 12:00:00",
        "2021-01-15 12:00:00",
        "2021-07-15 17:00:00",
        "2021-07-15 07:00:00",
        "2021-01-15 13:00:00",
        "2021-07-15 11:00:00",
        "2021-01-15 17:30:00",
        "2021-07-15 12:00:00",
        "2021-01-15 09:00:00",
        "2021-07-15 12:00:00",
    };

    static SimpleDateFormat sdf(String pattern, int raw, String id) {
        SimpleDateFormat f = new SimpleDateFormat(pattern, Locale.US);
        f.setTimeZone(new SimpleTimeZone(raw, id));
        return f;
    }

    /**
     * The anti-vacuity predicate, stated once. Fails the run rather than warning: a row whose
     * rawOffset equals the zone's true offset at the instant is a row that passes on the broken VM,
     * and it must not be allowed to sit in this file looking like coverage.
     */
    static void notVacuous(String row, int raw, String id, long at) {
        int trueOff = TimeZone.getTimeZone(id).getOffset(at);
        check(trueOff != raw,
                row + ": VACUOUS VECTOR -- rawOffset " + raw + " equals " + id + "'s true offset "
                        + trueOff + " at instant " + at + ", so both the correct and the"
                        + " ID-resolved implementation answer the same thing. Pick another"
                        + " (rawOffset, id) pair.");
    }

    // -----------------------------------------------------------------------
    // 1. control -- NEGATIVE CONTROL. Targets neither direction; must be green
    //    before AND after the fix.
    //
    // `TimeZone.getTimeZone(id)` returns a zone whose id IS the zone, and for
    // that receiver resolving the offset from the ID field is CORRECT. C12-1's
    // nomination narrows the fast arm to exactly this population, so if this
    // block ever goes red the fix over-reached. It is also the block that
    // proves the formatter itself works: without it, a red in `fmtdate` could
    // be a broken `SimpleDateFormat` rather than a broken zone lookup.
    // -----------------------------------------------------------------------
    static void control() {
        check(TimeZone.getTimeZone("America/Sao_Paulo").getRawOffset() == -10800000,
                "getTimeZone(\"America/Sao_Paulo\").getRawOffset() must be -10800000");
        check(TimeZone.getTimeZone("America/New_York").getOffset(JUL) == -14400000,
                "getTimeZone(\"America/New_York\").getOffset(JUL) must be -14400000 (DST)");
        check(TimeZone.getTimeZone("Europe/Berlin").getOffset(JUL) == 7200000,
                "getTimeZone(\"Europe/Berlin\").getOffset(JUL) must be 7200000 (CEST)");
        check(TimeZone.getTimeZone("America/New_York").getOffset(JAN) == -18000000,
                "getTimeZone(\"America/New_York\").getOffset(JAN) must be -18000000 (EST)");

        SimpleDateFormat a = new SimpleDateFormat(PAT, Locale.US);
        a.setTimeZone(TimeZone.getTimeZone("Asia/Kolkata"));
        String sa = fmt(a, new Date(JAN));
        ob("control-kolkata", sa);
        check("2021-01-15 17:30:00 +0530".equals(sa),
                "a ZoneInfo receiver must format JAN as \"2021-01-15 17:30:00 +0530\", got " + sa);

        SimpleDateFormat b = new SimpleDateFormat(PAT, Locale.US);
        b.setTimeZone(TimeZone.getTimeZone("GMT+05:30"));
        String sb = fmt(b, new Date(JAN));
        ob("control-gmtoffset", sb);
        check("2021-01-15 17:30:00 +0530".equals(sb),
                "a custom GMT+05:30 zone must format JAN as \"2021-01-15 17:30:00 +0530\", got "
                        + sb);

        SimpleDateFormat c = new SimpleDateFormat(PAT, Locale.US);
        c.setTimeZone(TimeZone.getTimeZone("UTC"));
        String sc = fmt(c, new Date(JAN));
        ob("control-utc", sc);
        check("2021-01-15 12:00:00 +0000".equals(sc),
                "a UTC ZoneInfo must format JAN as \"2021-01-15 12:00:00 +0000\", got " + sc);

        SimpleDateFormat d = new SimpleDateFormat(PAT, Locale.US);
        d.setTimeZone(TimeZone.getTimeZone("America/New_York"));
        String sd = fmt(d, new Date(JUL));
        ob("control-newyork-dst", sd);
        check("2021-07-15 08:00:00 -0400".equals(sd),
                "a ZoneInfo must apply the DST transition in force at the instant, got " + sd);

        sectionEnd("control", 8);
    }

    // -----------------------------------------------------------------------
    // 2. fmtdate -- TARGETS THE FORMAT PATH. This is the block C12-1 section 5
    //    fixes.
    //
    // `DateFormat.format(Date)` is the intercepted descriptor. Every row's
    // rawOffset contradicts its zone id, so on a VM that resolves the offset
    // from the ID field the formatted text is wrong in the clock field AND in
    // the `Z` field, and row 8 is additionally wrong in the DATE.
    // -----------------------------------------------------------------------
    static void fmtdate() {
        for (int k = 0; k < RAW.length; k++) {
            notVacuous("fmtdate[" + k + "]", RAW[k], ID[k], AT[k]);
            String s = fmt(sdf(PAT, RAW[k], ID[k]), new Date(AT[k]));
            ob("fmt" + k, s);
            check(FMT_EXPECT[k].equals(s),
                    "fmtdate[" + k + "] new SimpleTimeZone(" + RAW[k] + ", \"" + ID[k]
                            + "\").format(instant " + AT[k] + ") must be \"" + FMT_EXPECT[k]
                            + "\", got \"" + s + "\" -- the caller's rawOffset was ignored and the"
                            + " zone's ID was resolved against tzdb instead");
        }
        sectionEnd("fmtdate", 20);
    }

    // -----------------------------------------------------------------------
    // 3. routes -- TARGETS THE FORMAT PATH, with no expected value at all.
    //
    // Three descriptors, one contract:
    //   DateFormat.format(Date)Ljava/lang/String;                 <- INTERCEPTED
    //   DateFormat.format(Object)Ljava/lang/String;               <- real bytecode
    //   SimpleDateFormat.format(Date,StringBuffer,FieldPosition)  <- real bytecode, declared on
    //                                                                the receiver's own class so
    //                                                                the climb never runs
    // and a fourth call whose STATIC type is `DateFormat`, which must not change anything --
    // dispatch passes the RECEIVER's class name, never the call site's (C12-1 section 2).
    //
    // A VM whose intrinsic disagrees with its own bytecode fails here whatever the right answer
    // is, so this block survives every edit to the expected strings above. It is also the block
    // that tells a reader WHICH arm is wrong: `fmtdate` red + `routes` red means the intrinsic;
    // `fmtdate` red + `routes` green means both arms agree on a wrong answer, which is a
    // different defect and not this one.
    // -----------------------------------------------------------------------
    static void routes() {
        for (int k = 0; k < RAW.length; k++) {
            SimpleDateFormat f = sdf(PAT, RAW[k], ID[k]);
            Date d = new Date(AT[k]);
            String viaDate = fmt(f, d);
            String viaObject = f.format((Object) d);
            String viaBuffer = f.format(d, new StringBuffer(), new FieldPosition(0)).toString();
            DateFormat df = f;
            String viaStaticBase = fmtBase(df, d);
            check(viaDate.equals(viaObject),
                    "routes[" + k + "]: format(Date)=\"" + viaDate + "\" but format(Object)=\""
                            + viaObject + "\" -- the same formatter answered two different things,"
                            + " so one of the two dispatch routes is not running the class library");
            check(viaDate.equals(viaBuffer),
                    "routes[" + k + "]: format(Date)=\"" + viaDate
                            + "\" but format(Date,StringBuffer,FieldPosition)=\"" + viaBuffer
                            + "\"");
            check(viaDate.equals(viaStaticBase),
                    "routes[" + k + "]: the answer changed with the CALL SITE's static type ("
                            + viaDate + " vs " + viaStaticBase + "); dispatch must key on the"
                            + " receiver's runtime class");
        }
        sectionEnd("routes", 30);
    }

    // -----------------------------------------------------------------------
    // 4. roundtrip -- TARGETS BOTH DIRECTIONS AT ONCE, and closes a second
    //    vacuity trap that is easy to fall into.
    //
    // A round trip through PAT would prove nothing: `Z` writes the offset into
    // the text, so parse reads the offset back out of the STRING and never
    // consults the zone. The round trip stays green with a completely broken
    // format. So this block uses BARE, which has no zone field: format takes
    // the offset from the zone, parse takes it from the zone, and only a VM
    // where BOTH agree round-trips.
    //
    // That is exactly the half-landed state C12-1 warns about: with section 1
    // landed and section 5 not, parse answers from the real `rawOffset` while
    // format answers from tzdb, and the two disagree by
    // (trueOffset - rawOffset). The literal string check beside it says which
    // half moved.
    // -----------------------------------------------------------------------
    static void roundtrip() throws ParseException {
        for (int k = 0; k < RAW.length; k++) {
            notVacuous("roundtrip[" + k + "]", RAW[k], ID[k], AT[k]);
            SimpleDateFormat f = sdf(BARE, RAW[k], ID[k]);
            String s = fmt(f, new Date(AT[k]));
            ob("bare" + k, s);
            check(BARE_EXPECT[k].equals(s),
                    "roundtrip[" + k + "] zone-less format of instant " + AT[k]
                            + " under SimpleTimeZone(" + RAW[k] + ", \"" + ID[k] + "\") must be \""
                            + BARE_EXPECT[k] + "\", got \"" + s + "\"");
            long back = prs(f, s);
            check(back == AT[k],
                    "roundtrip[" + k + "]: format then parse under the SAME zone must return the"
                            + " original instant " + AT[k] + ", got " + back + " (delta "
                            + (back - AT[k]) + " ms) -- format and parse disagree about this zone's"
                            + " offset, which is the signature of a half-landed fix");
        }
        sectionEnd("roundtrip", 30);
    }

    // -----------------------------------------------------------------------
    // 5. parse -- TARGETS THE PARSE PATH, and pins the measured anchor.
    //
    // The bc-java idiom that found the whole family. C12-1 section 1 fixed this
    // direction by unregistering the four tzdb natives from
    // `java/util/SimpleTimeZone`; if it regresses, this block is where it shows
    // and `fmtdate` will not tell you.
    //
    // The last two checks pin the CORRECTED skew. An earlier record predicted
    // 10,800,000 ms (the zone's standard offset); the measured figure is
    // 7,200,000, because Sao Paulo was on daylight saving on 2002-01-22 and the
    // tzdb path applies the transition in force at the instant. Both numbers
    // are asserted -- the right one as the answer, the wrong one as an explicit
    // negative -- so nobody re-derives the guess from the standard offset.
    // -----------------------------------------------------------------------
    static void parse() throws ParseException {
        String[] text = { "20020122122220", "20020122122220", "20020122122220", "20210715120000" };
        int[] raw = { 0, 18000000, -18000000, 3600000 };
        String[] id = {
            "America/Sao_Paulo", "America/New_York", "Europe/Berlin", "Asia/Kolkata",
        };
        long[] at = { 1011702140000L, 1011702140000L, 1011702140000L, JUL };
        long[] want = { 1011702140000L, 1011684140000L, 1011720140000L, 1626346800000L };

        for (int k = 0; k < text.length; k++) {
            notVacuous("parse[" + k + "]", raw[k], id[k], at[k]);
            SimpleDateFormat f = new SimpleDateFormat("yyyyMMddHHmmss", Locale.US);
            f.setTimeZone(new SimpleTimeZone(raw[k], id[k]));
            long got = prs(f, text[k]);
            ob("parse" + k, Long.toString(got));
            check(got == want[k],
                    "parse[" + k + "] \"" + text[k] + "\" under SimpleTimeZone(" + raw[k] + ", \""
                            + id[k] + "\") must be " + want[k] + ", got " + got + " (delta "
                            + (got - want[k]) + " ms)");
        }

        // The anchor. The tzdb side is COMPUTED rather than written as a literal, so the number
        // stays honest if tzdb moves: the mutant answer is what `TimeZone.getTimeZone(id)`
        // produces, which is the same computation `zone_rules_cached` performs.
        SimpleDateFormat m = new SimpleDateFormat("yyyyMMddHHmmss", Locale.US);
        m.setTimeZone(TimeZone.getTimeZone("America/Sao_Paulo"));
        long tzdb = prs(m, "20020122122220");
        long skew = tzdb - 1011702140000L;
        ob("anchor-skew", Long.toString(skew));
        check(skew == 7200000L,
                "the Sao Paulo skew on 2002-01-22 must be 7,200,000 ms -- that date is inside"
                        + " Brazil's then-active daylight saving, so the tzdb path applies UTC-02:00"
                        + " for the instant; got " + skew);
        check(skew != 10800000L,
                "the skew must NOT be 10,800,000 ms (the zone's STANDARD offset). An earlier"
                        + " record predicted that figure and measurement corrected it; this check"
                        + " exists so the guess cannot come back");

        sectionEnd("parse", 10);
    }

    // -----------------------------------------------------------------------
    // 6. dstrule -- the ELEVEN-argument constructor, on the format path.
    //
    // "The caller's explicit rule is discarded in favour of one resolved from
    // the id" is not confined to the two-arg rawOffset: the longer constructors
    // lose their DST rule the same way (C6-2 section B). A real US rule on
    // rawOffset -05:00, labelled "UTC", is the sharpest single object in this
    // file. The label resolves to a zone with NO daylight saving, so a VM that
    // answers from the label formats both instants five hours away from the
    // truth and prints the SAME offset in January and July, where the correct
    // answer changes.
    // -----------------------------------------------------------------------
    static void dstrule() {
        SimpleTimeZone z = new SimpleTimeZone(-18000000, "UTC",
                Calendar.MARCH, 8, -Calendar.SUNDAY, 7200000,
                Calendar.NOVEMBER, 1, -Calendar.SUNDAY, 7200000,
                3600000);
        check(TimeZone.getTimeZone("UTC").getOffset(JAN) != -18000000,
                "dstrule: VACUOUS VECTOR -- the \"UTC\" label must not resolve to -18000000");
        check(z.useDaylightTime(), "dstrule: useDaylightTime() must be true -- a rule was supplied");
        check(z.getRawOffset() == -18000000, "dstrule: getRawOffset() must be the argument");
        check(z.getDSTSavings() == 3600000, "dstrule: getDSTSavings() must be 3600000");
        check(z.getOffset(JAN) == -18000000, "dstrule: getOffset(JAN) must be -18000000 (standard)");
        check(z.getOffset(JUL) == -14400000, "dstrule: getOffset(JUL) must be -14400000 (daylight)");
        check(!z.inDaylightTime(new Date(JAN)), "dstrule: inDaylightTime(JAN) must be false");
        check(z.inDaylightTime(new Date(JUL)), "dstrule: inDaylightTime(JUL) must be true");

        SimpleDateFormat f = new SimpleDateFormat(PAT, Locale.US);
        f.setTimeZone(z);
        String jan = fmt(f, new Date(JAN));
        String jul = fmt(f, new Date(JUL));
        ob("dstrule-jan", jan);
        ob("dstrule-jul", jul);
        check("2021-01-15 07:00:00 -0500".equals(jan),
                "dstrule: format(JAN) must be \"2021-01-15 07:00:00 -0500\", got \"" + jan + "\"");
        check("2021-07-15 08:00:00 -0400".equals(jul),
                "dstrule: format(JUL) must be \"2021-07-15 08:00:00 -0400\", got \"" + jul + "\"");
        check(!jan.substring(20).equals(jul.substring(20)),
                "dstrule: the printed offset must DIFFER between January and July -- a zone whose"
                        + " offset is resolved from the \"UTC\" label prints the same one twice"
                        + " (jan=" + jan + ", jul=" + jul + ")");

        sectionEnd("dstrule", 11);
    }

    // -----------------------------------------------------------------------
    // 7. memo -- TARGETS THE FORMAT PATH THROUGH THE PER-FORMATTER MEMO.
    //
    // The other format blocks cannot see this defect: a fresh formatter's
    // first format declines the VM's fast path (its `zeroDigit` field is not
    // armed yet) and the second is cross-checked against bytecode, which
    // corrects it. This block defeats both. It warms ONE formatter under a
    // zone the VM answers correctly for, so the shape is verified, then swaps
    // in a SimpleTimeZone with the SAME offset and a contradicting id: the
    // shape key is unchanged, the memo hits, and no cross-check runs. Removing
    // the two warm-up formats makes this block green on a broken VM, which is
    // its mutation check.
    //
    // The fast path memoizes verified shapes keyed by (formatter, shape) and
    // `setTimeZone` invalidates NOTHING, so a shape verified under one zone is
    // then SERVED under a different one with no cross-check. Both zones here
    // carry the same +03:00 offset so the shape key cannot change; only the
    // ANSWER may, and only on a VM that resolves the id instead of reading the
    // caller's rawOffset. Tokyo's true offset (32,400,000) differs from the
    // 10,800,000 supplied, which is what makes the row non-vacuous -- asserted
    // by notVacuous rather than assumed.
    // -----------------------------------------------------------------------
    static void memo() {
        SimpleDateFormat f = new SimpleDateFormat(PAT, Locale.US);
        f.setTimeZone(TimeZone.getTimeZone("Etc/GMT-3"));
        check(TimeZone.getTimeZone("Etc/GMT-3").getOffset(JAN) == 10800000,
                "memo: Etc/GMT-3 must be +03:00 at JAN");
        String warm1 = fmt(f, new Date(JAN));
        String warm2 = fmt(f, new Date(JAN));
        check("2021-01-15 15:00:00 +0300".equals(warm1), "memo: warm-up 1, got " + warm1);
        check("2021-01-15 15:00:00 +0300".equals(warm2), "memo: warm-up 2, got " + warm2);
        notVacuous("memo", 10800000, "Asia/Tokyo", JAN);
        f.setTimeZone(new SimpleTimeZone(10800000, "Asia/Tokyo"));
        // Second vacuity screen, and the reason this block is 6 checks rather than the 5 the
        // nomination's body contained: if setTimeZone silently failed to take, the assertion
        // below would pass for the wrong reason -- the formatter would still be holding
        // Etc/GMT-3, which answers +0300 correctly on every VM.
        check(f.getTimeZone().getRawOffset() == 10800000,
                "memo: setTimeZone must have TAKEN -- the formatter's zone must now report"
                        + " rawOffset 10800000, got " + f.getTimeZone().getRawOffset());
        String after = fmt(f, new Date(JAN));
        ob("memo-after-zone-swap", after);
        check("2021-01-15 15:00:00 +0300".equals(after),
                "memo: after setTimeZone(new SimpleTimeZone(10800000, \"Asia/Tokyo\")) the SAME"
                        + " formatter must still answer from the caller's rawOffset, got \"" + after
                        + "\" -- a per-(formatter, shape) memo served a zone it never checked");
        sectionEnd("memo", 6);
    }

    static final String[] FAMILIES = {
        "control", "fmtdate", "routes", "roundtrip", "parse", "dstrule", "memo",
    };

    static void runFamily(String name) throws ParseException {
        if ("control".equals(name)) {
            control();
        } else if ("fmtdate".equals(name)) {
            fmtdate();
        } else if ("routes".equals(name)) {
            routes();
        } else if ("roundtrip".equals(name)) {
            roundtrip();
        } else if ("parse".equals(name)) {
            parse();
        } else if ("dstrule".equals(name)) {
            dstrule();
        } else if ("memo".equals(name)) {
            memo();
        } else {
            throw new AssertionError("unknown family: " + name);
        }
    }

    public static void main(String[] args) throws ParseException {
        String only = null;
        for (int k = 0; k < args.length; k++) {
            if (args[k].startsWith("--only=")) {
                only = args[k].substring("--only=".length());
            } else if ("--list".equals(args[k])) {
                for (int j = 0; j < FAMILIES.length; j++) {
                    System.out.println("CK RSimpleDateFormatZone family=" + FAMILIES[j]);
                }
                return;
            }
        }
        if (only == null) {
            for (int k = 0; k < FAMILIES.length; k++) {
                runFamily(FAMILIES[k]);
            }
        } else {
            System.out.println("CK RSimpleDateFormatZone only=" + only);
            runFamily(only);
        }
        System.out.println("CK RSimpleDateFormatZone checks=" + checks);
        System.out.println("PASS RSimpleDateFormatZone (" + checks + " checks)");
    }
}
