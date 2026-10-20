import java.text.DecimalFormatSymbols;
import java.util.Locale;

/**
 * The residual: --jdk-only on JDK 21 answers US separators while the CLDR
 * adapter reports a full 1063 supported locales. So the adapter KNOWS de-DE and
 * the DATA lookup still comes back US. This asks the layer below the adapter --
 * the per-locale CLDR resource classes in jdk.localedata -- whether they are
 * reachable at all, and prints the separators in the same run so the two can be
 * read together.
 *
 * en-US is a control: it must be identical on every arm, because US data is
 * also what the defect produces.
 */
public class FmtBundle {

    static void cls(String cn) {
        try {
            Class<?> c = Class.forName(cn);
            System.out.println("  " + cn + " = OK  module="
                    + c.getModule().getName() + "  loader="
                    + (c.getClassLoader() == null ? "boot" : c.getClassLoader().getName()));
        } catch (Throwable t) {
            System.out.println("  " + cn + " -> " + t.getClass().getName());
        }
    }

    static void sep(String label, Locale l) {
        DecimalFormatSymbols d = DecimalFormatSymbols.getInstance(l);
        System.out.println("  " + label + " decimal=U+" + String.format("%04X", (int) d.getDecimalSeparator())
                + " grouping=U+" + String.format("%04X", (int) d.getGroupingSeparator()));
    }

    public static void main(String[] args) {
        System.out.println("java.version = " + System.getProperty("java.version"));
        System.out.println("separators:");
        sep("de-DE          ", Locale.GERMANY);
        sep("en-US [control]", Locale.US);
        System.out.println("CLDR per-locale resource classes (jdk.localedata):");
        cls("sun.text.resources.cldr.ext.FormatData_de");
        cls("sun.text.resources.cldr.ext.FormatData_fr");
        System.out.println("java.base base resource class (control, must load on any arm):");
        cls("sun.text.resources.cldr.FormatData");
        System.out.println("a jdk.localedata class already proven reachable:");
        cls("sun.util.resources.cldr.provider.CLDRLocaleDataMetaInfo");
    }
}
