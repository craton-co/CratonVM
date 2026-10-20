import java.lang.reflect.Method;
import java.util.ServiceLoader;

/**
 * The module graph is fine and BOTH LocaleDataMetaInfo providers are
 * discoverable, so the 5-vs-1063 gap is not a missing module. This asks each
 * provider what it actually returns: CLDRLocaleProviderAdapter builds its
 * supported set by merging java.base`s base tags with the non-base provider`s
 * `availableLanguageTags(category)`, so an empty or short answer from the
 * non-base provider produces exactly the observed symptom with nothing
 * throwing.
 */
public class LocaleTags {
    public static void main(String[] args) throws Exception {
        Class<?> spi = Class.forName("sun.util.locale.provider.LocaleDataMetaInfo");
        Method getType = spi.getMethod("getType");
        Method avail = spi.getMethod("availableLanguageTags", String.class);

        for (Object p : ServiceLoader.load(spi)) {
            String cn = p.getClass().getName();
            Object type;
            try {
                type = getType.invoke(p);
            } catch (Throwable t) {
                type = "<" + t.getClass().getSimpleName() + ">";
            }
            System.out.println("provider " + cn + "  type=" + type);
            for (String cat : new String[] { "AvailableLocales", "FormatData", "CurrencyNames" }) {
                try {
                    Object tags = avail.invoke(p, cat);
                    String s = (tags == null) ? "null" : String.valueOf(tags);
                    int count = "null".equals(s) ? -1 : (s.trim().isEmpty() ? 0 : s.trim().split("\s+").length);
                    System.out.println("    " + cat + "  count=" + count
                            + "  head=" + (s.length() > 90 ? s.substring(0, 90) + "..." : s));
                } catch (Throwable t) {
                    System.out.println("    " + cat + " -> " + t.getClass().getName()
                            + ": " + t.getMessage());
                }
            }
        }
    }
}
