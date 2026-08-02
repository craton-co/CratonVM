import java.time.temporal.ChronoUnit;

/**
 * `Enum.name()` is final on `java.lang.Enum` and must read the slot Enum itself
 * declares — never a same-named field a subclass happens to declare.
 *
 * `java.time.temporal.ChronoUnit` declares its own `private final String name`
 * holding the DISPLAY form ("Seconds"), while `Enum.name` holds the constant
 * identifier ("SECONDS"). `ChronoUnit.toString()` overrides to return the
 * display form, so only `name()` distinguishes them — and only `name()` is the
 * one every identity lookup goes through.
 *
 * When `name()` returns the shadow, `Enum.valueOf(ChronoUnit.class, "SECONDS")`
 * finds no match and throws, which is what made
 * `@DurationUnit(ChronoUnit.SECONDS)` fall back to a synthetic enum object and
 * failed all 14 LocalDevToolsAutoConfigurationTests via Spring's
 * `DurationStyle` ("Unknown unit null").
 *
 * `java.util.concurrent.TimeUnit` is the control: no shadowing field, so it
 * works either way.
 */
public class EnumShadowedNameFieldProbe {

    static int failures = 0;

    static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "  OK   " : "  FAIL ") + what + "   " + detail);
        if (!ok) {
            failures++;
        }
    }

    public static void main(String[] args) {
        System.out.println("[1] ChronoUnit — declares its own `name` field shadowing Enum.name");
        check("SECONDS.name()", "SECONDS".equals(ChronoUnit.SECONDS.name()),
                "name()=" + ChronoUnit.SECONDS.name());
        check("SECONDS.toString() (the display form)", "Seconds".equals(ChronoUnit.SECONDS.toString()),
                "toString()=" + ChronoUnit.SECONDS);
        check("MILLIS.name()", "MILLIS".equals(ChronoUnit.MILLIS.name()),
                "name()=" + ChronoUnit.MILLIS.name());
        check("valueOf(SECONDS) identity", roundTrip(ChronoUnit.SECONDS), "");
        check("valueOf(MINUTES) identity", roundTrip(ChronoUnit.MINUTES), "");
        check("ordinal() preserved", ChronoUnit.SECONDS.ordinal() == 3,
                "ordinal=" + ChronoUnit.SECONDS.ordinal());
        check("valueOf by the real identifier", Enum.valueOf(ChronoUnit.class, "SECONDS") == ChronoUnit.SECONDS,
                "the lookup Spring's DurationStyle depends on");

        System.out.println("[2] every ChronoUnit constant round-trips through name()/valueOf");
        int bad = 0;
        StringBuilder names = new StringBuilder();
        for (ChronoUnit u : ChronoUnit.values()) {
            if (!roundTrip(u)) {
                bad++;
                names.append(' ').append(u.toString());
            }
        }
        check("all constants", bad == 0, bad == 0 ? ChronoUnit.values().length + " constants"
                : bad + " failed:" + names);

        System.out.println("[3] TimeUnit — no shadowing field (control)");
        check("SECONDS.name()", "SECONDS".equals(java.util.concurrent.TimeUnit.SECONDS.name()),
                "name()=" + java.util.concurrent.TimeUnit.SECONDS.name());
        check("valueOf identity",
                java.util.concurrent.TimeUnit.valueOf("SECONDS") == java.util.concurrent.TimeUnit.SECONDS, "");

        System.out.println();
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        System.exit(failures == 0 ? 0 : 1);
    }

    private static boolean roundTrip(ChronoUnit u) {
        try {
            return Enum.valueOf(ChronoUnit.class, u.name()) == u;
        }
        catch (Throwable ex) {
            return false;
        }
    }
}
