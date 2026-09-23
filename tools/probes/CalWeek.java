import java.util.Locale;

/**
 * Decides the question left open by
 * docs/known-issues/calendar-firstdayofweek-is-root-data-because-the-provider-lookup-answers-null-20260909.md:
 * when the calendar week rules come back as root data, is that because the
 * WRONG provider/resources were selected, or because the right ones hold wrong
 * DATA?
 *
 * The page records that naively delegating the `CalendarDataProvider` arm makes
 * every locale answer 2/1 (CLDR root) and regresses the `en-US` control, so the
 * arm was reverted. That measurement went through `LocaleServiceProviderPool`.
 * This one goes STRAIGHT at the CLDR adapter, bypassing both the pool and the
 * shadowed `getLocaleServiceProvider`, and asks each layer separately:
 *
 *   1. adapter.getCalendarDataProvider().getFirstDayOfWeek(de-DE)
 *   2. adapter.getLocaleResources(de-DE).getCalendarData("firstDayOfWeek")
 *
 * If layer 2 is right and layer 1 is wrong, the provider is misreading correct
 * resources. If layer 2 is ALSO root, the resources handed out for a non-root
 * request are root -- the same fault as the retired DecimalFormatSymbols row,
 * and the fix belongs there rather than in the delegation table.
 *
 * `en-US` is the control and it is load-bearing here: root CLDR happens to be
 * `firstDay=mon(2), minDays=1`, and en-US is `1/1`, so the two are
 * distinguishable. HotSpot must print 1/1 for en-US and 2/4 for de-DE; an arm
 * where it does not is a broken probe.
 */
public class CalWeek {

    static Object call(Object on, Class<?> owner, String name, Class<?>[] sig, Object... args)
            throws Throwable {
        java.lang.reflect.Method m = owner.getDeclaredMethod(name, sig);
        try {
            m.setAccessible(true);
        } catch (Throwable ignored) {
            // reported by the invoke
        }
        return m.invoke(on, args);
    }

    static String rootOf(Throwable t) {
        Throwable c = t;
        while (c.getCause() != null) {
            c = c.getCause();
        }
        return c.getClass().getName() + (c.getMessage() == null ? "" : ": " + c.getMessage());
    }

    static void probe(Locale loc) {
        System.out.println("  [" + loc + "]");
        try {
            Class<?> lpa = Class.forName("sun.util.locale.provider.LocaleProviderAdapter");
            Class<?> typeCls = Class.forName("sun.util.locale.provider.LocaleProviderAdapter$Type");
            @SuppressWarnings({"unchecked", "rawtypes"})
            Object cldrType = Enum.valueOf((Class) typeCls, "CLDR");
            Object cldr = call(null, lpa, "forType", new Class<?>[]{typeCls}, cldrType);
            if (cldr == null) {
                System.out.println("    forType(CLDR) = null -- nothing below is meaningful");
                return;
            }

            // LAYER 1 -- the provider the delegation table would have returned.
            try {
                Object prov = call(cldr, lpa, "getCalendarDataProvider", new Class<?>[]{});
                if (prov == null) {
                    System.out.println("    L1 getCalendarDataProvider() = null");
                } else {
                    Class<?> cdp = Class.forName("java.util.spi.CalendarDataProvider");
                    Object fd = call(prov, cdp, "getFirstDayOfWeek", new Class<?>[]{Locale.class}, loc);
                    Object md = call(prov, cdp, "getMinimalDaysInFirstWeek",
                            new Class<?>[]{Locale.class}, loc);
                    System.out.println("    L1 provider(" + prov.getClass().getSimpleName()
                            + ")  firstDay=" + fd + " minimalDays=" + md);
                }
            } catch (Throwable t) {
                System.out.println("    L1 provider -> " + rootOf(t));
            }

            // LAYER 2 -- the resources underneath it, asked by the same key the
            // provider uses.
            try {
                Object lr = call(cldr, lpa, "getLocaleResources", new Class<?>[]{Locale.class}, loc);
                if (lr == null) {
                    System.out.println("    L2 getLocaleResources = null");
                } else {
                    Object inner;
                    try {
                        java.lang.reflect.Field f = lr.getClass().getDeclaredField("locale");
                        f.setAccessible(true);
                        inner = f.get(lr);
                    } catch (Throwable t2) {
                        inner = "<unreadable>";
                    }
                    Object cd = call(lr, lr.getClass(), "getCalendarData",
                            new Class<?>[]{String.class}, "firstDayOfWeek");
                    Object md = call(lr, lr.getClass(), "getCalendarData",
                            new Class<?>[]{String.class}, "minimalDaysInFirstWeek");
                    System.out.println("    L2 resources.locale=[" + inner + "]"
                            + "  firstDayOfWeek=\"" + cd + "\""
                            + "  minimalDaysInFirstWeek=\"" + md + "\""
                            + (String.valueOf(inner).isEmpty() ? "    <-- ROOT resources" : ""));
                }
            } catch (Throwable t) {
                System.out.println("    L2 resources -> " + rootOf(t));
            }
        } catch (Throwable t) {
            System.out.println("    setup -> " + rootOf(t));
        }
    }

    public static void main(String[] args) {
        System.out.println("java.version = " + System.getProperty("java.version"));
        // The public answer, for reference against the layers below.
        for (Locale l : new Locale[]{Locale.US, Locale.GERMANY, Locale.FRANCE}) {
            java.util.Calendar c = java.util.Calendar.getInstance(l);
            System.out.println("  public Calendar[" + l + "] firstDay=" + c.getFirstDayOfWeek()
                    + " minimalDays=" + c.getMinimalDaysInFirstWeek());
        }
        System.out.println();
        System.out.println("== layers ==");
        probe(Locale.US);
        probe(Locale.GERMANY);
        System.out.println();
        System.out.println("DONE");
    }
}
