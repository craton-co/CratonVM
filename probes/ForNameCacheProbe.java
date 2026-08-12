import java.io.File;
import java.io.InputStream;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;

/**
 * A repeated {@code Class.forName(name, true, loader)} through the SAME loader
 * must return the SAME {@code Class} object. HotSpot answers the second lookup
 * out of the loader's initiated-classes record; CratonVM was observed re-driving
 * a define and failing the second call with
 * {@code IncompatibleClassChangeError: class X already defined by
 * user-defined(N) loader}, surfaced as a {@code ClassFormatError} out of
 * {@code URLClassLoader.findClass}.
 *
 * The discriminating shape, and the reason "the second call did not throw" is a
 * VACUOUS assertion: a VM that answers the second lookup with a DIFFERENT Class
 * object of the same name passes "did not throw" while being just as broken.
 * Every repeat check here asserts {@code ==}.
 *
 * The last group is the OVER-CORRECTION guard: a genuine duplicate
 * {@code defineClass} of one name into one loader must STILL raise a
 * {@code LinkageError}. Without it, "fix" the repeat-lookup bug by deleting the
 * duplicate-define check and everything else here still passes.
 *
 * Usage: {@code ForNameCacheProbe <auxDir>} where {@code auxDir} holds only
 * {@code ForNameCacheProbeTarget.class} and is OFF the application classpath.
 */
public final class ForNameCacheProbe {

    private static final String TARGET = "ForNameCacheProbeTarget";

    private static int failures = 0;

    public static void main(String[] args) throws Exception {
        File auxDir = new File(args[0]);
        URL auxUrl = auxDir.toURI().toURL();

        // ---- Group 0: the target must NOT be visible to the application
        // loader, or every "distinct Class" result below is manufactured.
        try {
            Class.forName(TARGET);
            fail("group0", "target is on the application classpath; auxDir must be off it");
        } catch (ClassNotFoundException expected) {
            pass("group0", "target invisible to app loader");
        }

        // ---- Group 1: forName twice, one user-defined URLClassLoader.
        URLClassLoader l1 = new URLClassLoader("l1", new URL[] { auxUrl }, null);
        Class<?> a1 = forName(TARGET, l1, "group1.first");
        Class<?> a2 = forName(TARGET, l1, "group1.second");
        identical("group1", "forName x2 / user-defined URLClassLoader", a1, a2);
        Class<?> a3 = forName(TARGET, l1, "group1.third");
        identical("group1", "forName x3 / user-defined URLClassLoader", a1, a3);
        works("group1", a1);

        // ---- Group 2: loadClass twice, and mixed with forName, same loader.
        Class<?> b1 = loadClass(l1, TARGET, "group2.loadClass.first");
        Class<?> b2 = loadClass(l1, TARGET, "group2.loadClass.second");
        identical("group2", "loadClass x2 / same loader", b1, b2);
        identical("group2", "loadClass == forName / same loader", a1, b1);

        // ---- Group 3: a SECOND, independent loader over the same URL must get
        // its OWN distinct class, and both copies must still work. This is the
        // half a "just return the cached class globally" fix would break.
        URLClassLoader l2 = new URLClassLoader("l2", new URL[] { auxUrl }, null);
        Class<?> c1 = forName(TARGET, l2, "group3.other.first");
        Class<?> c2 = forName(TARGET, l2, "group3.other.second");
        identical("group3", "forName x2 / second loader", c1, c2);
        distinct("group3", "two loaders yield two classes", a1, c1);
        works("group3", c1);
        works("group3", a1);
        check("group3", "l1 class reports l1 as its loader", a1 != null && a1.getClassLoader() == l1);
        check("group3", "l2 class reports l2 as its loader", c1 != null && c1.getClassLoader() == l2);

        // ---- Group 4: the application loader.
        ClassLoader app = ForNameCacheProbe.class.getClassLoader();
        Class<?> d1 = forName("ForNameCacheProbe", app, "group4.app.first");
        Class<?> d2 = forName("ForNameCacheProbe", app, "group4.app.second");
        identical("group4", "forName x2 / application loader", d1, d2);
        identical("group4", "forName == literal / application loader", d1, ForNameCacheProbe.class);
        Class<?> d3 = Class.forName("ForNameCacheProbe");
        Class<?> d4 = Class.forName("ForNameCacheProbe");
        identical("group4", "forName(String) x2 / application loader", d3, d4);
        identical("group4", "forName(String) == forName(3-arg)", d1, d3);

        // ---- Group 5: the boot loader.
        Class<?> e1 = forName("java.util.zip.CRC32", null, "group5.boot.first");
        Class<?> e2 = forName("java.util.zip.CRC32", null, "group5.boot.second");
        identical("group5", "forName x2 / boot loader", e1, e2);
        Class<?> e3 = forName("java.util.zip.Adler32", app, "group5.viaApp.first");
        Class<?> e4 = forName("java.util.zip.Adler32", app, "group5.viaApp.second");
        identical("group5", "forName x2 / boot class via app loader", e3, e4);

        // ---- Group 6: repeat through a loader that DELEGATES to the app loader
        // rather than to bootstrap. The class is then an INITIATED (not defined)
        // class of the child, which is the record HotSpot's forName consults.
        URLClassLoader l3 = new URLClassLoader("l3", new URL[0], app);
        Class<?> f1 = forName("ForNameCacheProbe", l3, "group6.delegating.first");
        Class<?> f2 = forName("ForNameCacheProbe", l3, "group6.delegating.second");
        identical("group6", "forName x2 / delegating child loader", f1, f2);
        identical("group6", "delegating child returns the parent's class", f1, ForNameCacheProbe.class);

        // ---- Group 7: OVER-CORRECTION GUARD. A genuine duplicate defineClass
        // of the same name into one loader is a LinkageError, still.
        byte[] bytes;
        try (InputStream in = Files.newInputStream(new File(auxDir, TARGET + ".class").toPath())) {
            bytes = in.readAllBytes();
        }
        Definer def = new Definer();
        Class<?> g1;
        try {
            g1 = def.def(TARGET, bytes);
            pass("group7", "first defineClass succeeds");
        } catch (Throwable t) {
            fail("group7", "first defineClass must succeed, got " + t.getClass().getName()
                    + ": " + t.getMessage());
            g1 = null;
        }
        try {
            Class<?> g2 = def.def(TARGET, bytes);
            fail("group7", "second defineClass of the same name must raise LinkageError, returned "
                    + g2 + " (same object as first: " + (g2 == g1) + ")");
        } catch (LinkageError expected) {
            pass("group7", "second defineClass raised " + expected.getClass().getName());
        } catch (Throwable other) {
            fail("group7", "second defineClass raised " + other.getClass().getName()
                    + " (must be a LinkageError): " + other.getMessage());
        }
        // ...and the loader still answers lookups with the FIRST definition.
        if (g1 != null) {
            Class<?> g3 = forName(TARGET, def, "group7.afterDup");
            identical("group7", "lookup after a rejected duplicate define returns the first class",
                    g1, g3);
        }

        System.out.println("PROBE result=" + (failures == 0 ? "OK" : "FAILED(" + failures + ")"));
        if (failures != 0) {
            System.exit(1);
        }
    }

    /** A loader that defines exactly what it is told to, twice on demand. */
    private static final class Definer extends ClassLoader {
        Definer() {
            super("definer", null);
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    private static Class<?> forName(String name, ClassLoader cl, String where) {
        try {
            return Class.forName(name, true, cl);
        } catch (Throwable t) {
            fail(where, "Class.forName threw " + t.getClass().getName() + ": " + t.getMessage());
            return null;
        }
    }

    private static Class<?> loadClass(ClassLoader cl, String name, String where) {
        try {
            return cl.loadClass(name);
        } catch (Throwable t) {
            fail(where, "loadClass threw " + t.getClass().getName() + ": " + t.getMessage());
            return null;
        }
    }

    private static void works(String group, Class<?> c) {
        if (c == null) {
            fail(group, "class is null, cannot invoke tag()");
            return;
        }
        try {
            Method m = c.getMethod("tag");
            Object v = m.invoke(null);
            check(group, "tag() on " + idOf(c) + " returns the expected value",
                    "ForNameCacheProbeTarget-ok".equals(v));
        } catch (Throwable t) {
            fail(group, "tag() threw " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    private static void identical(String group, String what, Class<?> x, Class<?> y) {
        if (x == null || y == null) {
            fail(group, what + ": one side is null (an earlier call threw)");
            return;
        }
        check(group, what + " [" + idOf(x) + " vs " + idOf(y) + "]", x == y);
    }

    private static void distinct(String group, String what, Class<?> x, Class<?> y) {
        if (x == null || y == null) {
            fail(group, what + ": one side is null (an earlier call threw)");
            return;
        }
        check(group, what + " [" + idOf(x) + " vs " + idOf(y) + "]", x != y);
    }

    private static String idOf(Class<?> c) {
        ClassLoader cl = c.getClassLoader();
        return c.getName() + "#" + System.identityHashCode(c) + "@"
                + (cl == null ? "<boot>" : cl.getName() + "#" + System.identityHashCode(cl));
    }

    private static void check(String group, String what, boolean ok) {
        if (ok) {
            pass(group, what);
        } else {
            fail(group, what);
        }
    }

    private static void pass(String group, String what) {
        System.out.println("CK " + group + " PASS " + what);
    }

    private static void fail(String group, String what) {
        failures++;
        System.out.println("CK " + group + " FAIL " + what);
    }
}
