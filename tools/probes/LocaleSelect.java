import java.text.DateFormatSymbols;
import java.text.DecimalFormatSymbols;
import java.text.NumberFormat;
import java.util.Calendar;
import java.util.Locale;

/**
 * Narrows the JDK 21 strict locale row one step past
 * docs/known-issues/jdk-only/strict-mode-on-jdk-21-answers-a-non-root-locale-request-with-root-resources-20260909.md,
 * which localised the defect to "adapter / resource SELECTION" and stopped there.
 *
 * That page records a caution: its `adapter for de-DE = ...` line printed
 * FallbackLocaleProviderAdapter in cells that are CORRECT, so one composite
 * LocaleProviderAdapter.getAdapter call cannot be the verdict. This probe
 * therefore never asks a single composite question.
 *
 * Section A uses PUBLIC API only -- no reflection and no module access, so no
 * step of it can be mute -- and asks whether the fault is specific to the
 * DecimalFormatSymbols route or general to every locale-sensitive service.
 * Section B then asks the individual sub-questions that
 * LocaleProviderAdapter.findAdapter asks, reporting each step separately so a
 * step that COULD NOT RUN stays distinguishable from one that ran and answered.
 *
 * Locale.US is the control on every line: US data is also what the defect
 * PRODUCES, so an arm where the control differs is a broken probe, not a
 * finding.
 */
public class LocaleSelect {

    static String cp(char c) {
        return "U+" + String.format("%04X", (int) c);
    }

    /** Section A: every answer comes from public API, so no step here can be mute. */
    static void publicApi(String label, Locale loc) {
        System.out.println("  [" + label + "]");

        DecimalFormatSymbols d = DecimalFormatSymbols.getInstance(loc);
        System.out.println("    DecimalFormatSymbols decimal=" + cp(d.getDecimalSeparator())
                + " grouping=" + cp(d.getGroupingSeparator()));

        // Routed through LocaleProviderAdapter.getAdapter(DateFormatSymbolsProvider.class,
        // loc) and then getLocaleResources(loc) -- the SAME two steps, for a different
        // service. Correct here while the line above is wrong means selection is not
        // globally broken, which is the single most useful bit this probe can return.
        try {
            String[] months = DateFormatSymbols.getInstance(loc).getMonths();
            String[] days = DateFormatSymbols.getInstance(loc).getWeekdays();
            System.out.println("    DateFormatSymbols   month[0]=" + months[0]
                    + " weekday[2]=" + days[2]);
        } catch (Throwable t) {
            System.out.println("    DateFormatSymbols   -> " + t);
        }

        try {
            System.out.println("    NumberFormat        "
                    + NumberFormat.getInstance(loc).format(1234567.89));
        } catch (Throwable t) {
            System.out.println("    NumberFormat        -> " + t);
        }
        try {
            System.out.println("    Currency symbol     "
                    + NumberFormat.getCurrencyInstance(loc).getCurrency().getSymbol(loc));
        } catch (Throwable t) {
            System.out.println("    Currency symbol     -> " + t);
        }
        try {
            // CalendarDataProvider -- a third service through the same two steps.
            Calendar cal = Calendar.getInstance(loc);
            System.out.println("    Calendar firstDay   " + cal.getFirstDayOfWeek()
                    + " minimalDays=" + cal.getMinimalDaysInFirstWeek());
        } catch (Throwable t) {
            System.out.println("    Calendar            -> " + t);
        }
        try {
            System.out.println("    DisplayLanguage(de) " + loc.getDisplayLanguage(Locale.GERMAN));
        } catch (Throwable t) {
            System.out.println("    DisplayLanguage     -> " + t);
        }
    }

    static Object call(Object on, Class<?> owner, String name, Class<?>[] sig, Object... args)
            throws Throwable {
        java.lang.reflect.Method m = owner.getDeclaredMethod(name, sig);
        try {
            m.setAccessible(true);
        } catch (Throwable ignored) {
            // Reported by the invoke below; setAccessible failing is not itself the answer.
        }
        return m.invoke(on, args);
    }

    /**
     * Section B: the sub-questions findAdapter asks, one at a time. Each step prints
     * its own failure, so a step that could not run is never read as an answer.
     */
    static void selection(Locale loc) {
        System.out.println("  [selection for " + loc + "]");
        Class<?> lpa;
        Class<?> spiDfs;
        try {
            lpa = Class.forName("sun.util.locale.provider.LocaleProviderAdapter");
            spiDfs = Class.forName("java.text.spi.DecimalFormatSymbolsProvider");
        } catch (Throwable t) {
            System.out.println("    STEP0 class lookup  -> " + t);
            return;
        }

        // STEP 1 -- is there a CLDR adapter at all?
        Object cldr = null;
        try {
            Class<?> typeCls = Class.forName("sun.util.locale.provider.LocaleProviderAdapter$Type");
            @SuppressWarnings({"unchecked", "rawtypes"})
            Object cldrType = Enum.valueOf((Class) typeCls, "CLDR");
            cldr = call(null, lpa, "forType", new Class<?>[]{typeCls}, cldrType);
            System.out.println("    STEP1 forType(CLDR) = "
                    + (cldr == null ? "null" : cldr.getClass().getName()));
        } catch (Throwable t) {
            System.out.println("    STEP1 forType(CLDR) -> " + rootOf(t));
        }

        // STEP 2 -- does that adapter hand out a DecimalFormatSymbolsProvider?
        Object provider = null;
        if (cldr != null) {
            try {
                provider = call(cldr, lpa, "getLocaleServiceProvider",
                        new Class<?>[]{Class.class}, spiDfs);
                System.out.println("    STEP2 getLocaleServiceProvider(DFSProvider) = "
                        + (provider == null ? "null" : provider.getClass().getName()));
            } catch (Throwable t) {
                System.out.println("    STEP2 getLocaleServiceProvider -> " + rootOf(t));
            }
        }

        // STEP 3 -- THE DECIDER. findAdapter returns this adapter only if this is true.
        if (provider != null) {
            try {
                Object ok = call(provider, java.util.spi.LocaleServiceProvider.class,
                        "isSupportedLocale", new Class<?>[]{Locale.class}, loc);
                System.out.println("    STEP3 provider.isSupportedLocale(" + loc + ") = " + ok
                        + "    (false here means the CLDR adapter is SKIPPED)");
            } catch (Throwable t) {
                System.out.println("    STEP3 isSupportedLocale -> " + rootOf(t));
            }
            // STEP 3b -- the set STEP3 consults, where the provider exposes one.
            try {
                Class<?> alt = Class.forName("sun.util.locale.provider.AvailableLanguageTags");
                if (alt.isInstance(provider)) {
                    Object tags = call(provider, alt, "getAvailableLanguageTags", new Class<?>[]{});
                    java.util.Set<?> s = (java.util.Set<?>) tags;
                    System.out.println("    STEP3b availableLanguageTags size=" + s.size()
                            + " contains(de)=" + s.contains("de")
                            + " contains(de-DE)=" + s.contains("de-DE"));
                } else {
                    System.out.println("    STEP3b provider is NOT AvailableLanguageTags");
                }
            } catch (Throwable t) {
                System.out.println("    STEP3b availableLanguageTags -> " + rootOf(t));
            }
        }

        // STEP 4 -- the composite. Kept only so it sits NEXT TO the steps above, never
        // alone: on its own it read the same in correct and incorrect cells.
        Object chosen = null;
        try {
            chosen = call(null, lpa, "getAdapter", new Class<?>[]{Class.class, Locale.class},
                    spiDfs, loc);
            System.out.println("    STEP4 getAdapter(DFSProvider," + loc + ") = "
                    + (chosen == null ? "null" : chosen.getClass().getName()));
        } catch (Throwable t) {
            System.out.println("    STEP4 getAdapter -> " + rootOf(t));
        }

        // STEP 5 -- which LocaleResources does each adapter hand back, and for which
        // locale? This is the object the page found to be the ROOT one.
        Object[] adapters = {chosen, cldr};
        String[] names = {"chosen", "cldr"};
        for (int i = 0; i < adapters.length; i++) {
            Object adapter = adapters[i];
            if (adapter == null) {
                continue;
            }
            String who = names[i] + "=" + adapter.getClass().getSimpleName();
            try {
                Object lr = call(adapter, lpa, "getLocaleResources",
                        new Class<?>[]{Locale.class}, loc);
                if (lr == null) {
                    System.out.println("    STEP5 " + who + ".getLocaleResources = null");
                    continue;
                }
                Object inner;
                try {
                    java.lang.reflect.Field f = lr.getClass().getDeclaredField("locale");
                    f.setAccessible(true);
                    inner = f.get(lr);
                } catch (Throwable t2) {
                    inner = "<field unreadable: " + rootOf(t2) + ">";
                }
                System.out.println("    STEP5 " + who + ".getLocaleResources(" + loc + ") -> "
                        + lr.getClass().getSimpleName()
                        + " whose locale field = [" + inner + "]"
                        + (String.valueOf(inner).isEmpty() ? "    <-- ROOT" : ""));
            } catch (Throwable t) {
                System.out.println("    STEP5 " + who + ".getLocaleResources -> " + rootOf(t));
            }
        }
    }

    static String rootOf(Throwable t) {
        Throwable c = t;
        while (c.getCause() != null) {
            c = c.getCause();
        }
        return c.getClass().getName() + (c.getMessage() == null ? "" : ": " + c.getMessage());
    }

    public static void main(String[] args) {
        System.out.println("java.version          = " + System.getProperty("java.version"));
        System.out.println("java.locale.providers = " + System.getProperty("java.locale.providers"));
        System.out.println("Locale.getDefault()   = " + Locale.getDefault());
        System.out.println();
        System.out.println("== A. public API only (no step can be mute) ==");
        publicApi("CONTROL en-US", Locale.US);
        publicApi("de-DE", Locale.GERMANY);
        publicApi("fr-FR", Locale.FRANCE);
        System.out.println();
        System.out.println("== B. the steps findAdapter takes ==");
        selection(Locale.US);
        selection(Locale.GERMANY);
        System.out.println();
        System.out.println("DONE");
    }
}
