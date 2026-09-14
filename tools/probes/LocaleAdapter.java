import java.text.DecimalFormatSymbols;
import java.util.Locale;
import java.util.ResourceBundle;

/**
 * Next step for the JDK 21 strict `textformat` row, which was narrowed to
 * locale DATA rather than locale SELECTION: asking for
 * DecimalFormatSymbols.getInstance(Locale.GERMANY) BY NAME still answered US
 * separators, which rules out every user.language / user.country theory.
 *
 * The open question is where the data comes from. This prints the provider
 * chain rather than the answer, so a wrong separator can be attributed to an
 * adapter that is missing, to one that fell back to a different tier, or to a
 * resource bundle that did not load.
 *
 * Locale.US is a control: it must print the SAME values on every arm, because
 * US separators are also what the defect produces. A run where the control
 * differs is a broken probe, not a finding.
 */
public class LocaleAdapter {

    static void syms(String label, Locale loc) {
        DecimalFormatSymbols d = DecimalFormatSymbols.getInstance(loc);
        System.out.println("  " + label
                + "  decimal=U+" + String.format("%04X", (int) d.getDecimalSeparator())
                + "  grouping=U+" + String.format("%04X", (int) d.getGroupingSeparator())
                + "  minus=U+" + String.format("%04X", (int) d.getMinusSign()));
    }

    static void adapter(String what, Locale loc) {
        try {
            Class<?> lpa = Class.forName("sun.util.locale.provider.LocaleProviderAdapter");
            Class<?> spi = Class.forName("java.text.spi.DecimalFormatSymbolsProvider");
            Object got = lpa.getMethod("getAdapter", Class.class, Locale.class)
                    .invoke(null, spi, loc);
            System.out.println("  adapter for " + what + " = "
                    + (got == null ? "null" : got.getClass().getName()));
        } catch (Throwable t) {
            System.out.println("  adapter for " + what + " -> "
                    + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static void bundle(String base, Locale loc) {
        try {
            ResourceBundle rb = ResourceBundle.getBundle(base, loc);
            System.out.println("  bundle " + base + " [" + loc + "] -> locale="
                    + rb.getLocale() + "  class=" + rb.getClass().getName());
        } catch (Throwable t) {
            System.out.println("  bundle " + base + " [" + loc + "] -> "
                    + t.getClass().getName());
        }
    }

    public static void main(String[] args) {
        System.out.println("java.version           = " + System.getProperty("java.version"));
        System.out.println("java.locale.providers  = " + System.getProperty("java.locale.providers"));
        System.out.println("user.language/country  = " + System.getProperty("user.language")
                + "/" + System.getProperty("user.country"));
        System.out.println("Locale.getDefault()    = " + Locale.getDefault());

        System.out.println("separators (asked for BY NAME, so selection is not the variable):");
        syms("de-DE   ", Locale.GERMANY);
        syms("fr-FR   ", Locale.FRANCE);
        syms("en-US  [control]", Locale.US);

        System.out.println("provider chain:");
        adapter("de-DE", Locale.GERMANY);
        adapter("en-US [control]", Locale.US);

        System.out.println("resource bundles:");
        bundle("sun.text.resources.FormatData", Locale.GERMANY);
        bundle("sun.text.resources.cldr.FormatData", Locale.GERMANY);
        bundle("sun.text.resources.FormatData", Locale.US);
    }
}
