import java.util.Calendar;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.SimpleTimeZone;
import java.util.TimeZone;

/**
 * {@code java.util.SimpleTimeZone}'s ID is an OPAQUE LABEL, and
 * {@code TimeZone.getTimeZone(id)}'s DST family must answer from the database.
 *
 * <h2>Why this file was rewritten (2026-08-17)</h2>
 *
 * The previous revision was MEASURABLY BLIND, in two independent ways, and the
 * second one survived the first being fixed:
 *
 * <ol>
 *   <li>It reported through {@code throw new AssertionError(...)}. On CratonVM
 *       it died inside {@code realZonesStillResolve()} at
 *       {@code New_York inDaylightTime(JUL)} — BEFORE printing anything — so
 *       {@code extract()} recovered <b>0 lines</b> and the cross-VM diff had
 *       nothing at all to compare.</li>
 *   <li>Even on the green path it printed exactly two lines, and BOTH are
 *       constants the fixture computes about itself:
 *       {@code CK RSimpleTimeZoneRaw checks=104} and
 *       {@code PASS RSimpleTimeZoneRaw (104 checks)}. The cross-VM diff
 *       therefore compared {@code 104} against {@code 104}. <b>No answer the VM
 *       gave ever reached the comparison.</b> That is harness-guard.sh's G2
 *       exactly: "a constant always matches itself".</li>
 * </ol>
 *
 * So the shape changed, per the dialect harness-guard.sh states:
 * <b>every assertion publishes the VM's OWN answer on a {@code CK} line before
 * it is compared</b>, nothing throws, failures are counted rather than fatal,
 * and the {@code PASS} banner is emitted only when the count of failures is
 * zero. A run that diverges now prints 300-odd lines of the divergence instead
 * of dying on the first one.
 *
 * <h2>What is asserted, and why not less</h2>
 *
 * <h3>1. SimpleTimeZone: the id contributes NO offset</h3>
 *
 * {@code new SimpleTimeZone(rawOffset, ID)} takes its offset from
 * {@code rawOffset}. {@code ID} is never looked up in the timezone database. So
 * {@code new SimpleTimeZone(0, "America/Sao_Paulo")} is UTC that happens to be
 * NAMED {@code America/Sao_Paulo}.
 *
 * Two facts make a violation nearly invisible, and both would make a lazier
 * vector vacuous:
 *
 * <ul>
 *   <li>the overwhelmingly common idiom in real code is
 *       {@code new SimpleTimeZone(0, "Z")}. {@code "Z"} resolves to no zone, so
 *       a VM that wrongly resolves the label falls back to 0 and is
 *       ACCIDENTALLY RIGHT.</li>
 *   <li>on a UTC host the defect is entirely invisible, because the resolved
 *       offset and the passed {@code rawOffset} are both zero.</li>
 * </ul>
 *
 * So every zone id below is a FIXED literal, never the host default, and the
 * MISMATCHED block pairs a NON-ZERO {@code rawOffset} with a REAL zone id whose
 * true offset DIFFERS from it. {@code "Z"} and {@code "UTC"} appear only as
 * controls that must not move.
 *
 * {@code inDaylightTime(Date)} is the silent one: its real bytecode is
 * {@code getOffset(d.getTime()) != this.rawOffset} — it reads the FIELD while
 * calling {@code getOffset}. A zone built with NO daylight-saving rule can
 * therefore report that it IS in daylight time while {@code useDaylightTime()}
 * and {@code getDSTSavings()} keep saying there is no rule. That
 * self-contradictory triple is published row by row.
 *
 * The six-arg {@code getOffset(era,y,m,d,dow,ms)} is a DISCRIMINATOR: it is not
 * one of the registered native descriptors, so it runs real bytecode off the
 * real {@code rawOffset} field. It agreeing while the one-arg form disagrees is
 * what separates "the constructor stored the wrong value" from "the accessor is
 * hijacked".
 *
 * <h3>2. TimeZone.getTimeZone(id): the DST family</h3>
 *
 * The block the previous revision died in, widened from 2 zones to 32 and from
 * 3 observables to 8. {@code getRawOffset()} and {@code getOffset(long)} were
 * the only two members with native coverage; {@code getDSTSavings()},
 * {@code useDaylightTime()}, {@code observesDaylightTime()} and
 * {@code inDaylightTime(Date)} were not, and they are where the family
 * diverges.
 *
 * The zone list is chosen so that reading the answer off a single rule cannot
 * pass it:
 *
 * <ul>
 *   <li>{@code Australia/Lord_Howe} saves <b>1800000</b> ms, not 3600000 — the
 *       row that catches a hard-coded one-hour saving.</li>
 *   <li>{@code Antarctica/Troll} saves <b>7200000</b> ms.</li>
 *   <li>{@code America/Mexico_City} has {@code useDaylightTime()==false} today
 *       and {@code inDaylightTime(JUL 2021)==true} — DST was abolished in 2022,
 *       so the CURRENT rule and the HISTORICAL transition disagree. A VM that
 *       answers {@code inDaylightTime} from the current rule fails this row and
 *       only this row.</li>
 *   <li>{@code America/Godthab} has {@code rawOffset==-7200000} and
 *       {@code getOffset(JAN 2021)==-10800000}: same disagreement, in the
 *       standard offset rather than the saving.</li>
 *   <li>{@code Asia/Tehran} likewise: no current DST, but JUL 2021 was +04:30.
 *       </li>
 *   <li>southern-hemisphere zones ({@code Sydney}, {@code Santiago},
 *       {@code Auckland}, {@code Chatham}, {@code Lord_Howe}) invert JAN/JUL,
 *       so a hemisphere-blind rule cannot pass both halves.</li>
 *   <li>half-hour and three-quarter-hour standard offsets ({@code Kolkata},
 *       {@code Kathmandu}, {@code St_Johns}, {@code Eucla}, {@code Chatham})
 *       catch an hour-granular table.</li>
 *   <li>{@code Africa/Cairo} is in daylight time at NEITHER instant despite
 *       {@code useDaylightTime()==true} — DST returned in 2023, after both
 *       probe instants.</li>
 *   <li>{@code UTC}, {@code GMT}, {@code Etc/GMT+5}, {@code Etc/GMT-9} are
 *       fixed-offset controls, including the sign inversion the {@code Etc/}
 *       family famously carries.</li>
 * </ul>
 *
 * Every expectation in {@code DST_FAMILY} is MEASURED on HotSpot 25.0.3+9-LTS
 * (Temurin, Windows), 2026-08-17, by generating the literal table from a run of
 * the oracle rather than by hand.
 *
 * <h2>Determinism</h2>
 *
 * No host zone and no wall clock: the two instants are fixed epoch millis and
 * every id is a literal. {@code getAvailableIDs().length} is published because
 * it names the tzdb the rest of the table was measured against — if it moves,
 * the table is being read against a different database and the rows below are
 * the wrong question, not the wrong answer.
 *
 * <h2>Output contract</h2>
 *
 * Every line is {@code CK RSimpleTimeZoneRaw <key>=<value>} or the closing
 * {@code PASS RSimpleTimeZoneRaw (N checks)}; nothing is printed on any other
 * prefix, so nothing is deleted by {@code extract()} and guard G1 stays silent.
 * The count and the failure total are separate lines, as the dialect requires —
 * {@code CK <Class> checks=N fails=M} on ONE line makes harness_check_count
 * parse the count as the string {@code "N fails=M"} and silently no-ops G3.
 */
public class RSimpleTimeZoneRaw {
    static final String CLS = "RSimpleTimeZoneRaw";
    static int checks;
    static int fails;

    /** 2021-01-15T12:00:00Z -- northern winter, southern summer. */
    static final long JAN = 1610712000000L;
    /** 2021-07-15T12:00:00Z -- northern summer, southern winter. */
    static final long JUL = 1626350400000L;

    /** UTC-03:00, and with no daylight saving at all since 2019. */
    static final String SAO_PAULO = "America/Sao_Paulo";
    /** UTC-05:00 standard, UTC-04:00 in daylight time. */
    static final String NEW_YORK = "America/New_York";

    /**
     * id, rawOffset, dstSavings, useDaylightTime, observesDaylightTime,
     * inDaylightTime(JAN), inDaylightTime(JUL), getOffset(JAN), getOffset(JUL).
     * Booleans as 0/1. MEASURED on the oracle; see the class comment.
     */
    static final String[] DST_FAMILY = {
        "America/New_York,-18000000,3600000,1,1,0,1,-18000000,-14400000",
        "America/Sao_Paulo,-10800000,0,0,0,0,0,-10800000,-10800000",
        "Europe/London,0,3600000,1,1,0,1,0,3600000",
        "Australia/Lord_Howe,37800000,1800000,1,1,1,0,39600000,37800000",
        "Asia/Kolkata,19800000,0,0,0,0,0,19800000,19800000",
        "Asia/Tehran,12600000,0,0,0,0,1,12600000,16200000",
        "Pacific/Chatham,45900000,3600000,1,1,1,0,49500000,45900000",
        "Australia/Sydney,36000000,3600000,1,1,1,0,39600000,36000000",
        "Africa/Cairo,7200000,3600000,1,1,0,0,7200000,7200000",
        "America/Santiago,-14400000,3600000,1,1,1,0,-10800000,-14400000",
        "Asia/Tokyo,32400000,0,0,0,0,0,32400000,32400000",
        "America/Phoenix,-25200000,0,0,0,0,0,-25200000,-25200000",
        "Europe/Dublin,0,3600000,1,1,0,1,0,3600000",
        "Antarctica/Troll,0,7200000,1,1,0,1,0,7200000",
        "Asia/Kathmandu,20700000,0,0,0,0,0,20700000,20700000",
        "Pacific/Kiritimati,50400000,0,0,0,0,0,50400000,50400000",
        "America/St_Johns,-12600000,3600000,1,1,0,1,-12600000,-9000000",
        "Australia/Eucla,31500000,0,0,0,0,0,31500000,31500000",
        "Europe/Paris,3600000,3600000,1,1,0,1,3600000,7200000",
        "Europe/Moscow,10800000,0,0,0,0,0,10800000,10800000",
        "America/Havana,-18000000,3600000,1,1,0,1,-18000000,-14400000",
        "Asia/Jerusalem,7200000,3600000,1,1,0,1,7200000,10800000",
        "Pacific/Auckland,43200000,3600000,1,1,1,0,46800000,43200000",
        "America/Godthab,-7200000,3600000,1,1,0,1,-10800000,-7200000",
        "Asia/Gaza,7200000,3600000,1,1,0,1,7200000,10800000",
        "America/Mexico_City,-21600000,0,0,0,0,1,-21600000,-18000000",
        "Europe/Lisbon,0,3600000,1,1,0,1,0,3600000",
        "Atlantic/Azores,-3600000,3600000,1,1,0,1,-3600000,0",
        "UTC,0,0,0,0,0,0,0,0",
        "GMT,0,0,0,0,0,0,0,0",
        "Etc/GMT+5,-18000000,0,0,0,0,0,-18000000,-18000000",
        "Etc/GMT-9,32400000,0,0,0,0,0,32400000,32400000",
    };

    /**
     * The one reporting primitive. It PUBLISHES BEFORE IT COMPARES, which is
     * the whole point of the rewrite: the observed value reaches the cross-VM
     * diff whether the expectation holds or not, so a VM that answers
     * differently is caught by the diff even if this fixture's own expectation
     * were wrong.
     */
    static void ck(String key, String actual, String expected) {
        checks++;
        System.out.println("CK " + CLS + " " + key + "=" + actual);
        if (!actual.equals(expected)) {
            fails++;
            System.out.println("CK " + CLS + " want." + key + "=" + expected);
        }
    }

    static void ck(String key, int actual, int expected) {
        ck(key, Integer.toString(actual), Integer.toString(expected));
    }

    static void ck(String key, boolean actual, boolean expected) {
        ck(key, actual ? "1" : "0", expected ? "1" : "0");
    }

    /** Keys must be one token: no spaces, and no '/' to be mistaken for a path. */
    static String tag(String id) {
        return id.replace('/', '_').replace('+', 'P').replace('-', 'M');
    }

    public static void main(String[] args) {
        opaqueLabel();
        mismatched();
        dstRuleOnAMismatchedLabel();
        sixArgDiscriminator();
        mutation();
        dstFamily();
        unknownIdFallsBackToGmt();

        System.out.println("CK " + CLS + " checks=" + checks);
        System.out.println("CK " + CLS + " fails=" + fails);
        // BANNER, clean path only. run.sh's verdict and guard G4 both grep
        // `^PASS <Class>`; withholding it on a red run is how this vector
        // reports failure now that nothing throws. The parenthesised spelling
        // is mandatory — harness_check_count's PASS arm requires it, and
        // `PASS <Class> checks=N` reads as no count at all.
        if (fails == 0) {
            System.out.println("PASS " + CLS + " (" + checks + " checks)");
        }
    }

    /** The id contributes nothing, for every id, resolvable or not. */
    static void opaqueLabel() {
        for (String id : new String[] { SAO_PAULO, NEW_YORK, "UTC", "Z", "GMT+05:00" }) {
            String p = "stz.plain." + tag(id) + ".";
            SimpleTimeZone z = new SimpleTimeZone(0, id);
            ck(p + "getRawOffset", z.getRawOffset(), 0);
            ck(p + "getOffset.JAN", z.getOffset(JAN), 0);
            ck(p + "getOffset.JUL", z.getOffset(JUL), 0);
            ck(p + "getID", z.getID(), id);
            ck(p + "useDaylightTime", z.useDaylightTime(), false);
            ck(p + "getDSTSavings", z.getDSTSavings(), 0);
            ck(p + "inDaylightTime.JAN", z.inDaylightTime(new Date(JAN)), false);
            ck(p + "inDaylightTime.JUL", z.inDaylightTime(new Date(JUL)), false);
        }
    }

    /**
     * THE BLOCK THAT IS THE TEST for part 1. A non-zero rawOffset paired with a
     * real zone id whose true offset differs from it. Every row here moves on a
     * VM that resolves the id; every row outside this method is stable on one.
     *
     * MUTATION CHECK, so this cannot rot into a vacuous test: delete this
     * method and the SimpleTimeZone half stops being able to fail.
     */
    static void mismatched() {
        int[] raws = { 18000000, -18000000, 3600000 };
        String[] ids = { SAO_PAULO, NEW_YORK };
        for (int raw : raws) {
            for (String id : ids) {
                String p = "stz.mismatched." + raw + "." + tag(id) + ".";
                SimpleTimeZone z = new SimpleTimeZone(raw, id);
                ck(p + "getRawOffset", z.getRawOffset(), raw);
                ck(p + "getOffset.JAN", z.getOffset(JAN), raw);
                ck(p + "getOffset.JUL", z.getOffset(JUL), raw);
                // The silent triple: a zone with NO rule cannot be in daylight
                // time at any instant, and inDaylightTime must agree with the
                // two field-backed accessors that report the absence of a rule.
                ck(p + "useDaylightTime", z.useDaylightTime(), false);
                ck(p + "getDSTSavings", z.getDSTSavings(), 0);
                ck(p + "inDaylightTime.JAN", z.inDaylightTime(new Date(JAN)), false);
                ck(p + "inDaylightTime.JUL", z.inDaylightTime(new Date(JUL)), false);
            }
        }
    }

    /**
     * A REAL DST rule hung on a MISMATCHED label: the US rule on rawOffset
     * -05:00, labelled "UTC". HotSpot applies the rule and ignores the label
     * entirely; a VM that resolves the label answers 0 all year and loses the
     * rule the caller explicitly supplied.
     */
    static void dstRuleOnAMismatchedLabel() {
        SimpleTimeZone z = new SimpleTimeZone(
                -18000000, "UTC",
                Calendar.MARCH, 8, -Calendar.SUNDAY, 7200000,
                Calendar.NOVEMBER, 1, -Calendar.SUNDAY, 7200000,
                3600000);
        String p = "stz.ruled.";
        ck(p + "getRawOffset", z.getRawOffset(), -18000000);
        ck(p + "getOffset.JAN", z.getOffset(JAN), -18000000);
        ck(p + "getOffset.JUL", z.getOffset(JUL), -14400000);
        ck(p + "inDaylightTime.JAN", z.inDaylightTime(new Date(JAN)), false);
        ck(p + "inDaylightTime.JUL", z.inDaylightTime(new Date(JUL)), true);
        ck(p + "useDaylightTime", z.useDaylightTime(), true);
        ck(p + "observesDaylightTime", z.observesDaylightTime(), true);
        ck(p + "getDSTSavings", z.getDSTSavings(), 3600000);
        ck(p + "getID", z.getID(), "UTC");
    }

    /**
     * The discriminator. {@code getOffset(int,int,int,int,int,int)} is not one
     * of the registered native descriptors, so it runs real bytecode off the
     * real {@code rawOffset} field. It agreeing while {@link #mismatched()}
     * disagrees is the difference between fixing a constructor and
     * unregistering a native — so both arms are published, not just one.
     */
    static void sixArgDiscriminator() {
        SimpleTimeZone z = new SimpleTimeZone(18000000, SAO_PAULO);
        ck("stz.sixarg.JUL",
                z.getOffset(GregorianCalendar.AD, 2021, Calendar.JULY, 15,
                        Calendar.THURSDAY, 43200000),
                18000000);
        ck("stz.sixarg.JAN",
                z.getOffset(GregorianCalendar.AD, 2021, Calendar.JANUARY, 15,
                        Calendar.FRIDAY, 43200000),
                18000000);
        // Same question of a REAL database zone, where the six-arg form and the
        // one-arg form must agree with each other rather than differ.
        TimeZone ny = TimeZone.getTimeZone(NEW_YORK);
        ck("tz.sixarg.New_York.JUL",
                ny.getOffset(GregorianCalendar.AD, 2021, Calendar.JULY, 15,
                        Calendar.THURSDAY, 43200000),
                -14400000);
        ck("tz.sixarg.New_York.JAN",
                ny.getOffset(GregorianCalendar.AD, 2021, Calendar.JANUARY, 15,
                        Calendar.FRIDAY, 43200000),
                -18000000);
    }

    /** setRawOffset must be observable, on a zone whose id resolves elsewhere. */
    static void mutation() {
        SimpleTimeZone z = new SimpleTimeZone(0, SAO_PAULO);
        z.setRawOffset(3600000);
        ck("stz.mutation.getRawOffset", z.getRawOffset(), 3600000);
        ck("stz.mutation.getOffset.JAN", z.getOffset(JAN), 3600000);
        ck("stz.mutation.getOffset.JUL", z.getOffset(JUL), 3600000);
    }

    /**
     * THE CONTROL, and the block the previous revision died inside. Real
     * database zones must keep their real offsets, their real savings and their
     * real historical transitions. Any fix to the SimpleTimeZone half that
     * takes these with it is caught here, 32 zones wide.
     */
    static void dstFamily() {
        ck("tz.availableIDs.length",
                Integer.toString(TimeZone.getAvailableIDs().length), "632");
        ck("tz.New_York.class",
                TimeZone.getTimeZone(NEW_YORK).getClass().getName(),
                "sun.util.calendar.ZoneInfo");

        for (String row : DST_FAMILY) {
            String[] f = row.split(",");
            String id = f[0];
            String p = "tz." + tag(id) + ".";
            TimeZone z = TimeZone.getTimeZone(id);
            ck(p + "getID", z.getID(), id);
            ck(p + "getRawOffset", Integer.toString(z.getRawOffset()), f[1]);
            ck(p + "getDSTSavings", Integer.toString(z.getDSTSavings()), f[2]);
            ck(p + "useDaylightTime", z.useDaylightTime() ? "1" : "0", f[3]);
            ck(p + "observesDaylightTime", z.observesDaylightTime() ? "1" : "0", f[4]);
            ck(p + "inDaylightTime.JAN", z.inDaylightTime(new Date(JAN)) ? "1" : "0", f[5]);
            ck(p + "inDaylightTime.JUL", z.inDaylightTime(new Date(JUL)) ? "1" : "0", f[6]);
            ck(p + "getOffset.JAN", Integer.toString(z.getOffset(JAN)), f[7]);
            ck(p + "getOffset.JUL", Integer.toString(z.getOffset(JUL)), f[8]);
        }
    }

    /**
     * An unresolvable id is GMT, not an exception and not the host default.
     * This is the row that keeps a "resolve the label" implementation from
     * looking correct: a VM that falls back to the DEFAULT zone rather than to
     * GMT passes every other row in this file.
     */
    static void unknownIdFallsBackToGmt() {
        TimeZone z = TimeZone.getTimeZone("Nowhere/Atall");
        ck("tz.unknown.getID", z.getID(), "GMT");
        ck("tz.unknown.getRawOffset", z.getRawOffset(), 0);
        ck("tz.unknown.getOffset.JUL", z.getOffset(JUL), 0);
        ck("tz.unknown.useDaylightTime", z.useDaylightTime(), false);
        ck("tz.unknown.getDSTSavings", z.getDSTSavings(), 0);
    }
}
