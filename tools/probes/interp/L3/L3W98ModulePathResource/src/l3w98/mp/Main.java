package l3w98.mp;

import java.io.InputStream;
import java.net.URL;
import java.util.Collections;

/**
 * Interpreter round i4 wave 98, lane L3: resources of a named APPLICATION
 * module on --module-path (i95-L3-compatible-builtin-loader-find-resource-
 * by-module-is-null-20261127.md). Run from tools/probes/interp/L3/L3W98ModulePathResource:
 *
 *   javac -d out/L3W98MP/classes src/module-info.java src/l3w98/mp/Main.java
 *   cp src/top.txt out/L3W98MP/classes/
 *   cp src/l3w98/mp/data.txt out/L3W98MP/classes/l3w98/mp/
 *   jar --create --file out/L3W98MP/l3w98.mp.jar -C out/L3W98MP/classes .
 *   java --module-path out/L3W98MP/l3w98.mp.jar -m l3w98.mp/l3w98.mp.Main   (modular jar)
 *   java --module-path out/L3W98MP/classes -m l3w98.mp/l3w98.mp.Main        (exploded)
 *
 * Absolute paths are folded to "..." before the "L3W98MP/" marker, so the
 * output is the same in any checkout; the scheme spelling is kept.
 */
public class Main {
    static String shape(URL u) {
        if (u == null) {
            return "null";
        }
        String s = u.toString();
        int i = s.indexOf("L3W98MP/");
        if (i < 0) {
            int c = s.indexOf(':');
            return "other " + (c < 0 ? s : s.substring(0, c + 1));
        }
        String head = s.substring(0, i);
        String scheme = head.startsWith("jar:file:///") ? "jar:file:///"
                : head.startsWith("jar:file:/") ? "jar:file:/"
                : head.startsWith("file:///") ? "file:///"
                : head.startsWith("file:/") ? "file:/"
                : "?";
        return scheme + "..." + s.substring(i);
    }

    static String read(InputStream in) {
        if (in == null) {
            return "null";
        }
        try (InputStream s = in) {
            return new String(s.readAllBytes(), "UTF-8").trim();
        } catch (Exception e) {
            return e.getClass().getName();
        }
    }

    static void row(String name, Object answer) {
        System.out.println(name + " " + answer);
    }

    public static void main(String[] args) throws Exception {
        Module m = Main.class.getModule();
        ClassLoader app = ClassLoader.getSystemClassLoader();
        row("module", m.isNamed() + " " + m.getName());
        row("loader-is-app", Main.class.getClassLoader() == app);
        row("module-loader-is-app", m.getClassLoader() == app);

        // Class.getResource: BuiltinClassLoader.findResource(mn, name) on HotSpot.
        row("class-res", shape(Main.class.getResource("Main.class")));
        row("class-res-abs", shape(Main.class.getResource("/l3w98/mp/Main.class")));
        row("class-res-data", shape(Main.class.getResource("data.txt")));
        row("class-res-top", shape(Main.class.getResource("/top.txt")));
        row("class-res-missing", shape(Main.class.getResource("/missing.txt")));
        row("class-res-other-module", shape(Main.class.getResource("/java/lang/Object.class")));
        URL data = Main.class.getResource("data.txt");
        row("class-res-open", data == null ? "null" : read(data.openStream()));

        // Class.getResourceAsStream / Module.getResourceAsStream.
        row("class-stream-data", read(Main.class.getResourceAsStream("data.txt")));
        row("class-stream-top", read(Main.class.getResourceAsStream("/top.txt")));
        row("class-stream-missing", read(Main.class.getResourceAsStream("/missing.txt")));
        row("class-stream-other-module",
                Main.class.getResourceAsStream("/java/lang/Object.class") == null);
        row("module-stream-data", read(m.getResourceAsStream("l3w98/mp/data.txt")));
        row("module-stream-top", read(m.getResourceAsStream("top.txt")));
        row("module-stream-missing", read(m.getResourceAsStream("nope.txt")));

        // ClassLoader.getResource on the application loader: BuiltinClassLoader
        // findResource(name) asks the module's reader for a package of the
        // module (.class always; other names only when the package is open:
        // l3w98.mp is not), findMiscResource for a package-less name.
        row("cl-class", shape(app.getResource("l3w98/mp/Main.class")));
        row("cl-data", shape(app.getResource("l3w98/mp/data.txt")));
        row("cl-top", shape(app.getResource("top.txt")));
        row("cl-stream-data", read(app.getResourceAsStream("l3w98/mp/data.txt")));
        row("cl-stream-top", read(app.getResourceAsStream("top.txt")));
        row("cl-resources-top", Collections.list(app.getResources("top.txt")).size());
        row("cl-resources-data", Collections.list(app.getResources("l3w98/mp/data.txt")).size());

        // Wave 100, lane L3: getResources("module-info.class") lists this
        // module's own module-info once (findMiscResource over the app
        // loader's nameToModule), never a second class-path spelling of it.
        int own = 0;
        String ownShape = "none";
        for (URL u : Collections.list(app.getResources("module-info.class"))) {
            if (u.toString().contains("L3W98MP/")) {
                own++;
                ownShape = shape(u);
            }
        }
        row("cl-resources-module-info-own", own + " " + ownShape);

        // The defining CodeSource of a module-path class is the module location.
        row("code-source", shape(Main.class.getProtectionDomain().getCodeSource().getLocation()));
    }
}
