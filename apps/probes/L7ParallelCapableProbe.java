import java.net.URLClassLoader;
import java.security.SecureClassLoader;

/**
 * Lane 7, target 1: which link of the parallel-capable registration chain is
 * broken under --jdk-only.
 *
 * jdk/internal/loader/BuiltinClassLoader.&lt;clinit&gt; is, in JDK 25 bytecode,
 *
 *     if (!ClassLoader.registerAsParallelCapable())
 *         throw new InternalError("Unable to register as parallel capable");
 *
 * and ParallelLoaders.register(c) answers loaderTypes.contains(c.getSuperclass()).
 * So BuiltinClassLoader can only register once java.security.SecureClassLoader
 * has, and SecureClassLoader only once java.lang.ClassLoader is seeded (which
 * ParallelLoaders.&lt;clinit&gt; does).
 *
 * This probe reads each link of that chain WITHOUT reflection, so it needs no
 * --add-opens: a subclass of X that calls registerAsParallelCapable() returns
 * true exactly when X is already in loaderTypes. Three rows, three links.
 *
 * MEASURED 2026-09-10 (JDK 25.0.4+7, azure vm1):
 *
 *                                              HotSpot  armed  unarmed
 *   Direct       extends ClassLoader            true     true   true
 *   UnderSecure  extends SecureClassLoader      true     FALSE  true
 *   UnderUrl     extends URLClassLoader         true     FALSE  true
 *
 * where "armed" is CRATONVM_ENFORCE_NATIVE_SHADOW=all. Row 1 proves the real
 * registerAsParallelCapable bytecode and Reflection.getCallerClass() both work
 * here; rows 2 and 3 are the defect.
 *
 * Rows 2 and 3 are not independent: URLClassLoader extends SecureClassLoader,
 * so row 3 cannot pass while row 2 fails. It is printed anyway because a run in
 * which row 3 passes and row 2 fails would mean the chain is not the chain this
 * probe assumes.
 */
public class L7ParallelCapableProbe {

    /** Link 1: is java.lang.ClassLoader itself in loaderTypes? */
    static class Direct extends ClassLoader {
        static final boolean OK = ClassLoader.registerAsParallelCapable();
    }

    /** Link 2: is java.security.SecureClassLoader in loaderTypes? */
    static class UnderSecure extends SecureClassLoader {
        static final boolean OK = ClassLoader.registerAsParallelCapable();
    }

    /** Link 3: is java.net.URLClassLoader in loaderTypes? */
    static class UnderUrl extends URLClassLoader {
        static final boolean OK = ClassLoader.registerAsParallelCapable();

        UnderUrl() {
            super(new java.net.URL[0]);
        }
    }

    public static void main(String[] args) {
        System.out.println("1 ClassLoader registered       = " + Direct.OK);
        System.out.println("2 SecureClassLoader registered = " + UnderSecure.OK);
        System.out.println("3 URLClassLoader registered    = " + UnderUrl.OK);
    }
}
