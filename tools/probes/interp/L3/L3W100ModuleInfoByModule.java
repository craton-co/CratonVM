import java.io.InputStream;
import java.lang.module.ModuleDescriptor;
import java.net.URL;

/**
 * Interpreter round i5 wave 100, lane L3: a named image module's own
 * module-info.class through Class.getResource / getResourceAsStream and
 * Module.getResourceAsStream. Class.getResource of a named-module class asks
 * BootLoader.findResource(mn, name) (boot module) or
 * BuiltinClassLoader.findResource(mn, name) (platform / application module);
 * module-info.class is never encapsulated (Resources.canEncapsulate is false
 * for a ".class" name). CratonVM's --compatible fallback looked the name up
 * in an index that holds no module-info.class, so the URL rows read null.
 *
 * Run: java -cp . L3W100ModuleInfoByModule
 *
 * Expected HotSpot 25:
 *   java.base url: jrt:/java.base/module-info.class
 *   java.base class stream: java.base
 *   java.base module stream: java.base
 *   java.sql url: jrt:/java.sql/module-info.class
 *   java.sql class stream: java.sql
 *   java.sql module stream: java.sql
 *   jdk.compiler url: jrt:/jdk.compiler/module-info.class
 *   jdk.compiler class stream: jdk.compiler
 *   jdk.compiler module stream: jdk.compiler
 *   relative url: null
 */
public class L3W100ModuleInfoByModule {
    static String desc(InputStream in) {
        if (in == null) return "null";
        try (InputStream s = in) {
            return ModuleDescriptor.read(s).name();
        } catch (Exception e) {
            return e.getClass().getName();
        }
    }

    static void rows(String label, Class<?> c) throws Exception {
        URL u = c.getResource("/module-info.class");
        System.out.println(label + " url: " + u);
        System.out.println(label + " class stream: " + desc(c.getResourceAsStream("/module-info.class")));
        System.out.println(label + " module stream: " + desc(c.getModule().getResourceAsStream("module-info.class")));
    }

    public static void main(String[] args) throws Exception {
        rows("java.base", Object.class);
        rows("java.sql", java.sql.Date.class);
        rows("jdk.compiler", Class.forName("com.sun.tools.javac.Main"));
        // Relative to java.lang: java/lang/module-info.class, which no module holds.
        System.out.println("relative url: " + Object.class.getResource("module-info.class"));
    }
}
