import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.text.DateFormatSymbols;
import java.text.DecimalFormatSymbols;
import java.time.LocalDateTime;
import java.util.Locale;

/**
 * Which {@code Locale} the no-{@code Locale} format overloads localize against.
 *
 * <p>{@code String.format(String, Object...)} — the overload with <b>no</b>
 * {@code Locale} — is specified to format against
 * {@code Locale.getDefault(Locale.Category.FORMAT)}. CratonVM localized it
 * against {@code Locale.ROOT} instead (W7-91 §5, and the last open
 * {@code format} row of
 * {@code docs/known-issues/jdk-only/W7-34-formatter-family-residuals.md}). The
 * difference is invisible on a ROOT/en-US host and wrong everywhere else:
 * decimal separators, grouping separators, digits and date symbols all diverge.
 * It is exactly the class of defect that passes every test written by someone
 * on an English host and then breaks in production.
 *
 * <p><b>This vector is in {@code CORE_CLASSES} despite its {@code RJdk*} name</b>,
 * for the same reason {@code RJdkViews} is: the {@code RJdk*} prefix is the
 * JDK-only corpus's naming convention, but that corpus is about {@code --jdk-only}
 * POLICY, and this asserts {@code --real-jdk} COMPATIBILITY behaviour — HotSpot
 * parity on a formatting rule — which is a default-mode concern.
 *
 * <h2>How this asserts the RULE and not this host's incidental locale</h2>
 *
 * The FORMAT-category default is <b>pinned by the test itself</b>, to
 * {@code Locale.GERMANY}, and restored in a {@code finally}. A vector that
 * merely read the host's default would be green on an en-US CI box against a
 * VM that always answered ROOT — which is how this defect survived.
 *
 * <p>Every expectation is an <b>exact string equality</b>. "Non-null" and
 * "length" checks are not the contract here; two defects survived 2026 behind
 * exactly those. Where a rendering depends on locale DATA rather than on the
 * rule, the expected string is <b>derived from the JDK's own
 * {@code DecimalFormatSymbols}</b> for the same locale, so a platform whose
 * German data is unavailable cannot manufacture a red — but the equality is
 * still character-for-character, never a {@code contains}.
 *
 * <p>Nothing host-locale-dependent is printed. The harness diffs this class's
 * {@code CK} lines between CratonVM and HotSpot byte for byte, so every printed
 * value is either a count, a boolean, or a string pinned to {@code Locale.ROOT}.
 * The two booleans are the negative controls: they say whether the
 * locale-sensitive implications had an antecedent at all, so a vacuous pass is
 * visible in the diff rather than silent.
 */
public class RJdkFormatLocale {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** The value every check below formats. Chosen so grouping AND the decimal separator both show. */
    static final double V = 1234.5d;

    /** {@code %,.2f} of {@link #V} under ROOT/US separators. Pinned, ASCII, identical on every host. */
    static final String ROOT_RENDER = "1,234.50";

    /**
     * The rule, on the numeric conversions.
     *
     * <p>Called with the FORMAT-category default already pinned to
     * {@code Locale.GERMANY}.
     */
    static void numericFollowsTheFormatDefault() {
        // The oracle for the pinned locale, read out of the JDK's own table.
        DecimalFormatSymbols de = DecimalFormatSymbols.getInstance(Locale.GERMANY);
        // Whether this platform actually has German data. Reported below; a
        // `false` here means the implications are vacuous, and the CK-line diff
        // is what makes that visible instead of silent.
        boolean deResolved = de.getDecimalSeparator() == ',' && de.getGroupingSeparator() == '.';

        String noLocale = String.format("%,.2f", V);
        String explicitDe = String.format(Locale.GERMANY, "%,.2f", V);
        String explicitRoot = String.format(Locale.ROOT, "%,.2f", V);
        String explicitUs = String.format(Locale.US, "%,.2f", V);
        String explicitNull = String.format((Locale) null, "%,.2f", V);

        // 1. THE RULE. Two spellings of one request must agree, exactly. This
        //    is the whole defect: the left side used to be `1,234.50`.
        check(noLocale.equals(explicitDe),
                "String.format(String, Object...) must localize against"
                        + " Locale.getDefault(Locale.Category.FORMAT); got [" + noLocale
                        + "] vs explicit GERMANY [" + explicitDe + "]");

        // 2. The same rule as a CHARACTER-FOR-CHARACTER expectation, built from
        //    the locale's own symbols rather than from a pinned "1.234,50", so
        //    a platform without German data cannot turn it into a false red —
        //    and written as one unconditional implication rather than a guarded
        //    block, so the reported check COUNT does not move between hosts.
        //    The zero-digit guard is load-bearing: a locale whose digits are not
        //    ASCII (ar-EG's U+0660) would need the digits shifted too, and
        //    Germany's are ASCII, so the antecedent holds wherever the data does.
        String expected = "1" + de.getGroupingSeparator() + "234" + de.getDecimalSeparator() + "50";
        check(de.getZeroDigit() != '0' || noLocale.equals(expected),
                "the no-locale rendering must use the FORMAT locale's own separators; expected ["
                        + expected + "] got [" + noLocale + "]");

        // 3. THE ROOT FAST PATH, which the fix must not have moved. An explicit
        //    Locale.ROOT is `1,234.50` no matter what the default is — this is
        //    the check that fails if someone "fixes" the default by making every
        //    format call read the default.
        check(explicitRoot.equals(ROOT_RENDER),
                "an explicit Locale.ROOT must be unaffected by the FORMAT default; got ["
                        + explicitRoot + "]");
        check(explicitUs.equals(ROOT_RENDER),
                "an explicit Locale.US must be unaffected by the FORMAT default; got ["
                        + explicitUs + "]");

        // 4. An explicit NULL Locale is NOT the same request as an absent one.
        //    "If l is null then no localization is applied" — so this stays the
        //    root rendering while the no-locale overload above follows the
        //    default. One concept, two encodings: a fix that collapsed them
        //    would pass check 1 and fail here.
        check(explicitNull.equals(ROOT_RENDER),
                "String.format((Locale) null, ...) applies NO localization; got ["
                        + explicitNull + "]");

        // 5. The discrimination, as an implication so the count is stable: when
        //    the German symbols really differ from the root ones, the no-locale
        //    rendering must differ from the root one. Without this, a VM that
        //    answered ROOT for BOTH sides of check 1 would pass it.
        check(!deResolved || !noLocale.equals(explicitRoot),
                "German symbols resolved, so the no-locale rendering must NOT equal the ROOT"
                        + " rendering; both are [" + noLocale + "]");

        // 6. The integer conversion takes the same route (the grouping
        //    separator alone, no decimal separator involved).
        check(String.format("%,d", 1234567).equals(String.format(Locale.GERMANY, "%,d", 1234567)),
                "%,d must follow the FORMAT default too; got [" + String.format("%,d", 1234567)
                        + "] vs [" + String.format(Locale.GERMANY, "%,d", 1234567) + "]");
        check(String.format(Locale.ROOT, "%,d", 1234567).equals("1,234,567"),
                "an explicit Locale.ROOT %,d must stay ASCII-comma grouped; got ["
                        + String.format(Locale.ROOT, "%,d", 1234567) + "]");

        System.out.println("CK RJdkFormatLocale deResolved=" + deResolved
                + " rootRender=" + explicitRoot);
    }

    /**
     * The sibling surfaces. Each is the SAME rule reached by a different name;
     * one JVMS/JDK rule implemented twice in two places that then drift is this
     * codebase's most common defect shape, so each is asserted separately rather
     * than assumed to share a helper.
     */
    static void siblingSurfacesFollowTheSameRule() throws Exception {
        String oracle = String.format("%,.2f", V);

        // String.formatted(Object...) — specified as String.format(this, args).
        check("%,.2f".formatted(V).equals(oracle),
                "String.formatted must be String.format(this, args); got ["
                        + "%,.2f".formatted(V) + "] vs [" + oracle + "]");

        // PrintStream.printf / PrintStream.format, captured rather than printed:
        // writing a locale-dependent string to stdout would put it in the
        // harness's cross-VM diff, which is a different thing on trial.
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(sink, true, "UTF-8");
        ps.printf("%,.2f", V);
        ps.flush();
        check(!ps.checkError(), "the capture PrintStream reported an error");
        String printfText = sink.toString("UTF-8");
        check(printfText.equals(oracle),
                "PrintStream.printf(String, Object...) must follow the FORMAT default; got ["
                        + printfText + "] vs [" + oracle + "]");

        sink.reset();
        ps.format("%,.2f", V);
        ps.flush();
        String formatText = sink.toString("UTF-8");
        check(formatText.equals(oracle),
                "PrintStream.format(String, Object...) must agree with printf; got ["
                        + formatText + "] vs [" + oracle + "]");

        // And the locale-taking overload on the same stream must NOT be
        // answered from the default. This is the anti-overshoot half, and it is
        // the one an implementation that drops its Locale argument fails.
        sink.reset();
        ps.printf(Locale.ROOT, "%,.2f", V);
        ps.flush();
        String printfRoot = sink.toString("UTF-8");
        check(printfRoot.equals(ROOT_RENDER),
                "PrintStream.printf(Locale.ROOT, ...) must render ROOT whatever the FORMAT"
                        + " default is; got [" + printfRoot + "]");
        ps.close();

        // NOT ASSERTED, deliberately, and this is a hole rather than an
        // oversight: `new Formatter()` and `new Formatter(Appendable)` also
        // carry Locale.getDefault(Locale.Category.FORMAT) in the real JDK, but
        // CratonVM registers natives for both constructors that write a NULL
        // locale into the receiver, and a null receiver locale is legitimately
        // "no localization". Asserting it here would be red for a defect this
        // lane cannot fix from `native-builtins/src/lang_string.rs` — the
        // constructors are in `native-builtins/src/lib.rs`. Recorded, with the
        // patch, in W7-34-formatter-family-residuals.md. `new Formatter(sb,
        // Locale.GERMANY)` — the explicit-locale form — IS covered, in RStrings.

        System.out.println("CK RJdkFormatLocale printfMatchesFormat="
                + printfText.equals(formatText)
                + " formattedMatchesFormat=" + "%,.2f".formatted(V).equals(oracle));
    }

    /**
     * The date half of the same rule, which is the twin that must not drift.
     *
     * <p>The numeric symbols and the date symbols are one rule implemented
     * twice — {@code DecimalFormatSymbols} and {@code DateFormatSymbols} — and
     * they HAD drifted: the date side already resolved
     * {@code DateFormatSymbols.getInstance()} (the FORMAT default) for the
     * no-locale overload while the number side took the root constants. This
     * asserts they now answer from the same default.
     *
     * <p>The instant is fixed, so nothing here follows the calendar, and each
     * check is an equality against the JDK's own table for the same locale
     * rather than against a pinned month name.
     */
    static void dateNamesFollowTheSameDefault() {
        LocalDateTime tstamp = LocalDateTime.of(2026, 3, 4, 5, 6, 7);   // March
        DateFormatSymbols byDefault = DateFormatSymbols.getInstance();
        DateFormatSymbols germany = DateFormatSymbols.getInstance(Locale.GERMANY);

        check(String.format("%tB", tstamp).equals(byDefault.getMonths()[2]),
                "%tB with no locale must be DateFormatSymbols.getInstance().getMonths(); got ["
                        + String.format("%tB", tstamp) + "]");
        check(String.format("%tb", tstamp).equals(byDefault.getShortMonths()[2]),
                "%tb with no locale must be getInstance().getShortMonths(); got ["
                        + String.format("%tb", tstamp) + "]");
        // The default IS Germany for the duration of this test, so the two
        // spellings must agree — the same two-spellings equality the numeric
        // half uses, on the other helper.
        check(String.format("%tB", tstamp).equals(String.format(Locale.GERMANY, "%tB", tstamp)),
                "the no-locale %tB must equal the explicit-GERMANY %tB while GERMANY is the"
                        + " FORMAT default; got [" + String.format("%tB", tstamp) + "] vs ["
                        + String.format(Locale.GERMANY, "%tB", tstamp) + "]");
        // Negative control for the implication above.
        boolean deNames = !germany.getMonths()[2].equals("March");
        check(!deNames || !String.format("%tB", tstamp)
                        .equals(String.format(Locale.ROOT, "%tB", tstamp)),
                "German month names resolved, so the no-locale %tB must differ from the ROOT one;"
                        + " both are [" + String.format("%tB", tstamp) + "]");

        // LENGTHS and a boolean, never the name itself: printing `März` here
        // would put the two VMs' stdout ENCODINGS on trial in a row about the
        // format locale.
        System.out.println("CK RJdkFormatLocale deNames=" + deNames
                + " deMonthLen=" + germany.getMonths()[2].length());
    }

    public static void main(String[] args) throws Exception {
        // Read the incoming FORMAT default so it can be put back exactly.
        // `Locale.setDefault(Category, Locale)` writes only the named category
        // — DISPLAY and the base default are deliberately left alone, so
        // nothing else in this process sees a display-locale change.
        // NOTE on the assertion messages below: a `Locale` is never
        // concatenated into one. Java builds a check's message EAGERLY, on the
        // passing path too, and `Locale.toString()` on this VM's synthetic
        // default-locale object is exactly the kind of incidental call that
        // turns a green vector red for a reason that is not the rule under
        // test. Everything reported here is a boolean or an already-built
        // String.
        Locale before = Locale.getDefault(Locale.Category.FORMAT);
        boolean restored;
        try {
            Locale.setDefault(Locale.Category.FORMAT, Locale.GERMANY);
            check(Locale.GERMANY.equals(Locale.getDefault(Locale.Category.FORMAT)),
                    "setDefault(FORMAT, GERMANY) must be readable back through getDefault(FORMAT)"
                            + " — the rest of this vector cannot mean anything without it");
            numericFollowsTheFormatDefault();
            siblingSurfacesFollowTheSameRule();
            dateNamesFollowTheSameDefault();
        } finally {
            Locale.setDefault(Locale.Category.FORMAT, before);
        }
        restored = before.equals(Locale.getDefault(Locale.Category.FORMAT));
        check(restored, "the FORMAT default must be restored to the value this vector found");
        // With the default back to the host's, the ROOT overload is still ROOT.
        // A last guard that the restore did not leave the VM in the pinned state.
        check(String.format(Locale.ROOT, "%,.2f", V).equals(ROOT_RENDER),
                "an explicit Locale.ROOT must still render " + ROOT_RENDER + " after the restore;"
                        + " got [" + String.format(Locale.ROOT, "%,.2f", V) + "]");

        System.out.println("CK RJdkFormatLocale restored=" + restored);
        System.out.println("CK RJdkFormatLocale checks=" + checks);
        System.out.println("PASS RJdkFormatLocale (" + checks + " checks)");
    }
}
