import java.lang.reflect.Method;

/**
 * Paired probe for the `…ToAllUnnamed` module edges, diffed against the host JDK.
 *
 * {@code AddOpensFlagProbe} covers the launcher's {@code --add-opens} path. This
 * one covers the *runtime* path with the same shape: {@code JavaLangAccess
 * .addExportsToAllUnnamed} / {@code addOpensToAllUnnamed}, which is what
 * agents, ByteBuddy and Mockito's {@code InstrumentationMemberAccessor} reach
 * through, and which CratonVM implemented by sharing the
 * {@code addExportsToAll0} native — recording an *unqualified* edge, i.e. one
 * that grants every module in the process rather than the unnamed ones.
 *
 * The distinguishing observation is not "did the access work" — an over-grant
 * and a correct grant both let the unnamed module through. It is
 * {@code Module.isExported(pkg)} / {@code isOpen(pkg)}, the unqualified
 * queries, which stay **false** on HotSpot after a `…ToAllUnnamed` call and
 * answered **true** on CratonVM. Every line prints a value, never a verdict.
 *
 * Run with:
 *   --add-exports java.base/jdk.internal.access=ALL-UNNAMED
 * That flag names a different package from the ones under test, so it does not
 * contaminate the measurement.
 *
 * A run that cannot reach {@code SharedSecrets} prints {@code jla=<throwable>}
 * and nothing else; that is a reachability failure, not a verdict about the
 * edges.
 */
public class ModuleToAllUnnamedProbe {

    /** Packages java.base neither exports nor opens to the classpath. */
    private static final String EXPORT_SUBJECT = "jdk.internal.misc";
    private static final String OPEN_SUBJECT = "jdk.internal.loader";

    static String outcome(ThrowingRunnable r) {
        try {
            r.run();
            return "OK";
        } catch (Throwable t) {
            Throwable c = (t.getCause() != null) ? t.getCause() : t;
            return c.getClass().getName();
        }
    }

    interface ThrowingRunnable {
        void run() throws Throwable;
    }

    public static void main(String[] args) {
        Module base = Object.class.getModule();
        Module self = ModuleToAllUnnamedProbe.class.getModule();

        System.out.println("self.named=" + self.isNamed());
        System.out.println("export before unqualified=" + base.isExported(EXPORT_SUBJECT));
        System.out.println("export before toSelf=" + base.isExported(EXPORT_SUBJECT, self));
        System.out.println("open before unqualified=" + base.isOpen(OPEN_SUBJECT));
        System.out.println("open before toSelf=" + base.isOpen(OPEN_SUBJECT, self));

        final Object jla;
        final Class<?> jlaType;
        try {
            Class<?> ss = Class.forName("jdk.internal.access.SharedSecrets");
            jlaType = Class.forName("jdk.internal.access.JavaLangAccess");
            Method get = ss.getMethod("getJavaLangAccess");
            jla = get.invoke(null);
        } catch (Throwable t) {
            System.out.println("jla=" + t.getClass().getName() + ": " + t.getMessage());
            return;
        }
        System.out.println("jla=" + (jla != null ? "present" : "null"));

        System.out.println("addExportsToAllUnnamed=" + outcome(() -> {
            Method m = jlaType.getMethod("addExportsToAllUnnamed", Module.class, String.class);
            m.invoke(jla, base, EXPORT_SUBJECT);
        }));
        System.out.println("addOpensToAllUnnamed=" + outcome(() -> {
            Method m = jlaType.getMethod("addOpensToAllUnnamed", Module.class, String.class);
            m.invoke(jla, base, OPEN_SUBJECT);
        }));

        // The whole point. `toSelf` true says the grant landed; `unqualified`
        // false says it landed as a QUALIFIED edge, which is the half CratonVM
        // used to get wrong.
        System.out.println("export after unqualified=" + base.isExported(EXPORT_SUBJECT));
        System.out.println("export after toSelf=" + base.isExported(EXPORT_SUBJECT, self));
        System.out.println("open after unqualified=" + base.isOpen(OPEN_SUBJECT));
        System.out.println("open after toSelf=" + base.isOpen(OPEN_SUBJECT, self));

        // Control: a sibling package nobody granted must not move.
        System.out.println("control open java.util toSelf=" + base.isOpen("java.util", self));
        System.out.println("control export jdk.internal.vm unqualified="
                + base.isExported("jdk.internal.vm"));
    }
}
