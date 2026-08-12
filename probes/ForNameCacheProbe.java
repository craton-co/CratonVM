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
 * Group 7 is the OVER-CORRECTION guard: a genuine duplicate
 * {@code defineClass} of one name into one loader must STILL raise a
 * {@code LinkageError}. Without it, "fix" the repeat-lookup bug by deleting the
 * duplicate-define check and everything else here still passes.
 *
 * <p>W7-87 added the OTHER half of the same predicate asymmetry, and it points
 * the opposite way: a bare {@code URLClassLoader} must NOT see an
 * application-namespace class it neither defined nor was ever asked to load.
 * Group 6 used to pin only the delegating {@code forName} answer (which is
 * HotSpot-correct and unchanged); it now also pins what {@code findLoadedClass}
 * reports there, and group 8 drives the isolating shape —
 * {@code new URLClassLoader(urls, null)} — end to end. Both were measured
 * against HotSpot 25 before being written down.
 *
 * Usage: {@code ForNameCacheProbe <auxDir>} where {@code auxDir} holds only
 * {@code ForNameCacheProbeTarget.class} and is OFF the application classpath.
 *
 * <p>REQUIRES {@code --add-opens java.base/java.lang=ALL-UNNAMED}.
 * {@code ClassLoader.findLoadedClass} is {@code protected}, and the whole point
 * of groups 6 and 8 is that a bare {@code java.net.URLClassLoader} behaves
 * differently from a SUBCLASS — so exposing it through a subclass would test the
 * arm that was never broken. Reflection keeps the receiver bare. If the flag is
 * missing the probe FAILS rather than skipping: a silent skip is a vacuous
 * green, and this is exactly the check that has to be able to fail.
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
        // W7-87. BEFORE any load: `findLoadedClass` must report NOTHING. This
        // loader has defined nothing and has never been asked for anything, so
        // on HotSpot 25 it is neither a defining nor an initiating loader of any
        // name — MEASURED, both for an already-loaded application class and for
        // an already-loaded bootstrap class. CratonVM answered both of them with
        // the application/bootstrap class through the built-in branch's global
        // fallback, because `is_builtin_loader_class` lists
        // `java/net/URLClassLoader`. This group PINNED that wrong answer while
        // W7-82 settled the other half; these two lines are the pin, flipped to
        // HotSpot's value. See W7-87-urlclassloader-namespace-asymmetry.md.
        flcIsNull("group6", l3, "ForNameCacheProbe", "an app class it never initiated");
        flcIsNull("group6", l3, "java.lang.String", "a boot class it never initiated");
        Class<?> f1 = forName("ForNameCacheProbe", l3, "group6.delegating.first");
        Class<?> f2 = forName("ForNameCacheProbe", l3, "group6.delegating.second");
        identical("group6", "forName x2 / delegating child loader", f1, f2);
        identical("group6", "delegating child returns the parent's class", f1, ForNameCacheProbe.class);
        // The OBVIOUS over-correction: hiding the app class from the cache probe
        // must not stop parent-first delegation from finding it. `loadClass` is
        // asserted separately from `forName` because they reach the VM by
        // different roads.
        Class<?> f3 = loadClass(l3, "ForNameCacheProbe", "group6.delegating.loadClass");
        identical("group6", "loadClass through the delegating child agrees",
                f3, ForNameCacheProbe.class);

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

        // ---- Group 8: W7-87. THE ISOLATING LOADER.
        // `new URLClassLoader(urls, null)` — a private URL search path and a
        // BOOTSTRAP parent — is the standard idiom for building a loader that
        // deliberately cannot see the application classpath (Spring Boot's
        // ModifiedClassPathClassLoader, javax.tools test harnesses, plugin
        // containers). On HotSpot 25 it raises ClassNotFoundException for an
        // application class, whatever the application loader has already loaded.
        // CratonVM answered with the application loader's class, so the
        // isolation the loader was constructed for did not exist.
        //
        // The name asked for below is `ForNameCacheProbe` itself, which the
        // application loader is certainly holding — that is the point. Asking
        // for a never-loaded name would pass on a leaky VM too, which is why the
        // "app loader has it" side is asserted first.
        URLClassLoader iso = new URLClassLoader("iso", new URL[] { auxUrl }, null);
        check("group8", "the app loader really does hold this class",
                ForNameCacheProbe.class.getClassLoader() == app);
        flcIsNull("group8", iso, "ForNameCacheProbe", "an app class it never initiated");
        throwsCnfe("group8", "loadClass of an app class through an isolating loader",
                () -> iso.loadClass("ForNameCacheProbe"));
        throwsCnfe("group8", "forName of an app class through an isolating loader",
                () -> Class.forName("ForNameCacheProbe", false, iso));

        // ANTI-VACUITY for the three above: the loader is isolated, not broken.
        // Bootstrap delegation is NOT narrowed, and its own URL path still works.
        Class<?> h1 = loadClass(iso, "java.util.zip.CRC32", "group8.bootDelegation");
        check("group8", "an isolating loader still delegates to bootstrap",
                h1 != null && h1.getClassLoader() == null);
        Class<?> h2 = forName(TARGET, iso, "group8.ownUrlPath.first");
        Class<?> h3 = forName(TARGET, iso, "group8.ownUrlPath.second");
        identical("group8", "an isolating loader still defines from its OWN urls", h2, h3);
        check("group8", "...and owns the result", h2 != null && h2.getClassLoader() == iso);
        // W7-82's half, restated as a regression guard against the OBVIOUS
        // over-correction: narrowing the global fallback must not re-hide the
        // loader's OWN class from it.
        flcIs("group8", iso, TARGET, h2, "its own defined class");

        // The DISCRIMINATOR, now inverted into an invariant. Before W7-87 a
        // `URLClassLoader` SUBCLASS answered all four of these correctly and a
        // BARE instance did not — one line of `extends` decided it. They must
        // now agree. (A test written against the subclass alone cannot fail
        // here, which is precisely how the bare case survived six weeks.)
        URLClassLoader isoSub = new SubLoader("isoSub", new URL[] { auxUrl }, null);
        flcIsNull("group8", isoSub, "ForNameCacheProbe", "subclass control");
        throwsCnfe("group8", "loadClass of an app class through an isolating SUBCLASS",
                () -> isoSub.loadClass("ForNameCacheProbe"));
        Class<?> h4 = forName(TARGET, isoSub, "group8.subclass.ownUrlPath");
        check("group8", "the subclass defines its own copy too",
                h4 != null && h4.getClassLoader() == isoSub);
        distinct("group8", "bare and subclass isolating loaders stay distinct", h2, h4);

        System.out.println("PROBE result=" + (failures == 0 ? "OK" : "FAILED(" + failures + ")"));
        if (failures != 0) {
            System.exit(1);
        }
    }

    /**
     * A plain {@code URLClassLoader} SUBCLASS. It adds nothing — the single
     * {@code extends} is the whole experiment (W7-82's discriminator, W7-87's
     * control).
     */
    private static final class SubLoader extends URLClassLoader {
        SubLoader(String name, URL[] urls, ClassLoader parent) {
            super(name, urls, parent);
        }
    }

    @FunctionalInterface
    private interface Thrower {
        Object run() throws Exception;
    }

    /**
     * {@code ClassLoader.findLoadedClass} on an arbitrary receiver. Resolved
     * reflectively and ONCE; a failure here is reported as a failed check on
     * every call site rather than silently skipped, because "the probe could not
     * ask the question" and "the VM gave the right answer" must never look alike.
     */
    private static Method findLoadedClass;
    private static String findLoadedClassError;

    static {
        try {
            findLoadedClass = ClassLoader.class.getDeclaredMethod("findLoadedClass", String.class);
            findLoadedClass.setAccessible(true);
        } catch (Throwable t) {
            findLoadedClass = null;
            findLoadedClassError = t.getClass().getName() + ": " + t.getMessage()
                    + " (rerun with --add-opens java.base/java.lang=ALL-UNNAMED)";
        }
    }

    private static Object flc(String group, ClassLoader cl, String name) {
        if (findLoadedClass == null) {
            fail(group, "findLoadedClass is not reachable: " + findLoadedClassError);
            return Boolean.FALSE; // a value no assertion below can accept
        }
        try {
            return findLoadedClass.invoke(cl, name);
        } catch (Throwable t) {
            fail(group, "findLoadedClass(" + name + ") threw " + t);
            return Boolean.FALSE;
        }
    }

    /** HotSpot: {@code null} unless this loader defined or initiated {@code name}. */
    private static void flcIsNull(String group, ClassLoader cl, String name, String what) {
        Object r = flc(group, cl, name);
        check(group, cl.getName() + ".findLoadedClass(" + name + ") is null -- " + what
                + (r instanceof Class ? " [got " + idOf((Class<?>) r) + "]" : ""), r == null);
    }

    private static void flcIs(String group, ClassLoader cl, String name, Class<?> expected,
            String what) {
        Object r = flc(group, cl, name);
        check(group, cl.getName() + ".findLoadedClass(" + name + ") == " + what,
                expected != null && r == expected);
    }

    private static void throwsCnfe(String group, String what, Thrower t) {
        Object got;
        try {
            got = t.run();
        } catch (ClassNotFoundException expected) {
            pass(group, what + " raised ClassNotFoundException");
            return;
        } catch (Throwable other) {
            fail(group, what + " raised " + other.getClass().getName()
                    + " (must be ClassNotFoundException): " + other.getMessage());
            return;
        }
        fail(group, what + " must raise ClassNotFoundException, returned "
                + (got instanceof Class ? idOf((Class<?>) got) : String.valueOf(got)));
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
