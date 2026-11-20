import java.io.InputStream;
import java.lang.reflect.Proxy;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.Arrays;

/**
 * Which loader DEFINES a class, and what a loader may claim to have defined.
 *
 * <h2>Row 1 — not every image class is boot-loaded</h2>
 *
 * The JDK splits the runtime image's modules between the boot and the platform
 * loader ({@code jdk.internal.module.ModuleLoaderMap}), so
 * {@code java.sql.Connection.class.getClassLoader()} is the PLATFORM loader
 * while {@code java.lang.String}'s is {@code null}. A VM that reads the whole
 * jimage through one class path reports {@code null} for both, and every caller
 * that keys a cache on the defining loader, picks a proxy loader, or decides a
 * delegation parent then reads "the boot loader owns this".
 *
 * <p>{@code java.awt.Color} and {@code java.util.logging.Logger} are the rows
 * that stop a package-name prefix from standing in for the answer:
 * {@code java.desktop} and {@code java.logging} are BOOT modules, so those two
 * are {@code null} on HotSpot even though {@code java.sql} beside them is not.
 * Nothing short of the JDK's own table gets all four right.
 *
 * <h2>Row 2 — a loader may only claim what it DEFINED</h2>
 *
 * {@code getDefinedPackage} is non-delegating. A loader that has defined
 * nothing must answer {@code null} for every name, including the application
 * class path's packages, which it can see through no view of its own. The
 * BEFORE/AFTER pair around {@code defineClass} is the load-bearing half: a VM
 * that answers {@code null} unconditionally passes every negative row here, and
 * a VM that answers from the process class path passes the positive one. Only
 * the pair separates them.
 *
 * <p>The class defined into the isolated loader is this vector's OWN bytes,
 * read back through {@code getResourceAsStream}, so the fixture needs no second
 * source file and no build-layout assumption.
 *
 * <p>Determinism: no timing, no identity hash codes, no exception messages.
 */
public class RLoaderIdentity {
    static int checks;

    static void ck(String what, boolean ok, String detail) {
        checks++;
        if (!ok) {
            throw new AssertionError("RLoaderIdentity: " + what + ": " + detail);
        }
    }

    static ClassLoader loaderOf(String cls) throws Exception {
        return Class.forName(cls).getClassLoader();
    }

    static String name(ClassLoader cl) {
        return cl == null ? "null" : cl.getClass().getName();
    }

    /** Defines from raw bytes and delegates to nothing. */
    static final class Isolated extends ClassLoader {
        Isolated() {
            super(null);
        }

        Class<?> define(String n, byte[] b) {
            return defineClass(n, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        ClassLoader app = RLoaderIdentity.class.getClassLoader();
        ck("the platform loader exists", plat != null, "getPlatformClassLoader() was null");
        ck("the application loader exists", app != null, "this class has no loader");
        ck("app != platform", app != plat, "the two built-ins are the same object");

        // ---- Row 1: BOOT modules answer null ---------------------------------
        for (String boot : new String[] {
                "java.lang.String",            // java.base
                "javax.crypto.Cipher",         // java.base
                "java.util.logging.Logger",    // java.logging
                "javax.naming.Context",        // java.naming
                "java.awt.Color" }) {          // java.desktop
            ck(boot + " is boot-defined", loaderOf(boot) == null, "got " + name(loaderOf(boot)));
        }

        // ---- Row 1: PLATFORM modules answer the platform loader --------------
        for (String platform : new String[] {
                "java.sql.Connection",             // java.sql
                "javax.sql.DataSource",            // java.sql
                "javax.sql.rowset.CachedRowSet",   // java.sql.rowset
                "javax.script.ScriptEngine",       // java.scripting
                "com.sun.net.httpserver.HttpServer" }) { // jdk.httpserver
            ck(platform + " is platform-defined", loaderOf(platform) == plat,
                    "got " + name(loaderOf(platform)));
        }

        // ---- Row 1: the shapes that must NOT move ----------------------------
        ck("int.class has no loader", int.class.getClassLoader() == null,
                "a primitive reported " + name(int.class.getClassLoader()));
        ck("String[] follows its component", String[].class.getClassLoader() == null,
                "got " + name(String[].class.getClassLoader()));
        Class<?> conn = Class.forName("java.sql.Connection");
        Class<?> connArray = java.lang.reflect.Array.newInstance(conn, 0).getClass();
        ck("Connection[] follows its component", connArray.getClassLoader() == plat,
                "got " + name(connArray.getClassLoader()));
        ck("this class is app-defined", RLoaderIdentity.class.getClassLoader() == app, "");
        ck("this class's array is app-defined",
                RLoaderIdentity[].class.getClassLoader() == app,
                "got " + name(RLoaderIdentity[].class.getClassLoader()));

        // ---- Row 1: a module and its classes must not disagree ---------------
        ck("java.sql module reports the platform loader",
                conn.getModule().getClassLoader() == plat,
                "got " + name(conn.getModule().getClassLoader()));
        ck("java.base module reports no loader",
                String.class.getModule().getClassLoader() == null,
                "got " + name(String.class.getModule().getClassLoader()));

        // ---- Row 1 consumers: the answer moved, nothing else may -------------
        ck("a platform class still resolves its own resource",
                conn.getResourceAsStream("/java/sql/Connection.class") != null,
                "getResourceAsStream returned null");
        ck("the platform loader loads its own class",
                plat.loadClass("java.sql.Connection") == conn, "a different Class came back");
        ck("Class.forName through the platform loader agrees",
                Class.forName("java.sql.Connection", false, plat) == conn, "");
        ck("Class.forName through the app loader still agrees",
                Class.forName("java.sql.Connection", false, app) == conn, "");
        String appThroughPlatform = "<none>";
        try {
            plat.loadClass("RLoaderIdentity");
            appThroughPlatform = "<loaded>";
        } catch (Throwable t) {
            appThroughPlatform = t.getClass().getSimpleName();
        }
        ck("the platform loader cannot reach an application class",
                appThroughPlatform.equals("ClassNotFoundException"),
                "got " + appThroughPlatform);

        // A proxy is defined by the loader its interfaces name, and reports it.
        Object proxy = Proxy.newProxyInstance(conn.getClassLoader(),
                new Class<?>[] { conn }, (p, m, ar) -> null);
        ck("the proxy implements the interface", conn.isInstance(proxy), "");
        ck("the proxy reports the platform loader",
                proxy.getClass().getClassLoader() == plat,
                "got " + name(proxy.getClass().getClassLoader()));

        // ---- Row 2: a loader may only claim what it DEFINED -------------------
        Isolated iso = new Isolated();
        for (String absent : new String[] {
                "java.lang", "java.util", "java.sql", "no.such.package.here" }) {
            ck("an isolated loader does not define " + absent,
                    iso.getDefinedPackage(absent) == null,
                    "got " + iso.getDefinedPackage(absent));
        }
        ck("an isolated loader does not define the unnamed package either",
                iso.getDefinedPackage("") == null,
                "got " + iso.getDefinedPackage(""));

        byte[] own;
        try (InputStream in = RLoaderIdentity.class.getResourceAsStream("/RLoaderIdentity.class")) {
            ck("this vector can read its own class file", in != null, "resource was null");
            own = in.readAllBytes();
        }
        ck("its class file is non-empty", own.length > 0, "read " + own.length + " bytes");

        Class<?> twin = iso.define("RLoaderIdentity", own);
        ck("the twin is a different Class", twin != RLoaderIdentity.class, "the same object");
        ck("the twin names the isolated loader", twin.getClassLoader() == iso,
                "got " + name(twin.getClassLoader()));
        ck("the isolated loader NOW defines the unnamed package",
                iso.getDefinedPackage("") != null,
                "still null after defineClass");
        ck("and names it the empty string", "".equals(iso.getDefinedPackage("").getName()),
                "got [" + iso.getDefinedPackage("").getName() + "]");
        ck("the twin's own package agrees",
                twin.getPackage() != null && "".equals(twin.getPackage().getName()),
                "got " + twin.getPackage());
        ck("defining did not make it claim anything else",
                iso.getDefinedPackage("java.sql") == null && iso.getDefinedPackage("java.lang") == null,
                "an unrelated package became visible");
        Package[] isoDefined = iso.getDefinedPackages();
        ck("getDefinedPackages is a Package[]",
                isoDefined.getClass().getComponentType() == Package.class,
                "got " + isoDefined.getClass().getComponentType());
        ck("getDefinedPackages contains the unnamed package",
                Arrays.stream(isoDefined).anyMatch(p -> p != null && "".equals(p.getName())),
                "the plural method disagrees with the singular one");

        // A positively-EMPTY URL set stays empty — the row the URLClassLoader
        // half of this defect was pinned on.
        ClassLoader empty = new URLClassLoader("empty", new URL[0], null);
        ck("an empty URLClassLoader defines nothing",
                empty.getDefinedPackage("java.lang") == null
                        && empty.getDefinedPackage("no.such.package.here") == null,
                "an empty loader claimed a package");

        System.out.println("PASS RLoaderIdentity (" + checks + " checks)");
    }
}
