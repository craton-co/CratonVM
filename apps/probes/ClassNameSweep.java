import java.util.concurrent.Callable;

/**
 * `java.lang.Class` name accessors, every receiver shape whose rules differ,
 * against a real HotSpot of the same version as the image.
 *
 * This is the review that lane 0's §1.4 reviewed-`Intrinsic` protocol requires
 * before `Class.getName` may be tagged, and it is written so the tag cannot
 * freeze a wrong answer: it covers every accessor the JDK DERIVES from
 * `getName` (`getTypeName`, `getCanonicalName`, `getSimpleName`,
 * `getPackageName`, `toString`) as well as `getName` itself,
 * so a change that repairs one and breaks another shows up here.
 *
 * Why the tag rather than a yield: `getName`'s real body is
 * `String name = this.name; return name != null ? name : initClassName();`,
 * and `java.lang.Class.name` in this VM is an OVERLAY — the mirror allocator
 * stores the VM's INTERNAL name there (`java/lang/Object`), and
 * `mirror_class_name_strict` and its callers read it back in that form on
 * purpose. Yielding therefore returns the internal form for every reference
 * type, which is worse than a null because nothing throws: it propagates into
 * every JDK name comparison.
 *
 * Probe hygiene, per docs/contributing/jdk-only-lane-operations.md §1:
 *   - stdout only; run the CratonVM arms with 2>/dev/null.
 *   - NO printf/String.format anywhere: java.util.Formatter reaches
 *     DecimalFormatSymbols -> LocaleProviderAdapter, which throws
 *     ServiceConfigurationError("Locale provider adapter \"CLDR\" cannot be
 *     instantiated") in the armed arm. The first version of this probe printed
 *     ZERO rows for that reason, and a zero-row arm is a mute instrument, not a
 *     result.
 *   - no identity hash codes, no addresses, no timings.
 *   - a lambda's and a hidden class's names contain a VM-chosen suffix, so this
 *     asks their SHAPE (does it contain "$$Lambda", does the prefix match the
 *     host class) and never the value.
 */
public class ClassNameSweep {

    static class Nested {
    }

    class Inner {
    }

    enum Color {
        RED {
            @Override
            String tag() {
                return "r";
            }
        };

        String tag() {
            return "?";
        }
    }

    record Point(int x, int y) {
    }

    interface Iface {
    }

    static int n = 0;

    static void row(String label, Callable<Object> s) {
        n++;
        Object v;
        try {
            v = s.call();
        } catch (Throwable t) {
            v = t.getClass().getName() + ": " + t.getMessage();
        }
        // No printf: java.util.Formatter reaches DecimalFormatSymbols ->
        // LocaleProviderAdapter, which throws ServiceConfigurationError in the
        // armed arm. An instrument that cannot run measures nothing, so this
        // pads by hand and stays on System.out.println.
        StringBuilder sb = new StringBuilder();
        String idx = Integer.toString(n);
        for (int i = idx.length(); i < 3; i++) sb.append(' ');
        sb.append(idx).append(' ').append(label);
        while (sb.length() < 42) sb.append(' ');
        sb.append(' ').append(v);
        System.out.println(sb);
    }

    /** Every accessor the JDK derives from getName, for one receiver. */
    static void family(String what, Class<?> c) {
        row(what + ".getName", c::getName);
        row(what + ".getTypeName", c::getTypeName);
        row(what + ".getCanonicalName", c::getCanonicalName);
        row(what + ".getSimpleName", c::getSimpleName);
        row(what + ".getPackageName", c::getPackageName);
        row(what + ".toString", c::toString);
    }

    public static void main(String[] args) throws Exception {
        family("Object", Object.class);
        family("nested", Nested.class);
        family("inner", Inner.class);
        family("iface", Iface.class);
        family("record", Point.class);
        family("enumConst", Color.RED.getClass());
        family("int", int.class);
        family("void", void.class);
        family("int[]", int[].class);
        family("String[][]", String[][].class);
        family("defaultPkg", ClassNameSweep.class);

        // Anonymous class: the trailing $N is javac's, identical on both VMs
        // because both load the same class file.
        Runnable anon = new Runnable() {
            public void run() {
            }
        };
        family("anon", anon.getClass());

        // Round-trips: the name a VM prints must be the name Class.forName takes
        // back. This is the property every JDK name comparison depends on, and
        // it fails loudly when getName answers the internal form.
        row("forName(Object.getName())==Object",
                () -> Class.forName(Object.class.getName()) == Object.class);
        row("forName(String[].getName())==String[]",
                () -> Class.forName(String[].class.getName()) == String[].class);
        row("forName(nested.getName())==nested",
                () -> Class.forName(Nested.class.getName()) == Nested.class);

        // Shape-only rows: the suffix is VM-chosen, so ask the shape.
        Runnable lambda = () -> {
        };
        row("lambda name contains $$Lambda",
                () -> lambda.getClass().getName().contains("$$Lambda"));
        row("lambda name starts with host",
                () -> lambda.getClass().getName().startsWith("ClassNameSweep"));
        row("lambda name has no slash",
                () -> !lambda.getClass().getName().contains("/")
                        || lambda.getClass().getName().indexOf('/') > lambda.getClass().getName().indexOf("$$Lambda"));

        // The invariant the whole family rests on: a binary name never contains
        // '/'. One row per receiver kind, so a single failure names its shape.
        row("no slash: Object", () -> !Object.class.getName().contains("/"));
        row("no slash: nested", () -> !Nested.class.getName().contains("/"));
        row("no slash: int[]", () -> !int[].class.getName().contains("/"));
        row("no slash: String[][]", () -> !String[][].class.getName().contains("/"));
        row("no slash: record", () -> !Point.class.getName().contains("/"));
        row("no slash: anon", () -> !anon.getClass().getName().contains("/"));

        // getName is what ServiceLoader's caller check compares, and it is the
        // reason this row is not cosmetic.
        row("Class.getName().replace('.','/') round trip",
                () -> Object.class.getName().replace('.', '/').equals("java/lang/Object"));
    }
}
