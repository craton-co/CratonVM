import java.io.*;
import java.lang.reflect.*;
import java.net.URL;
import java.util.*;

/** L5, part 3: the `java.lang.ClassLoader` (29) triples the
 *  `--jdk-only-report` marks `outcome=native-won`, and the `java/lang/System$1`
 *  (29) surface reached INDIRECTLY through them.
 *
 *  See `HANDOFF-20260828-L5-reflection.md`. Parts 1 and 2 were
 *  `ClassShadowSweep` (261 rows, 9 defects) and `FieldMethodShadowSweep`
 *  (132 rows, 1 defect — the accessor surface is solid).
 *
 *  `System$1` is `JavaLangAccess`. It is not callable from Java at all, so
 *  every row here reaches it sideways: `Module.addExports`/`addOpens`/`addReads`
 *  /`addUses`, `ClassLoader.defineClass`, `Class.getEnumConstantsShared`,
 *  `Throwable.setCause` and the UTF-8 no-repl string paths all route through it.
 *  A defect in `System$1` shows up as a wrong answer from one of those.
 *
 *  DETERMINISM: a classloader's identity, a URL's absolute form and a package
 *  array's order are all VM- or host-chosen, so this prints shapes, names,
 *  counts and membership — never an identity, an address or an order. The one
 *  resource looked up on the class path is this probe's own class file, which
 *  must exist by construction.
 */
public class ClassLoaderShadowSweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    static final ClassLoader APP = ClassLoaderShadowSweep.class.getClassLoader();

    // ---- the loader hierarchy -------------------------------------------
    static void hierarchy() {
        p("app loader non-null", APP != null);
        p("app loader name", APP.getName());
        p("systemClassLoader is app", ClassLoader.getSystemClassLoader() == APP);
        ClassLoader plat = APP.getParent();
        p("parent non-null", plat != null);
        p("parent name", plat == null ? "null" : plat.getName());
        p("platform is getPlatformClassLoader", plat == ClassLoader.getPlatformClassLoader());
        p("platform parent is boot(null)", plat != null && plat.getParent() == null);
        p("boot loader is null for String", String.class.getClassLoader());
        p("boot loader is null for Object", Object.class.getClassLoader());
        p("int.class loader is null", int.class.getClassLoader());
        p("array of app class loader is app",
          ClassLoaderShadowSweep[].class.getClassLoader() == APP);
        p("array of String loader is null", String[].class.getClassLoader());
        p("getUnnamedModule non-null", APP.getUnnamedModule() != null);
        p("getUnnamedModule isNamed", APP.getUnnamedModule().isNamed());
        p("getUnnamedModule name", APP.getUnnamedModule().getName());
        p("unnamed module is stable", APP.getUnnamedModule() == APP.getUnnamedModule());
        p("platform unnamed differs from app",
          plat != null && plat.getUnnamedModule() != APP.getUnnamedModule());
    }

    // ---- loadClass / findLoadedClass ------------------------------------
    static void loading() throws Exception {
        p("loadClass String", APP.loadClass("java.lang.String").getName());
        p("loadClass self", APP.loadClass(ClassLoaderShadowSweep.class.getName()).getSimpleName());
        // An ARRAY name is resolvable by `Class.forName` and NOT by
        // `ClassLoader.loadClass` -- the loader delegates to a binary-name
        // lookup that has no array syntax. My first cut asserted the value and
        // died against the ORACLE, which is the right order: correct the probe's
        // expectation before judging any VM.
        t("loadClass array form refused", () -> APP.loadClass("[I"));
        p("forName array form works", Class.forName("[I").getName());
        // The 2-arg `loadClass` and `findLoadedClass` are PROTECTED. They are
        // reached from a SUBCLASS, not reflectively: `setAccessible` on a
        // protected member of `java.lang.ClassLoader` is refused from the
        // unnamed module (`InaccessibleObjectException`) -- on HotSpot too, so
        // the first cut of this probe failed at row 19 against the ORACLE. A
        // subclass is also how a framework actually reaches them.
        Definer d0 = new Definer();
        p("loadClass 2-arg resolve", d0.load2("java.util.ArrayList", true).getName());
        t("loadClass missing", () -> APP.loadClass("no.such.Klass"));
        t("loadClass empty", () -> APP.loadClass(""));
        t("loadClass null", () -> APP.loadClass(null));
        // The SLASH form is not a binary name and must not resolve.
        t("loadClass slash form", () -> APP.loadClass("java/lang/String"));
        // A primitive is not loadable by name.
        t("loadClass int", () -> APP.loadClass("int"));

        // findLoadedClass, from the subclass. It answers only for classes THIS
        // loader defined or was recorded against, so a fresh Definer that has
        // just loaded ArrayList through delegation still answers null for it --
        // the distinction between "loaded by me" and "visible to me".
        p("findLoadedClass never seen", d0.findLoaded("no.such.Klass"));
        p("findLoadedClass not defined here", d0.findLoaded("java.util.ArrayList"));
        p("findLoadedClass null", thrownOf(() -> d0.findLoaded(null)));

        // The BOOT loader (null) can find String and cannot find an app class.
        p("forName boot finds String",
          Class.forName("java.lang.String", false, null).getName());
        t("forName boot misses app class",
          () -> Class.forName(ClassLoaderShadowSweep.class.getName(), false, null));
    }
    static String thrownOf(ThrowingRun r) {
        try { r.run(); return "no-throw"; }
        catch (Throwable e) {
            Throwable c = (e instanceof InvocationTargetException && e.getCause() != null)
                ? e.getCause() : e;
            return "THREW " + c.getClass().getName();
        }
    }

    // ---- resources -------------------------------------------------------
    static void resources() throws Exception {
        // A ClassLoader resource name is ALWAYS absolute and must NOT start
        // with '/', which is the opposite of Class.getResource. That asymmetry
        // is the single most-confused rule in this API.
        String own = "ClassLoaderShadowSweep.class";
        p("loader getResource own class present", APP.getResource(own) != null);
        p("loader getResource leading slash is null", APP.getResource("/" + own));
        p("loader getResourceAsStream own present", APP.getResourceAsStream(own) != null);
        p("loader getResource absent", APP.getResource("no/such/resource.txt"));
        p("loader getResourceAsStream absent", APP.getResourceAsStream("no/such/resource.txt"));
        p("loader getResource empty", APP.getResource(""));
        t("loader getResource null", () -> APP.getResource(null));
        t("loader getResourceAsStream null", () -> APP.getResourceAsStream(null));

        p("getSystemResource own present", ClassLoader.getSystemResource(own) != null);
        p("getSystemResource absent", ClassLoader.getSystemResource("no/such/x"));
        p("getSystemResourceAsStream absent", ClassLoader.getSystemResourceAsStream("no/such/x"));
        t("getSystemResource null", () -> ClassLoader.getSystemResource(null));

        Enumeration<URL> e = APP.getResources(own);
        p("getResources own hasMoreElements", e.hasMoreElements());
        p("getResources absent hasMoreElements", APP.getResources("no/such/x").hasMoreElements());
        t("getResources null", () -> APP.getResources(null));
        p("getSystemResources absent hasMore",
          ClassLoader.getSystemResources("no/such/x").hasMoreElements());
        // A boot class file must be reachable from SOMEWHERE, which is a
        // shape assertion rather than a path assertion.
        p("boot class file reachable",
          Object.class.getResource("/java/lang/Object.class") != null
          || ClassLoader.getSystemResource("java/lang/Object.class") != null);
    }

    // ---- packages, assertions, parallel-capable -------------------------
    static void packagesAndFlags() {
        p("getDefinedPackages non-null", APP.getDefinedPackages() != null);
        p("getDefinedPackage absent", APP.getDefinedPackage("no.such.pkg"));
        t("getDefinedPackage null", () -> APP.getDefinedPackage(null));
        // Loading this class defines its package (the default package here has
        // the empty name), so the array is at least reachable and countable.
        p("getDefinedPackages is an array",
          APP.getDefinedPackages().getClass().getName());
        p("String's package via Class", String.class.getPackage() != null);
        p("String's package name", String.class.getPackageName());
        p("primitive package name", int.class.getPackageName());

        t("setDefaultAssertionStatus", () -> APP.setDefaultAssertionStatus(false));
        t("setPackageAssertionStatus", () -> APP.setPackageAssertionStatus("x.y", true));
        t("setClassAssertionStatus", () -> APP.setClassAssertionStatus("x.Y", true));
        t("clearAssertionStatus", () -> APP.clearAssertionStatus());
    }

    // ---- System$1 / JavaLangAccess, reached sideways ---------------------
    static void javaLangAccess() throws Exception {
        Module base = String.class.getModule();
        Module mine = ClassLoaderShadowSweep.class.getModule();

        // addExports / addOpens / addReads / addUses on the UNNAMED module are
        // documented no-ops that return `this` -- they route through
        // JavaLangAccess and are the cheapest way to reach it.
        p("unnamed addExports returns this", mine.addExports("x.y", base) == mine);
        p("unnamed addOpens returns this", mine.addOpens("x.y", base) == mine);
        p("unnamed addReads returns this", mine.addReads(base) == mine);
        p("unnamed addUses returns this", mine.addUses(Runnable.class) == mine);
        t("addExports null package", () -> mine.addExports(null, base));
        t("addExports null module", () -> mine.addExports("x.y", null));
        t("addReads null", () -> mine.addReads(null));
        t("addUses null", () -> mine.addUses(null));

        // getEnumConstantsShared is JavaLangAccess's; Class.getEnumConstants
        // is the public door onto it and must COPY, so two calls differ.
        p("enum constants length", Colour.class.getEnumConstants().length);
        p("enum constants are copied",
          Colour.class.getEnumConstants() == Colour.class.getEnumConstants());
        p("enum constants first", Colour.class.getEnumConstants()[0]);
        p("enum constants on non-enum", String.class.getEnumConstants());

        // Throwable.setCause goes through JavaLangAccess for the
        // already-initialised case.
        Throwable a = new Throwable("a");
        p("initCause returns this", a.initCause(new Throwable("b")) == a);
        p("getCause after initCause", a.getCause().getMessage());
        t("initCause twice", () -> a.initCause(new Throwable("c")));
        Throwable self = new Throwable("s");
        t("initCause with self", () -> self.initCause(self));
        p("cause null by default", new Throwable("n").getCause());

        // The UTF-8 no-repl paths: an UNPAIRED surrogate must be refused
        // rather than replaced, which is the whole point of "NoRepl".
        p("String getBytes UTF_8 ok",
          new String("ok".getBytes(java.nio.charset.StandardCharsets.UTF_8),
                     java.nio.charset.StandardCharsets.UTF_8));
        String lone = "a\uD800b";
        byte[] replaced = lone.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        p("lone surrogate replaced length", replaced.length);
        p("lone surrogate becomes replacement char",
          new String(replaced, java.nio.charset.StandardCharsets.UTF_8).contains("�"));
    }
    enum Colour { RED, GREEN }

    // ---- defineClass, the one that writes ------------------------------
    static class Definer extends ClassLoader {
        Definer() { super(APP); }
        Class<?> def(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
        Class<?> defNoName(byte[] b) { return defineClass(b, 0, b.length); }
        Class<?> load2(String n, boolean resolve) throws ClassNotFoundException {
            return loadClass(n, resolve);
        }
        Class<?> findLoaded(String n) { return findLoadedClass(n); }
    }
    static byte[] ownBytes() throws IOException {
        try (InputStream in = APP.getResourceAsStream("ClassLoaderShadowSweep.class")) {
            if (in == null) return null;
            ByteArrayOutputStream o = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            for (int n; (n = in.read(buf)) > 0; ) o.write(buf, 0, n);
            return o.toByteArray();
        }
    }
    static void defineClass() throws Exception {
        byte[] good = ownBytes();
        p("own bytes readable", good != null);
        if (good == null) return;
        // A class file from the future is UnsupportedClassVersionError; bad
        // magic and truncation are ClassFormatError. Same three the Phase 3
        // probe asks of the other defineClass door -- asked here because this
        // is a DIFFERENT entry point that must agree.
        byte[] future = good.clone(); future[6] = 0; future[7] = (byte) 0xFF;
        p("future version", thrownOf(() -> new Definer().defNoName(future)));
        byte[] magic = good.clone(); magic[0] = 0;
        p("bad magic", thrownOf(() -> new Definer().defNoName(magic)));
        p("truncated", thrownOf(() -> new Definer().defNoName(Arrays.copyOf(good, 20))));
        p("empty", thrownOf(() -> new Definer().defNoName(new byte[0])));
        p("null bytes", thrownOf(() -> new Definer().defNoName(null)));
        // A name that disagrees with the class file is NoClassDefFoundError.
        p("wrong name", thrownOf(() -> new Definer().def("com.example.Wrong", good)));
        // Defining the SAME name twice in one loader is LinkageError.
        Definer d = new Definer();
        p("define once", thrownOf(() -> d.def(null, good)));
        p("define twice same loader", thrownOf(() -> d.def(null, good)));
    }

    public static void main(String[] a) throws Exception {
        hierarchy();
        loading();
        resources();
        packagesAndFlags();
        javaLangAccess();
        defineClass();
        System.out.println("DONE ClassLoaderShadowSweep");
    }
}
