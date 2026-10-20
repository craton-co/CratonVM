import java.time.DayOfWeek;
import java.time.Instant;
import java.time.Month;
import java.time.MonthDay;
import java.time.OffsetTime;
import java.time.Year;
import java.time.YearMonth;
import java.time.ZoneId;
import java.util.Locale;

/**
 * G2-1: the {@code java.util.Formatter} conversion surface measured against
 * HotSpot 25.0.3+9-LTS, 2026-08-16 ({@code Locale.US} throughout).
 *
 * <p>Four defects, all fixed in {@code native-builtins/src/lang_string.rs}
 * before this vector existed: {@code IllegalFormatConversionException}
 * reporting the upper-case conversion character instead of the lower-case one
 * ({@code format_arg}'s applicability screen), {@code %<} with no previous
 * conversion silently formatting {@code args[0]} instead of refusing
 * ({@code MissingFormatArgumentException}), {@code java.time.OffsetTime}
 * having no decoder arm at all, and the {@code date} support flag being one
 * flag where the JDK effectively has three ({@code year}/{@code month}/
 * {@code day} independently, for {@code Year}/{@code YearMonth}/
 * {@code MonthDay}/{@code Month}).
 *
 * <p>This class also covers G2-1's residual §4.1, closed separately:
 * {@code java.time.DayOfWeek} answers {@code %tA}/{@code %ta} with no year,
 * month or day at all, which is the one source
 * {@code FmtSupport::full_date}'s derivation cannot express on its own (see
 * {@code FmtSupport::weekday} in {@code lang_string.rs}).
 */
public class RJdkFormatSurface {

    static int checks = 0;
    static final int EXPECTED_CHECKS = 44;

    static void eq(String label, String expect, String actual) {
        checks++;
        if (!expect.equals(actual)) {
            throw new AssertionError(
                    label + ": expected=[" + expect + "] actual=[" + actual + "]");
        }
    }

    static void throwsWith(
            String label,
            String expectClass,
            String expectMsg,
            java.util.function.Supplier<String> body) {
        checks++;
        String got;
        try {
            got = "no-throw:" + body.get();
        } catch (Throwable t) {
            got = t.getClass().getName() + "|" + t.getMessage();
        }
        String want = expectClass + "|" + expectMsg;
        if (!want.equals(got)) {
            throw new AssertionError(label + ": expected=[" + want + "] actual=[" + got + "]");
        }
    }

    public static void main(String[] args) {
        final Locale US = Locale.US;

        // 2.1 the exception carries the LOWER-case conversion, even though
        // `%X`/`%E`/`%G`/`%A` themselves render upper-case digits.
        throwsWith(
                "upperConvX",
                "java.util.IllegalFormatConversionException",
                "x != java.lang.Boolean",
                () -> String.format(US, "%X", Boolean.TRUE));
        throwsWith(
                "upperConvE",
                "java.util.IllegalFormatConversionException",
                "e != java.lang.String",
                () -> String.format(US, "%E", "s"));
        throwsWith(
                "upperConvG",
                "java.util.IllegalFormatConversionException",
                "g != java.lang.Boolean",
                () -> String.format(US, "%G", Boolean.TRUE));
        throwsWith(
                "upperConvA",
                "java.util.IllegalFormatConversionException",
                "a != java.math.BigDecimal",
                () -> String.format(US, "%A", new java.math.BigDecimal("1.5")));
        // ...and %t does NOT fold: `printDateTime` reports the FIELD
        // character as typed, the opposite rule.
        throwsWith(
                "dateConvNotFolded",
                "java.util.IllegalFormatConversionException",
                "Y != java.lang.String",
                () -> String.format(US, "%tY", "x"));
        // getConversion() carries it too, not only the message.
        checks++;
        try {
            String.format(US, "%X", Boolean.TRUE);
            throw new AssertionError("upperConvAccessor: expected an exception");
        } catch (java.util.IllegalFormatConversionException e) {
            if (e.getConversion() != 'x') {
                throw new AssertionError(
                        "upperConvAccessor: expected=[x] actual=[" + e.getConversion() + "]");
            }
        }

        // 2.2 relative index with no previous conversion must REFUSE.
        throwsWith(
                "relNoPrevious",
                "java.util.MissingFormatArgumentException",
                "Format specifier '%<s'",
                () -> String.format(US, "%<s", "a"));
        throwsWith(
                "relNoPreviousDate",
                "java.util.MissingFormatArgumentException",
                "Format specifier '%<tY'",
                () -> String.format(US, "%<tY", 0L));
        eq("relAfterOrdinary", "aa", String.format(US, "%s%<s", "a"));
        eq("relAfterExplicit", "bb", String.format(US, "%2$s%<s", "a", "b"));
        eq("relSecondOrdinary", "abb", String.format(US, "%s%s%<s", "a", "b"));
        // the range check runs after the parse-time flag checks.
        throwsWith(
                "relLosesToFlagCheck",
                "java.util.FormatFlagsConversionMismatchException",
                "Conversion = s, Flags = 0",
                () -> String.format(US, "%<0s", "a"));

        // 2.3 OffsetTime had no decoder arm at all.
        OffsetTime ot =
                Instant.ofEpochMilli(1755300645123L)
                        .atZone(ZoneId.of("Asia/Kolkata"))
                        .toOffsetDateTime()
                        .toOffsetTime();
        eq("offsetTimeH", "05", String.format(US, "%tH", ot));
        eq("offsetTimeT", "05:00:45", String.format(US, "%tT", ot));
        eq("offsetTimeR", "05:00:45 AM", String.format(US, "%tr", ot));
        eq("offsetTimeN", "123000000", String.format(US, "%tN", ot));
        eq("offsetTimeSmallZ", "+0530", String.format(US, "%tz", ot));
        eq("offsetTimeBigZ", "+05:30", String.format(US, "%tZ", ot));
        eq("offsetTimeP", "am", String.format(US, "%tp", ot));
        throwsWith(
                "offsetTimeNoInstant",
                "java.util.IllegalFormatConversionException",
                "s != java.time.OffsetTime",
                () -> String.format(US, "%ts", ot));
        throwsWith(
                "offsetTimeNoDate",
                "java.util.IllegalFormatConversionException",
                "Y != java.time.OffsetTime",
                () -> String.format(US, "%tY", ot));
        throwsWith(
                "offsetTimeDComposite",
                "java.util.IllegalFormatConversionException",
                "m != java.time.OffsetTime",
                () -> String.format(US, "%tD", ot));
        throwsWith(
                "offsetTimeCComposite",
                "java.util.IllegalFormatConversionException",
                "a != java.time.OffsetTime",
                () -> String.format(US, "%tc", ot));

        // 2.4 the partial java.time types: `date` is really three flags.
        YearMonth ym = YearMonth.of(2020, 2);
        MonthDay md = MonthDay.of(1, 2);
        eq("yearMonthY", "2020", String.format(US, "%tY", ym));
        eq("yearMonthM", "02", String.format(US, "%tm", ym));
        eq("yearMonthB", "February", String.format(US, "%tB", ym));
        throwsWith(
                "yearMonthNoDay",
                "java.util.IllegalFormatConversionException",
                "d != java.time.YearMonth",
                () -> String.format(US, "%td", ym));
        eq("monthDayD", "02", String.format(US, "%td", md));
        eq("monthDayE", "2", String.format(US, "%te", md));
        eq("monthDayB", "January", String.format(US, "%tB", md));
        throwsWith(
                "monthDayNoYear",
                "java.util.IllegalFormatConversionException",
                "Y != java.time.MonthDay",
                () -> String.format(US, "%tY", md));
        eq("yearY", "2020", String.format(US, "%tY", Year.of(2020)));
        eq("monthM", "01", String.format(US, "%tm", Month.JANUARY));
        eq("monthB", "January", String.format(US, "%tB", Month.JANUARY));

        // G2-1 residual §4.1, closed: DayOfWeek answers %tA/%ta with no
        // year, month or day, and %tc reports 'b' (the MONTH group), not
        // 'a' -- the weekday check now passes for it.
        eq("dayOfWeekA", "Monday", String.format(US, "%tA", DayOfWeek.MONDAY));
        eq("dayOfWeekA_sunday", "Sunday", String.format(US, "%tA", DayOfWeek.SUNDAY));
        eq("dayOfWeekA_saturday", "Saturday", String.format(US, "%tA", DayOfWeek.SATURDAY));
        eq("dayOfWeeka", "Mon", String.format(US, "%ta", DayOfWeek.MONDAY));
        throwsWith(
                "dayOfWeekNoYear",
                "java.util.IllegalFormatConversionException",
                "Y != java.time.DayOfWeek",
                () -> String.format(US, "%tY", DayOfWeek.MONDAY));
        throwsWith(
                "dayOfWeekNoDayOfYear",
                "java.util.IllegalFormatConversionException",
                "j != java.time.DayOfWeek",
                () -> String.format(US, "%tj", DayOfWeek.MONDAY));
        throwsWith(
                "dayOfWeekCComposite",
                "java.util.IllegalFormatConversionException",
                "b != java.time.DayOfWeek",
                () -> String.format(US, "%tc", DayOfWeek.MONDAY));

        // 3.2 the internal uppercase flag, which Flags.toString renders as ^.
        throwsWith(
                "upperFlagCaret",
                "java.util.IllegalFormatFlagsException",
                "Flags = '^+ '",
                () -> String.format(US, "%+ 8X", -42));
        throwsWith(
                "upperFlagCaretDash",
                "java.util.IllegalFormatFlagsException",
                "Flags = '-^0'",
                () -> String.format(US, "%-08E", -1.5d));
        throwsWith(
                "lowerFlagNoCaret",
                "java.util.IllegalFormatFlagsException",
                "Flags = '+ '",
                () -> String.format(US, "%+ 8x", -42));

        if (checks != EXPECTED_CHECKS) {
            throw new AssertionError(
                    "check count moved: expected " + EXPECTED_CHECKS + ", ran " + checks);
        }
        System.out.println("CK RJdkFormatSurface checks=" + checks);
        System.out.println("PASS RJdkFormatSurface (" + checks + " checks)");
    }
}
