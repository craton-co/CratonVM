import java.util.Calendar;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.SimpleTimeZone;
import java.util.TimeZone;

/**
 * {@code java.util.SimpleTimeZone}: the {@code ID} argument is an OPAQUE LABEL.
 *
 * WHY THIS EXISTS. CratonVM registers four natives on
 * {@code java/util/SimpleTimeZone} -- {@code getRawOffset()},
 * {@code getOffset(J)}, {@code getOffsets(J[I)} and {@code getOffsetsByWall(J[I)}
 * (native-builtins/src/lib.rs, {@code register_tzdb_offset_natives_for}) -- and
 * every one of them answers by reading the receiver's {@code ID} field and
 * resolving it against tzdb. The receiver's OWN {@code rawOffset} and its OWN
 * DST rule are never consulted. Those natives were added to serve the
 * {@code SimpleTimeZone} the VM itself fabricates inside
 * {@code alloc_synth_timezone} for {@code TimeZone.getTimeZone(id)}; registration
 * is per CLASS, so they also captured every SimpleTimeZone the APPLICATION
 * constructs, which have real bytecode and real state.
 *
 * The contract they violate is exact: {@code new SimpleTimeZone(rawOffset, ID)}
 * takes its offset from {@code rawOffset}. {@code ID} is not looked up in the
 * timezone database and contributes no offset. So
 * {@code new SimpleTimeZone(0, "America/Sao_Paulo")} is UTC that happens to be
 * NAMED {@code America/Sao_Paulo}.
 *
 * WHY THIS VECTOR IS SHAPED THE WAY IT IS. Two facts make the defect nearly
 * invisible, and both would make a lazier vector vacuous:
 *
 *   * the overwhelmingly common idiom in real code is
 *     {@code new SimpleTimeZone(0, "Z")}. {@code "Z"} resolves to no zone, so
 *     the wrong code path falls back to 0 and is ACCIDENTALLY RIGHT. A vector
 *     built only from that spelling passes on a broken VM.
 *   * on a UTC host the defect is entirely invisible, because the resolved
 *     offset and the passed {@code rawOffset} are both zero.
 *
 * So every zone id below is a FIXED literal, never the host default, and the
 * load-bearing rows pair a NON-ZERO {@code rawOffset} with a REAL zone id whose
 * true offset DIFFERS from it. {@code "Z"} and {@code "UTC"} appear only as
 * controls that must not move.
 *
 * MUTATION CHECK, so this cannot rot into a vacuous test: delete the
 * {@code MISMATCHED} block and the remaining assertions all pass on today's
 * VM. That block is the test.
 *
 * WHAT IS ASSERTED, AND WHY NOT LESS. The defect is not confined to
 * {@code getRawOffset()}, so neither is this:
 *
 *   * {@code getRawOffset()} must return the constructor argument verbatim.
 *   * {@code getOffset(long)} must derive from that argument plus only the DST
 *     rule actually supplied, never from a lookup of the id.
 *   * {@code inDaylightTime(Date)} is the SILENT one. Its real bytecode is
 *     {@code getOffset(d.getTime()) != this.rawOffset} -- it reads the FIELD
 *     directly while calling the hijacked {@code getOffset}. On a broken VM a
 *     zone constructed with NO daylight-saving rule at all reports that it IS
 *     in daylight time, while {@code useDaylightTime()} and
 *     {@code getDSTSavings()} (unhijacked, field-backed) keep saying there is
 *     no rule. That self-contradictory triple is asserted explicitly.
 *   * the six-arg {@code getOffset(era,y,m,d,dow,ms)} overload is NOT among the
 *     registered descriptors, so it runs real bytecode off the real
 *     {@code rawOffset} field. It is asserted as a DISCRIMINATOR: it agreeing
 *     while the one-arg form disagrees is what proves the constructor stored
 *     {@code rawOffset} correctly and the accessors are what is wrong.
 *   * {@code setRawOffset} must be observable through {@code getRawOffset}.
 *   * {@code getID()} must return the label verbatim -- this half is already
 *     correct and is the control for the others.
 *   * {@code TimeZone.getTimeZone(id)} must keep answering from tzdb, including
 *     a real DST transition. Any fix that removes the SimpleTimeZone natives
 *     must not take these with it. HotSpot returns
 *     {@code sun.util.calendar.ZoneInfo} here and never a SimpleTimeZone.
 *
 * Determinism: no host zone, no wall clock. The two instants are fixed epoch
 * millis.
 *
 * OUTPUT CONTRACT. Exactly two lines, both of which survive {@code run.sh}'s
 * {@code extract()} filter, and no third line on any other prefix:
 * <pre>
 *   CK RSimpleTimeZoneRaw checks=104
 *   PASS RSimpleTimeZoneRaw (104 checks)
 * </pre>
 * Measured on HotSpot 25.0.3+9 (Microsoft build, Windows), rc=0. Anything
 * printed on a prefix other than {@code PASS }/{@code CK } is deleted before
 * the cross-VM diff and trips guard G1 on the oracle, so this vector reports
 * only through the count line and through throwing.
 */
public class RSimpleTimeZoneRaw {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void eq(int actual, int expected, String m) {
        check(actual == expected, m + ": expected " + expected + " but was " + actual);
    }

    /** 2021-01-15T12:00:00Z -- northern winter, southern summer. */
    static final long JAN = 1610712000000L;
    /** 2021-07-15T12:00:00Z -- northern summer, southern winter. */
    static final long JUL = 1626350400000L;

    /** UTC-03:00, and with no daylight saving at all since 2019. */
    static final String SAO_PAULO = "America/Sao_Paulo";
    /** UTC-05:00 standard, UTC-04:00 in daylight time. */
    static final String NEW_YORK = "America/New_York";

    public static void main(String[] args) {
        opaqueLabel();
        mismatched();
        dstRuleOnAMismatchedLabel();
        sixArgDiscriminator();
        mutation();
        realZonesStillResolve();
        System.out.println("CK RSimpleTimeZoneRaw checks=" + checks);
        // The banner run.sh actually looks for. This line used to read
        // `RESULT RSimpleTimeZoneRaw PASS`, which is neither: run.sh's own
        // verdict greps `^PASS <Class>` and its guard G4 greps the same, so a
        // PERFECT VM was scored FAIL ("no PASS line") and the HotSpot oracle was
        // reported sick ("exited 0 but printed no PASS line"). extract() then
        // DELETED the RESULT line, so nothing carried the word PASS into the
        // diff at all. Measured on HotSpot 25.0.3+9: rc=0, 104 checks, every
        // assertion holding — the fixture's expectations were always right and
        // only its banner was in the wrong dialect.
        // The parenthesised spelling is deliberate: harness_check_count parses
        // `PASS <Class> (N checks)` or `CK <Class> checks=N` and NOTHING else.
        System.out.println("PASS RSimpleTimeZoneRaw (" + checks + " checks)");
    }

    /** The id contributes nothing, for every id, resolvable or not. */
    static void opaqueLabel() {
        for (String id : new String[] { SAO_PAULO, NEW_YORK, "UTC", "Z", "GMT+05:00" }) {
            SimpleTimeZone z = new SimpleTimeZone(0, id);
            eq(z.getRawOffset(), 0, "getRawOffset(0, " + id + ")");
            eq(z.getOffset(JAN), 0, "getOffset(JAN) of (0, " + id + ")");
            eq(z.getOffset(JUL), 0, "getOffset(JUL) of (0, " + id + ")");
            check(id.equals(z.getID()), "getID must be verbatim for " + id);
            check(!z.useDaylightTime(), "no rule was supplied for " + id);
            eq(z.getDSTSavings(), 0, "getDSTSavings of (0, " + id + ")");
            check(!z.inDaylightTime(new Date(JAN)), "inDaylightTime(JAN) of (0, " + id + ")");
            check(!z.inDaylightTime(new Date(JUL)), "inDaylightTime(JUL) of (0, " + id + ")");
        }
    }

    /**
     * THE BLOCK THAT IS THE TEST. A non-zero rawOffset paired with a real zone
     * id whose true offset differs from it. Every row here fails on a VM that
     * resolves the id; every row outside this method passes on one.
     */
    static void mismatched() {
        int[] raws = { 18000000, -18000000, 3600000 };
        String[] ids = { SAO_PAULO, NEW_YORK };
        for (int raw : raws) {
            for (String id : ids) {
                SimpleTimeZone z = new SimpleTimeZone(raw, id);
                eq(z.getRawOffset(), raw, "MISMATCHED getRawOffset(" + raw + ", " + id + ")");
                eq(z.getOffset(JAN), raw, "MISMATCHED getOffset(JAN) of (" + raw + ", " + id + ")");
                eq(z.getOffset(JUL), raw, "MISMATCHED getOffset(JUL) of (" + raw + ", " + id + ")");

                // The silent triple. A zone with NO rule cannot be in daylight
                // time at any instant, and inDaylightTime must agree with the
                // two field-backed accessors that report the absence of a rule.
                check(!z.useDaylightTime(), "MISMATCHED useDaylightTime(" + raw + ", " + id + ")");
                eq(z.getDSTSavings(), 0, "MISMATCHED getDSTSavings(" + raw + ", " + id + ")");
                for (long t : new long[] { JAN, JUL }) {
                    boolean in = z.inDaylightTime(new Date(t));
                    check(!in, "MISMATCHED inDaylightTime(" + t + ") of (" + raw + ", " + id
                            + ") -- a zone with no DST rule reported daylight time; "
                            + "getOffset() and the rawOffset field disagree");
                }
            }
        }
    }

    /**
     * A REAL DST rule hung on a MISMATCHED label. The US rule on rawOffset
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
        eq(z.getRawOffset(), -18000000, "ruled getRawOffset");
        eq(z.getOffset(JAN), -18000000, "ruled getOffset(JAN) -- standard time");
        eq(z.getOffset(JUL), -14400000, "ruled getOffset(JUL) -- daylight time");
        check(!z.inDaylightTime(new Date(JAN)), "ruled inDaylightTime(JAN)");
        check(z.inDaylightTime(new Date(JUL)), "ruled inDaylightTime(JUL)");
        check(z.useDaylightTime(), "ruled useDaylightTime");
        eq(z.getDSTSavings(), 3600000, "ruled getDSTSavings");
    }

    /**
     * The discriminator. {@code getOffset(int,int,int,int,int,int)} is not one
     * of the registered native descriptors, so it runs real bytecode off the
     * real {@code rawOffset} field. If this passes while
     * {@link #mismatched()} fails, the constructor stored the argument
     * correctly and the one-arg accessors are what is wrong -- which is the
     * difference between fixing a constructor and unregistering a native.
     */
    static void sixArgDiscriminator() {
        SimpleTimeZone z = new SimpleTimeZone(18000000, SAO_PAULO);
        eq(z.getOffset(GregorianCalendar.AD, 2021, Calendar.JULY, 15, Calendar.THURSDAY, 43200000),
                18000000, "six-arg getOffset must read the rawOffset field");
    }

    /** setRawOffset must be observable, on a zone whose id resolves elsewhere. */
    static void mutation() {
        SimpleTimeZone z = new SimpleTimeZone(0, SAO_PAULO);
        z.setRawOffset(3600000);
        eq(z.getRawOffset(), 3600000, "setRawOffset then getRawOffset");
        eq(z.getOffset(JAN), 3600000, "setRawOffset then getOffset");
    }

    /**
     * THE CONTROL, and the thing a fix most easily breaks. Real database zones
     * must keep their real offsets and their real transitions.
     */
    static void realZonesStillResolve() {
        TimeZone sp = TimeZone.getTimeZone(SAO_PAULO);
        eq(sp.getRawOffset(), -10800000, "TimeZone.getTimeZone(Sao_Paulo).getRawOffset");
        eq(sp.getOffset(JAN), -10800000, "Sao_Paulo getOffset(JAN)");
        eq(sp.getOffset(JUL), -10800000, "Sao_Paulo getOffset(JUL)");
        check(!sp.useDaylightTime(), "Sao_Paulo has had no DST since 2019");

        TimeZone ny = TimeZone.getTimeZone(NEW_YORK);
        eq(ny.getRawOffset(), -18000000, "New_York getRawOffset");
        eq(ny.getOffset(JAN), -18000000, "New_York getOffset(JAN) -- standard");
        eq(ny.getOffset(JUL), -14400000, "New_York getOffset(JUL) -- daylight");
        check(ny.inDaylightTime(new Date(JUL)), "New_York inDaylightTime(JUL)");
        check(!ny.inDaylightTime(new Date(JAN)), "New_York inDaylightTime(JAN)");
        check(ny.useDaylightTime(), "New_York useDaylightTime");

        TimeZone utc = TimeZone.getTimeZone("UTC");
        eq(utc.getRawOffset(), 0, "UTC getRawOffset");
        eq(utc.getOffset(JUL), 0, "UTC getOffset(JUL)");
    }
}
