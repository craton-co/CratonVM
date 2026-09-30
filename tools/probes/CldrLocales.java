import java.lang.reflect.Method;
import java.util.Locale;

/**
 * The CLDR adapter constructs fine on CratonVM, yet
 * LocaleProviderAdapter.getAdapter falls back for every locale. That selection
 * is driven by the adapter's AVAILABLE-LOCALE set, so this asks for it
 * directly. An empty (or root-only) set explains the fallback without any
 * exception ever being thrown, which is why nothing showed up in a stack trace.
 */
public class CldrLocales {

    static void report(String label, Object adapter) throws Exception {
        Class<?> lpa = Class.forName("sun.util.locale.provider.LocaleProviderAdapter");
        Method type = lpa.getMethod("getAdapterType");
        System.out.println("  " + label + " adapterType = " + type.invoke(adapter));

        Method gdfsp = lpa.getMethod("getDecimalFormatSymbolsProvider");
        Object prov = gdfsp.invoke(adapter);
        System.out.println("  " + label + " dfsProvider = "
                + (prov == null ? "null" : prov.getClass().getName()));
        if (prov != null) {
            Locale[] avail = (Locale[]) prov.getClass()
                    .getMethod("getAvailableLocales").invoke(prov);
            System.out.println("  " + label + " availableLocales = "
                    + (avail == null ? "null" : String.valueOf(avail.length)));
            if (avail != null) {
                boolean de = false;
                for (Locale l : avail) {
                    if ("de".equals(l.getLanguage())) { de = true; break; }
                }
                System.out.println("  " + label + " contains a de locale = " + de);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> lpa = Class.forName("sun.util.locale.provider.LocaleProviderAdapter");

        try {
            Method fp = lpa.getMethod("getAdapterPreference");
            System.out.println("adapterPreference = " + fp.invoke(null));
        } catch (Throwable t) {
            System.out.println("adapterPreference -> " + t.getClass().getName());
        }

        Class<?> cldr = Class.forName("sun.util.cldr.CLDRLocaleProviderAdapter");
        java.lang.reflect.Constructor<?> c = cldr.getDeclaredConstructor();
        c.setAccessible(true);
        Object adapter = c.newInstance();
        report("CLDR", adapter);

        Method forType = lpa.getMethod("forType",
                Class.forName("sun.util.locale.provider.LocaleProviderAdapter$Type"));
        Object[] types = Class.forName("sun.util.locale.provider.LocaleProviderAdapter$Type")
                .getEnumConstants();
        for (Object t : types) {
            try {
                Object a = forType.invoke(null, t);
                System.out.println("forType(" + t + ") = "
                        + (a == null ? "null" : a.getClass().getName()));
            } catch (Throwable e) {
                System.out.println("forType(" + t + ") -> "
                        + e.getClass().getName() + " / " + e.getCause());
            }
        }
    }
}
