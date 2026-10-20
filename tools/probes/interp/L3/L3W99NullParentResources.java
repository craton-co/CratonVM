import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.*;

/**
 * Interpreter round i4 wave 99, lane L3: getResource / getResources /
 * getResourceAsStream of loaders whose parent is null (the bootstrap loader).
 * HotSpot's ClassLoader.getResource(s) asks BootLoader.findResource(s) first;
 * the boot loader sees only its own modules' resources (no class path, no
 * platform- or application-loader module). Page
 * docs/internal/fixed-bugs/interpreter-i99-L3-null-parent-get-resources-omits-boot-FIXED-20261015.md.
 *
 * Run from the class's own directory: java -cp . L3W99NullParentResources
 *
 * Expected HotSpot 25 output (both loaders print the same rows):
 *   <l> getResource(java/lang/Object.class): jrt:/java.base/java/lang/Object.class
 *   <l> getResources(java/lang/Object.class): [jrt:/java.base/java/lang/Object.class]
 *   <l> stream(java/lang/Object.class): bytes>0=true
 *   <l> getResource(java/sql/Date.class): null          (platform loader's module)
 *   <l> getResources(java/sql/Date.class): []
 *   <l> stream(java/sql/Date.class): null
 *   the same null / [] / null rows for com/sun/tools/javac/Main.class (application
 *   loader's module), META-INF/MANIFEST.MF and L3W99NullParentResources.class
 *   (class path), and no/such/Thing.class
 *   <l> getResources(module-info.class) count: 21
 *   <l> getResource(module-info.class) listed: true
 *   <l> getResource(module-info.class) == first: true
 *     (which module comes first varies between HotSpot runs; not printed)
 * where <l> is "url-null-parent" (new URLClassLoader(new URL[0], null)) and
 * "custom-null-parent" (a ClassLoader subclass constructed with a null parent).
 */
public class L3W99NullParentResources {
    static String len(InputStream in) {
        if (in == null) return "null";
        try (InputStream s = in) {
            return "bytes>0=" + (s.readAllBytes().length > 0);
        } catch (Exception e) {
            return e.getClass().getName();
        }
    }

    static String all(Enumeration<URL> e) {
        List<String> out = new ArrayList<>();
        while (e.hasMoreElements()) out.add(String.valueOf(e.nextElement()));
        return out.toString();
    }

    static void rows(String label, ClassLoader l) throws Exception {
        String[] names = {
            "java/lang/Object.class", "java/sql/Date.class", "com/sun/tools/javac/Main.class",
            "META-INF/MANIFEST.MF", "L3W99NullParentResources.class", "no/such/Thing.class",
        };
        for (String n : names) {
            System.out.println(label + " getResource(" + n + "): " + l.getResource(n));
            System.out.println(label + " getResources(" + n + "): " + all(l.getResources(n)));
            System.out.println(label + " stream(" + n + "): " + len(l.getResourceAsStream(n)));
        }
        List<URL> mi = Collections.list(l.getResources("module-info.class"));
        System.out.println(label + " getResources(module-info.class) count: " + mi.size());
        // The first answer is HotSpot's own in-bin insertion order, which varies
        // between HotSpot runs; print only that it is one of the listed URLs and
        // the list's first element (wave 100 hand-back).
        URL one = l.getResource("module-info.class");
        boolean listed = false;
        for (URL u : mi) if (one != null && u.toString().equals(one.toString())) listed = true;
        System.out.println(label + " getResource(module-info.class) listed: " + listed);
        System.out.println(label + " getResource(module-info.class) == first: "
                + (one != null && !mi.isEmpty() && mi.get(0).toString().equals(one.toString())));
    }

    public static void main(String[] args) throws Exception {
        try (URLClassLoader u = new URLClassLoader(new URL[0], null)) {
            rows("url-null-parent", u);
        }
        rows("custom-null-parent", new ClassLoader(null) { });
    }
}
