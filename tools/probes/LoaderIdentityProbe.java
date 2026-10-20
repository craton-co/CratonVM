import java.io.InputStream;
import java.lang.reflect.Method;

/**
 * The two rows of the loader-identity model the getDefinedPackage fix left:
 * which loader DEFINES an image class, and what a custom loader with no
 * recorded URL set may claim.
 *
 * <p>Value rows, printed for a cross-VM diff. The consumer rows below the two
 * headline questions are the ones that say whether moving an image class's
 * reported loader BREAKS anything — a `getResource` that used to go through the
 * boot path, a `Class.forName` through the loader that now owns the class.
 */
public class LoaderIdentityProbe {
    static String s(Object o) {
        if (o == null) {
            return "null";
        }
        if (o instanceof ClassLoader) {
            String n = o.getClass().getName();
            return n.substring(n.lastIndexOf('.') + 1);
        }
        return String.valueOf(o);
    }

    static void row(String what, Object got) {
        System.out.println("ROW " + what + " = " + got);
    }

    static void loaderOf(String cls) {
        try {
            row(cls + ".getClassLoader", s(Class.forName(cls).getClassLoader()));
        } catch (Throwable t) {
            row(cls + ".getClassLoader", "THREW " + t.getClass().getSimpleName());
        }
    }

    /** A loader that delegates to nothing and defines from raw bytes. */
    static final class Isolated extends ClassLoader {
        Isolated() {
            super(null);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] bytesOf(String resource) throws Exception {
        try (InputStream in = LoaderIdentityProbe.class.getResourceAsStream(resource)) {
            if (in == null) {
                return null;
            }
            return in.readAllBytes();
        }
    }

    public static void main(String[] a) throws Exception {
        // ---- ROW 1: which loader defines an image class ---------------------
        // java.base and the other BOOT modules -> null.
        loaderOf("java.lang.String");
        loaderOf("javax.crypto.Cipher");         // java.base
        loaderOf("java.util.logging.Logger");    // java.logging  (boot)
        loaderOf("java.awt.Color");              // java.desktop  (boot)
        loaderOf("javax.naming.Context");        // java.naming   (boot)
        // PLATFORM modules -> the platform loader.
        loaderOf("java.sql.Connection");         // java.sql
        loaderOf("javax.sql.DataSource");        // java.sql
        loaderOf("javax.sql.rowset.CachedRowSet"); // java.sql.rowset
        loaderOf("java.net.http.HttpClient");    // java.net.http
        loaderOf("javax.script.ScriptEngine");   // java.scripting
        loaderOf("com.sun.net.httpserver.HttpServer"); // jdk.httpserver
        loaderOf("javax.smartcardio.CardTerminal");    // java.smartcardio
        // The application's own class.
        row("self.getClassLoader", s(LoaderIdentityProbe.class.getClassLoader()));

        // Shapes that must NOT move.
        row("int.class.getClassLoader", s(int.class.getClassLoader()));
        row("String[].getClassLoader", s(String[].class.getClassLoader()));
        row("Connection[].getClassLoader",
                s(java.lang.reflect.Array.newInstance(
                        Class.forName("java.sql.Connection"), 0).getClass().getClassLoader()));
        row("self[].getClassLoader", s(LoaderIdentityProbe[].class.getClassLoader()));

        // The module's own answer, which the JDK keeps in step with the class's.
        row("java.sql module.getClassLoader",
                s(Class.forName("java.sql.Connection").getModule().getClassLoader()));
        row("java.base module.getClassLoader", s(String.class.getModule().getClassLoader()));

        // ---- ROW 1 consumers: does moving the answer break anything? --------
        Class<?> conn = Class.forName("java.sql.Connection");
        row("Connection.getResource(Connection.class) != null",
                conn.getResource("Connection.class") != null);
        row("Connection.getResourceAsStream(/java/sql/Connection.class) != null",
                conn.getResourceAsStream("/java/sql/Connection.class") != null);
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        row("platform.loadClass(java.sql.Connection) == Connection",
                plat.loadClass("java.sql.Connection") == conn);
        row("Class.forName(.., platform) == Connection",
                Class.forName("java.sql.Connection", false, plat) == conn);
        row("Class.forName(.., app) == Connection",
                Class.forName("java.sql.Connection", false,
                        LoaderIdentityProbe.class.getClassLoader()) == conn);
        row("Connection.getClassLoader().getParent()",
                conn.getClassLoader() == null ? "n/a" : s(conn.getClassLoader().getParent()));
        row("Connection.getClassLoader() == platform", conn.getClassLoader() == plat);
        // Can the platform loader reach an APPLICATION class? HotSpot: no.
        // This is the row that says whether handing a JDK caller the platform
        // loader could NARROW a one-arg Class.forName.
        String platSeesApp;
        try {
            platSeesApp = plat.loadClass("com.example.app.Marker").getName();
        } catch (Throwable t) {
            platSeesApp = "THREW " + t.getClass().getSimpleName();
        }
        row("platform.loadClass(com.example.app.Marker)", platSeesApp);
        // A loader-keyed JDK service: the proxy machinery picks a loader from
        // the interfaces, so this is the shape that notices a wrong answer.
        Object proxy = java.lang.reflect.Proxy.newProxyInstance(
                conn.getClassLoader(), new Class<?>[] { conn }, (p, m, ar) -> null);
        row("Proxy over Connection is a Connection", conn.isInstance(proxy));
        row("Proxy class loader", s(proxy.getClass().getClassLoader()));

        // ---- ROW 2: a custom loader with no recorded URL set ----------------
        Isolated iso = new Isolated();
        row("isolated.getDefinedPackage(com.example.app) BEFORE",
                s(iso.getDefinedPackage("com.example.app")));
        row("isolated.getDefinedPackage(java.lang)", s(iso.getDefinedPackage("java.lang")));
        row("isolated.getDefinedPackage(java.sql)", s(iso.getDefinedPackage("java.sql")));
        row("isolated.getDefinedPackage(no.such.package)",
                s(iso.getDefinedPackage("no.such.package")));
        row("isolated.getDefinedPackage(probe.custom) BEFORE",
                s(iso.getDefinedPackage("probe.custom")));

        byte[] widget = bytesOf("/probe/custom/Widget.class");
        row("widget bytes read", widget != null ? widget.length > 0 : false);
        if (widget != null) {
            Class<?> defined = iso.define("probe.custom.Widget", widget);
            row("defined class name", defined.getName());
            row("defined class loader is the isolated one", defined.getClassLoader() == iso);
            row("isolated.getDefinedPackage(probe.custom) AFTER",
                    s(iso.getDefinedPackage("probe.custom")));
            row("defined.getPackage()", s(defined.getPackage() == null
                    ? null : defined.getPackage().getName()));
            // The package the APP loader defined stays the app loader's.
            row("isolated.getDefinedPackage(com.example.app) AFTER",
                    s(iso.getDefinedPackage("com.example.app")));
        }

        // A URLClassLoader with a positively empty URL set — the row the
        // earlier Spring fix pinned; it must stay null.
        ClassLoader empty = new java.net.URLClassLoader("empty", new java.net.URL[0], null);
        row("emptyURLCL.getDefinedPackage(com.example.app)",
                s(empty.getDefinedPackage("com.example.app")));

        // And the application loader's own answer is unchanged.
        ClassLoader app = LoaderIdentityProbe.class.getClassLoader();
        Class.forName("com.example.app.Marker");
        row("app.getDefinedPackage(com.example.app)",
                s(app.getDefinedPackage("com.example.app") == null
                        ? null : app.getDefinedPackage("com.example.app").getName()));

        System.out.println("DONE LoaderIdentityProbe");
    }
}
