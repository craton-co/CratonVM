import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.Locale;

/** Prints WHICH locales the CLDR adapter claims, when it claims few enough to list. */
public class CldrWhich {
    public static void main(String[] args) throws Exception {
        Class<?> lpa = Class.forName("sun.util.locale.provider.LocaleProviderAdapter");
        Class<?> cldr = Class.forName("sun.util.cldr.CLDRLocaleProviderAdapter");
        Constructor<?> c = cldr.getDeclaredConstructor();
        c.setAccessible(true);
        Object adapter = c.newInstance();
        Object prov = lpa.getMethod("getDecimalFormatSymbolsProvider").invoke(adapter);
        Locale[] avail = (Locale[]) prov.getClass().getMethod("getAvailableLocales").invoke(prov);
        System.out.println("count = " + avail.length);
        if (avail.length <= 40) {
            Arrays.sort(avail, (a, b) -> a.toString().compareTo(b.toString()));
            for (Locale l : avail) {
                System.out.println("   [" + l + "]");
            }
        }
        Method gl = cldr.getMethod("getLanguageTagSet", String.class);
        gl.setAccessible(true);
        for (String key : new String[] { "AvailableLocales", "FormatData" }) {
            try {
                Object set = gl.invoke(adapter, key);
                String s = String.valueOf(set);
                System.out.println("getLanguageTagSet(" + key + ") size/text = "
                        + (s.length() > 200 ? s.substring(0, 200) + " ..." : s));
            } catch (Throwable t) {
                System.out.println("getLanguageTagSet(" + key + ") -> " + t.getClass().getName()
                        + " / " + t.getCause());
            }
        }
    }
}
