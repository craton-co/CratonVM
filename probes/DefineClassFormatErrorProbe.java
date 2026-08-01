import java.net.URL;
import java.net.URLClassLoader;

/**
 * Repro for the RestartClassLoaderTests.getUpdatedClass residual: a
 * ClassFormatError raised inside the JDK-internal `defineClass1` native must be
 * delivered as a normal, catchable java.lang.ClassFormatError, not unwind to
 * the top-level VM run().
 */
public class DefineClassFormatErrorProbe {

    static int failures = 0;

    static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "  OK   " : "  FAIL ") + what + "   " + detail);
        if (!ok) {
            failures++;
        }
    }

    /** Mirrors RestartClassLoader: findClass hands raw bytes to defineClass. */
    static final class BytesLoader extends URLClassLoader {

        private final byte[] bytes;

        BytesLoader(byte[] bytes) {
            super(new URL[0], BytesLoader.class.getClassLoader());
            this.bytes = bytes;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            return defineClass(name, this.bytes, 0, this.bytes.length);
        }
    }

    public static void main(String[] args) {
        System.out.println("[1] direct defineClass(byte[10]) -> ClassFormatError");
        BytesLoader loader = new BytesLoader(new byte[10]);
        try {
            loader.loadClass("probe.pkg.Sample");
            check("direct defineClass", false, "no throwable at all");
        }
        catch (Throwable ex) {
            check("direct defineClass", ex instanceof ClassFormatError,
                    ex.getClass().getName() + ": " + ex.getMessage());
        }

        System.out.println("[2] Class.forName(name, false, loader) -> ClassFormatError");
        BytesLoader loader2 = new BytesLoader(new byte[10]);
        try {
            Class.forName("probe.pkg.Sample2", false, loader2);
            check("Class.forName", false, "no throwable at all");
        }
        catch (Throwable ex) {
            check("Class.forName", ex instanceof ClassFormatError,
                    ex.getClass().getName() + ": " + ex.getMessage());
        }

        System.out.println("[3] Class.forName(name, true, loader) -> ClassFormatError");
        BytesLoader loader3 = new BytesLoader(new byte[10]);
        try {
            Class.forName("probe.pkg.Sample3", true, loader3);
            check("Class.forName initialize=true", false, "no throwable at all");
        }
        catch (Throwable ex) {
            check("Class.forName initialize=true", ex instanceof ClassFormatError,
                    ex.getClass().getName() + ": " + ex.getMessage());
        }

        System.out.println("[4] non-empty but truncated bytes");
        byte[] truncated = { (byte) 0xCA, (byte) 0xFE, (byte) 0xBA, (byte) 0xBE, 0, 0, 0 };
        try {
            new BytesLoader(truncated).loadClass("probe.pkg.Sample4");
            check("truncated classfile", false, "no throwable at all");
        }
        catch (Throwable ex) {
            check("truncated classfile", ex instanceof ClassFormatError,
                    ex.getClass().getName() + ": " + ex.getMessage());
        }

        System.out.println();
        System.out.println("VM still alive after all four attempts");
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        System.exit(failures == 0 ? 0 : 1);
    }
}
