// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.text.DecimalFormatSymbols;
import java.text.NumberFormat;
import java.util.Locale;

/**
 * Separates "the default locale was determined wrongly" from "the locale DATA
 * could not be resolved" -- the two readings of a wrong decimal separator.
 *
 * The distinction is the whole point: a report that only prints the default
 * locale's formatting cannot tell them apart, and the first reading sends you
 * to `user.language`/`user.country`, which on the image that produced this are
 * both correct.
 *
 * Ask for locales EXPLICITLY (de-DE, ru-RU) rather than relying on the host's:
 * if an explicitly named locale answers with US separators, the default-locale
 * question is settled and the answer is the data.
 *
 *   java     -cp probes Jdk21StrictLocaleData
 *   cratonvm --real-jdk  -cp probes Jdk21StrictLocaleData
 *   cratonvm --jdk-only  -cp probes Jdk21StrictLocaleData
 *
 * Print separators as code points, not as characters: ru-RU's grouping
 * separator is U+00A0 (no-break space), which renders as a space or as mojibake
 * depending on the console, and "looks like a space" is not a measurement.
 * `grep` also calls such a transcript a binary file and prints nothing.
 */
public class Jdk21StrictLocaleData {

    static void report(String label, Locale l) {
        DecimalFormatSymbols s = (l == null)
                ? DecimalFormatSymbols.getInstance()
                : DecimalFormatSymbols.getInstance(l);
        System.out.printf("%-22s grouping=U+%04X decimal=U+%04X%n",
                label, (int) s.getGroupingSeparator(), (int) s.getDecimalSeparator());
    }

    public static void main(String[] args) {
        System.out.println("default=" + Locale.getDefault()
                + " FORMAT=" + Locale.getDefault(Locale.Category.FORMAT));
        System.out.println("user.language=" + System.getProperty("user.language")
                + " user.country=" + System.getProperty("user.country"));
        System.out.println("format(1234.5)=" + toCodePoints(NumberFormat.getInstance().format(1234.5)));

        report("default", null);
        report("explicit de-DE", Locale.GERMANY);          // expect grouping U+002E decimal U+002C
        report("explicit ru-RU", Locale.forLanguageTag("ru-RU")); // expect U+00A0 / U+002C
        report("explicit en-US", Locale.US);               // expect U+002C / U+002E -- the control:
                                                           // it is US-shaped even when everything works,
                                                           // so it must NOT be read as a pass on its own
    }

    static String toCodePoints(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7E) sb.append(String.format("<U+%04X>", (int) c));
            else sb.append(c);
        }
        return sb.toString();
    }
}
