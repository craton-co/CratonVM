import java.util.Locale;

/**
 * decompose_locale reads a Locale through getLanguage/getCountry/getVariant.
 * In --jdk-only on JDK 21 it yields an empty language for the Locale held by
 * LocaleResources. This asks whether those accessors are broken generally in
 * that cell, or only for that particular instance.
 *
 * Locale.US is the control: it must answer "en"/"US" on every arm.
 */
public class LocAcc {
    static void show(String label, Locale l) {
        System.out.println("  " + label
                + " lang=[" + l.getLanguage() + "]"
                + " country=[" + l.getCountry() + "]"
                + " variant=[" + l.getVariant() + "]"
                + " toString=[" + l + "]");
    }

    public static void main(String[] args) throws Exception {
        System.out.println("java.version = " + System.getProperty("java.version"));
        show("Locale.GERMANY      ", Locale.GERMANY);
        show("Locale.FRANCE       ", Locale.FRANCE);
        show("Locale.US [control] ", Locale.US);
        show("new Locale(de,DE)   ", new Locale("de", "DE"));
        show("forLanguageTag de-DE", Locale.forLanguageTag("de-DE"));
    }
}
