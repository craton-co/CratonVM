/**
 * Lane 7, target 1: the built-in loader hierarchy, one class-init at a time.
 *
 * Lane page trap: "&lt;clinit&gt; failures cascade. The first throw hides the
 * silent ones; run each row independently." Every row below is wrapped, so a
 * row that throws prints its own exception and the run continues to the next.
 *
 * The rows are in dependency order:
 *
 *   1-2  an app-class two-level chain (Mid registers, Leaf registers under it).
 *        This is the SHAPE of SecureClassLoader/BuiltinClassLoader, in a
 *        namespace no native shadows. If 1 or 2 fails, the defect is in
 *        registerAsParallelCapable itself, not in the JDK loader classes.
 *   3    java.security.SecureClassLoader initialises.
 *   4    jdk.internal.loader.BuiltinClassLoader initialises. This is the row the
 *        lane page is about: it threw InternalError under
 *        CRATONVM_ENFORCE_NATIVE_SHADOW=all, which every later consumer sees as
 *        NoClassDefFoundError.
 *   5    ClassLoader.getPlatformClassLoader(), the first ordinary API call that
 *        needs jdk.internal.loader.ClassLoaders, which needs row 4.
 *
 * Row 3 succeeding is NOT evidence the registration happened: the real
 * SecureClassLoader.&lt;clinit&gt; discards registerAsParallelCapable()'s result
 * (invokestatic; pop; return), so it cannot throw. Read row 3 together with
 * L7ParallelCapableProbe row 2, which is the same question asked so that the
 * answer is visible.
 */
public class L7LoaderBootstrapProbe {

    static class Mid extends ClassLoader {
        static final boolean OK = ClassLoader.registerAsParallelCapable();
    }

    static class Leaf extends Mid {
        static final boolean OK = ClassLoader.registerAsParallelCapable();
    }

    /** Run one row; print its own failure rather than aborting the sweep. */
    static void row(String label, Body b) {
        try {
            b.run();
        } catch (Throwable t) {
            System.out.println(label + " THREW " + t.getClass().getName() + ": " + t.getMessage());
            for (Throwable c = t.getCause(); c != null; c = c.getCause()) {
                System.out.println("      caused by " + c.getClass().getName() + ": " + c.getMessage());
            }
        }
    }

    interface Body {
        void run() throws Throwable;
    }

    static void initialised(String label, String binaryName) throws Throwable {
        Class<?> c = Class.forName(binaryName, true, null);
        System.out.println(label + " ok, super=" + c.getSuperclass().getName());
    }

    public static void main(String[] args) {
        row("1 Mid", () -> System.out.println("1 Mid registered under ClassLoader = " + Mid.OK));
        row("2 Leaf", () -> System.out.println("2 Leaf registered under Mid       = " + Leaf.OK));
        row("3 SecureClassLoader", () -> initialised("3 SecureClassLoader init", "java.security.SecureClassLoader"));
        row("4 BuiltinClassLoader", () -> initialised("4 BuiltinClassLoader init", "jdk.internal.loader.BuiltinClassLoader"));
        row("5 platform loader", () -> System.out.println("5 getPlatformClassLoader = "
                + ClassLoader.getPlatformClassLoader().getClass().getName()));
    }
}
