import java.time.temporal.ChronoUnit;

/**
 * `Enum.valueOf(Class, String)` and `Class.getEnumConstants()` must work on an
 * enum class that has been LOADED but not yet INITIALIZED. Both read `$VALUES`,
 * which only exists once `<clinit>` has run, so both have to force
 * initialization first — on HotSpot `getEnumConstantsShared` invokes the
 * class's own `values()`, which does.
 *
 * This is the mechanism behind the annotation defect: materializing
 * `@DurationUnit(ChronoUnit.SECONDS)` resolves the enum class and calls
 * `Enum.valueOf`, but nothing had initialized `ChronoUnit`, so `$VALUES` was
 * null, `valueOf` failed, and the annotation code fell back to a synthetic
 * enum-shaped object with a null name and ordinal 0. Spring's
 * `DurationStyle` then reported `Unknown unit null` and every
 * LocalDevToolsAutoConfigurationTests test failed.
 *
 * A class LITERAL (`ChronoUnit.class`) does not initialize, and neither does
 * `Class.forName(name, false, loader)` — both are used here deliberately so the
 * class reaches the calls uninitialized, which is the state the annotation path
 * finds it in.
 */
public class EnumValueOfUninitializedProbe {

    static int failures = 0;

    static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "  OK   " : "  FAIL ") + what + "   " + detail);
        if (!ok) {
            failures++;
        }
    }

    public static void main(String[] args) throws Exception {
        // Resolve without initializing. Nothing above has touched ChronoUnit.
        Class<?> loaded = Class.forName("java.time.temporal.ChronoUnit", false,
                EnumValueOfUninitializedProbe.class.getClassLoader());
        System.out.println("loaded (uninitialized) = " + loaded.getName());

        System.out.println("[1] getEnumConstants() on an uninitialized enum");
        Object[] constants = loaded.getEnumConstants();
        check("non-null", constants != null, constants == null ? "null" : (constants.length + " constants"));
        check("full set", constants != null && constants.length == ChronoUnit.values().length,
                constants == null ? "n/a" : constants.length + " vs " + ChronoUnit.values().length);

        System.out.println("[2] Enum.valueOf on an uninitialized enum");
        Object got = null;
        try {
            @SuppressWarnings({ "unchecked", "rawtypes" })
            Object v = Enum.valueOf((Class) loaded, "SECONDS");
            got = v;
            check("returned a value", got != null, String.valueOf(got));
        }
        catch (Throwable ex) {
            check("returned a value", false, ex.getClass().getName() + ": " + ex.getMessage());
        }
        check("is the interned constant", got == ChronoUnit.SECONDS,
                "got@" + System.identityHashCode(got) + " expected@"
                        + System.identityHashCode(ChronoUnit.SECONDS));
        check("name()", got instanceof Enum && "SECONDS".equals(((Enum<?>) got).name()),
                got instanceof Enum ? "name=" + ((Enum<?>) got).name() : "not an Enum");

        System.out.println("[3] the same calls once the class IS initialized (control)");
        ChronoUnit forceInit = ChronoUnit.SECONDS;
        check("control identity", Enum.valueOf(ChronoUnit.class, "SECONDS") == forceInit, "");

        System.out.println();
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        System.exit(failures == 0 ? 0 : 1);
    }
}
