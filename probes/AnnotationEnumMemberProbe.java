import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.time.temporal.ChronoUnit;

/**
 * An enum member read back from an annotation must be the interned constant
 * itself — `== ChronoUnit.SECONDS`, with a working `name()`/`toString()` and a
 * usable `ordinal()`. Spring relies on the identity comparison directly:
 *
 *     DurationStyle.SIMPLE.parse(value, unit)
 *       -> Unit.fromChronoUnit(unit)
 *            for (Unit candidate : values())
 *                if (candidate.chronoUnit == chronoUnit) return candidate;   // identity
 *            throw new IllegalArgumentException("Unknown unit " + chronoUnit);
 *
 * where `unit` comes from `@DurationUnit(ChronoUnit.SECONDS)` on the target
 * property. On CratonVM that threw `Unknown unit null` while binding
 * `spring.web.resources.cache.period` — so the value was non-null (it passed
 * `fromChronoUnit`'s own null check) yet matched no constant and rendered as
 * "null". All 14 LocalDevToolsAutoConfigurationTests failed on that.
 *
 * Checks a JDK enum (ChronoUnit, loaded from java.base) and a local enum
 * separately: the two travel different class-resolution paths, and only the JDK
 * one is exercised by Spring here.
 */
public class AnnotationEnumMemberProbe {

    enum Local {
        ALPHA, BETA, GAMMA
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ ElementType.FIELD, ElementType.METHOD, ElementType.TYPE })
    @interface WithJdkEnum {

        ChronoUnit value();

        ChronoUnit other() default ChronoUnit.MILLIS;
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ ElementType.FIELD, ElementType.METHOD, ElementType.TYPE })
    @interface WithLocalEnum {

        Local value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ ElementType.FIELD, ElementType.METHOD, ElementType.TYPE })
    @interface WithEnumArray {

        ChronoUnit[] value();
    }

    @WithJdkEnum(ChronoUnit.SECONDS)
    @WithLocalEnum(Local.BETA)
    @WithEnumArray({ ChronoUnit.SECONDS, ChronoUnit.MINUTES })
    static class Holder {

        @WithJdkEnum(ChronoUnit.SECONDS)
        String field;

        @WithJdkEnum(ChronoUnit.SECONDS)
        void method() {
        }
    }

    static int failures = 0;

    static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "  OK   " : "  FAIL ") + what + "   " + detail);
        if (!ok) {
            failures++;
        }
    }

    /** The three properties Spring's identity-based lookup depends on. */
    static void assertIsRealConstant(String where, ChronoUnit got, ChronoUnit expected) {
        check(where + " identity (== the interned constant)", got == expected,
                "got@" + System.identityHashCode(got) + " expected@" + System.identityHashCode(expected));
        String name;
        try {
            name = got.name();
        }
        catch (Throwable ex) {
            name = "<threw " + ex.getClass().getSimpleName() + ">";
        }
        check(where + " name()", expected.name().equals(name), "name=" + name);
        // NOT name(): ChronoUnit overrides toString() ("Seconds", not "SECONDS").
        // Compare against the interned constant's own rendering.
        check(where + " toString()", expected.toString().equals(String.valueOf(got)), "toString=" + got);
        // The exact loop Spring runs.
        ChronoUnit matched = null;
        for (ChronoUnit candidate : ChronoUnit.values()) {
            if (candidate == got) {
                matched = candidate;
                break;
            }
        }
        check(where + " found by values() identity scan", matched != null,
                matched == null ? "no ChronoUnit constant is == this value" : "matched " + matched);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("[1] JDK enum member on a TYPE annotation");
        WithJdkEnum onType = Holder.class.getAnnotation(WithJdkEnum.class);
        check("annotation present", onType != null, String.valueOf(onType));
        if (onType != null) {
            assertIsRealConstant("[1] value()", onType.value(), ChronoUnit.SECONDS);
            assertIsRealConstant("[1] default other()", onType.other(), ChronoUnit.MILLIS);
        }

        System.out.println("[2] JDK enum member on a FIELD annotation (Spring's shape)");
        Field field = Holder.class.getDeclaredField("field");
        WithJdkEnum onField = field.getAnnotation(WithJdkEnum.class);
        check("annotation present", onField != null, String.valueOf(onField));
        if (onField != null) {
            assertIsRealConstant("[2] value()", onField.value(), ChronoUnit.SECONDS);
        }

        System.out.println("[3] JDK enum member on a METHOD annotation");
        Method method = Holder.class.getDeclaredMethod("method");
        WithJdkEnum onMethod = method.getAnnotation(WithJdkEnum.class);
        check("annotation present", onMethod != null, String.valueOf(onMethod));
        if (onMethod != null) {
            assertIsRealConstant("[3] value()", onMethod.value(), ChronoUnit.SECONDS);
        }

        System.out.println("[4] enum ARRAY member");
        WithEnumArray arr = Holder.class.getAnnotation(WithEnumArray.class);
        check("annotation present", arr != null, String.valueOf(arr));
        if (arr != null) {
            ChronoUnit[] units = arr.value();
            check("[4] length", units.length == 2, "length=" + units.length);
            if (units.length == 2) {
                assertIsRealConstant("[4] [0]", units[0], ChronoUnit.SECONDS);
                assertIsRealConstant("[4] [1]", units[1], ChronoUnit.MINUTES);
            }
        }

        System.out.println("[5] locally-declared enum member (control)");
        WithLocalEnum local = Holder.class.getAnnotation(WithLocalEnum.class);
        check("annotation present", local != null, String.valueOf(local));
        if (local != null) {
            Local got = local.value();
            check("[5] identity", got == Local.BETA,
                    "got@" + System.identityHashCode(got) + " expected@" + System.identityHashCode(Local.BETA));
            check("[5] name()", "BETA".equals(got.name()), "name=" + got.name());
            check("[5] ordinal()", got.ordinal() == Local.BETA.ordinal(), "ordinal=" + got.ordinal());
        }

        System.out.println();
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        System.exit(failures == 0 ? 0 : 1);
    }
}
