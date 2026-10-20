import java.io.InputStream;
import java.net.URL;
import java.util.*;

/**
 * Interpreter round i4 wave 98, lane L3: getResource / getResources /
 * getResourceAsStream on every built-in loader (and through Class and
 * Module) as HotSpot 25 answers them. Run from the class's own directory on
 * the class path (`java -cp . L3W98ResourcesByLoader`); a class-path URL is
 * folded to its scheme plus the part after the class-path root.
 */
public class L3W98ResourcesByLoader {
    static String fold(URL u) {
        if (u == null) return "null";
        String s = u.toString();
        if (s.startsWith("jrt:")) return s;
        int i = s.lastIndexOf("/L3W98ResourcesByLoader");
        String scheme = s.startsWith("jar:file:///") ? "jar:file:///" : s.startsWith("jar:file:/") ? "jar:file:/"
                : s.startsWith("file:///") ? "file:///" : s.startsWith("file:/") ? "file:/" : s.substring(0, Math.max(0, s.indexOf(':') + 1));
        return i < 0 ? scheme + "...(" + s.substring(s.lastIndexOf('/') + 1) + ")" : scheme + "..." + s.substring(i);
    }

    static String all(Enumeration<URL> e) {
        List<String> out = new ArrayList<>();
        while (e.hasMoreElements()) out.add(fold(e.nextElement()));
        return out.toString();
    }

    static String len(InputStream in) {
        if (in == null) return "null";
        try (InputStream s = in) {
            return "bytes>0=" + (s.readAllBytes().length > 0);
        } catch (Exception e) {
            return e.getClass().getName();
        }
    }

    static void row(String label, Object v) {
        System.out.println(label + ": " + v);
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        Map<String, ClassLoader> loaders = new LinkedHashMap<>();
        loaders.put("app", app);
        loaders.put("platform", plat);
        loaders.put("context", Thread.currentThread().getContextClassLoader());
        String[] names = {
            "java/lang/Object.class", "/java/lang/Object.class", "java/sql/Date.class", "java/net/http/HttpClient.class",
            "com/sun/tools/javac/Main.class", "L3W98ResourcesByLoader.class", "META-INF/services/java.sql.Driver",
            // "module-info.class" moved to L3W99ModuleInfoResources (wave 99):
            // HotSpot's list order inside a shared ConcurrentHashMap bin is a
            // CDS dump-time artifact, so that probe prints it canonically.
            "java/lang/", "java/lang", "", "no/such/Thing.class", "jdk/internal/misc/Unsafe.class",
        };
        for (Map.Entry<String, ClassLoader> e : loaders.entrySet()) {
            ClassLoader l = e.getValue();
            for (String n : names) {
                row(e.getKey() + " getResource(" + n + ")", fold(l.getResource(n)));
                row(e.getKey() + " getResources(" + n + ")", all(l.getResources(n)));
                row(e.getKey() + " stream(" + n + ")", len(l.getResourceAsStream(n)));
            }
        }
        for (String n : names) {
            row("system getSystemResource(" + n + ")", fold(ClassLoader.getSystemResource(n)));
        }
        Class<?>[] classes = { Object.class, java.sql.Date.class, L3W98ResourcesByLoader.class, int[].class, Map.Entry.class };
        String[] rel = { "Object.class", "/java/lang/Object.class", "Date.class", "/java/sql/Date.class",
                "L3W98ResourcesByLoader.class", "/L3W98ResourcesByLoader.class", "Map.class", "Map$Entry.class", "missing.txt" };
        for (Class<?> c : classes) {
            for (String n : rel) {
                row(c.getTypeName() + " getResource(" + n + ")", fold(c.getResource(n)));
                row(c.getTypeName() + " stream(" + n + ")", len(c.getResourceAsStream(n)));
            }
        }
        Module base = Object.class.getModule();
        row("java.base module stream java/lang/Object.class", len(base.getResourceAsStream("java/lang/Object.class")));
        row("java.base module stream /java/lang/Object.class", len(base.getResourceAsStream("/java/lang/Object.class")));
        row("java.base module stream java/sql/Date.class", len(base.getResourceAsStream("java/sql/Date.class")));
        row("java.base module stream module-info.class", len(base.getResourceAsStream("module-info.class")));
        row("java.base module stream jdk/internal/misc/Unsafe.class", len(base.getResourceAsStream("jdk/internal/misc/Unsafe.class")));
        row("java.base module stream java/lang/uniName.dat", len(base.getResourceAsStream("java/lang/uniName.dat")));
        Module sql = java.sql.Date.class.getModule();
        row("java.sql module stream java/sql/Date.class", len(sql.getResourceAsStream("java/sql/Date.class")));
        Module unnamed = L3W98ResourcesByLoader.class.getModule();
        row("unnamed module stream L3W98ResourcesByLoader.class", len(unnamed.getResourceAsStream("L3W98ResourcesByLoader.class")));
        row("unnamed module stream java/lang/Object.class", len(unnamed.getResourceAsStream("java/lang/Object.class")));
        try {
            app.getResource(null);
        } catch (NullPointerException npe) {
            row("getResource(null)", "NPE " + npe.getMessage());
        }
        try {
            L3W98ResourcesByLoader.class.getResource(null);
        } catch (NullPointerException npe) {
            row("Class.getResource(null)", "NPE");
        }
    }
}
