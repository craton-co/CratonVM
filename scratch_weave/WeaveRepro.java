import java.io.InputStream;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Paths;

public class WeaveRepro {

    static final String DOTTED = "org.apache.catalina.loader.TesterUnweavedClass";

    // Compiled TesterUnweavedClass whose doMethod() returns "Hello, Weaver #1!"
    static final byte[] WEAVED_REPLACEMENT_1 = new byte[] { -54, -2, -70, -66, 0, 0, 0, 50, 0, 17, 10, 0, 4, 0,
            13, 8, 0, 14, 7, 0, 15, 7, 0, 16, 1, 0, 6, 60, 105, 110, 105, 116, 62, 1, 0, 3, 40, 41, 86, 1, 0, 4, 67,
            111, 100, 101, 1, 0, 15, 76, 105, 110, 101, 78, 117, 109, 98, 101, 114, 84, 97, 98, 108, 101, 1, 0, 8, 100,
            111, 77, 101, 116, 104, 111, 100, 1, 0, 20, 40, 41, 76, 106, 97, 118, 97, 47, 108, 97, 110, 103, 47, 83,
            116, 114, 105, 110, 103, 59, 1, 0, 10, 83, 111, 117, 114, 99, 101, 70, 105, 108, 101, 1, 0, 24, 84, 101,
            115, 116, 101, 114, 85, 110, 119, 101, 97, 118, 101, 100, 67, 108, 97, 115, 115, 46, 106, 97, 118, 97, 12,
            0, 5, 0, 6, 1, 0, 17, 72, 101, 108, 108, 111, 44, 32, 87, 101, 97, 118, 101, 114, 32, 35, 49, 33, 1, 0, 46,
            111, 114, 103, 47, 97, 112, 97, 99, 104, 101, 47, 99, 97, 116, 97, 108, 105, 110, 97, 47, 108, 111, 97, 100,
            101, 114, 47, 84, 101, 115, 116, 101, 114, 85, 110, 119, 101, 97, 118, 101, 100, 67, 108, 97, 115, 115, 1,
            0, 16, 106, 97, 118, 97, 47, 108, 97, 110, 103, 47, 79, 98, 106, 101, 99, 116, 0, 33, 0, 3, 0, 4, 0, 0, 0,
            0, 0, 2, 0, 1, 0, 5, 0, 6, 0, 1, 0, 7, 0, 0, 0, 29, 0, 1, 0, 1, 0, 0, 0, 5, 42, -73, 0, 1, -79, 0, 0, 0, 1,
            0, 8, 0, 0, 0, 6, 0, 1, 0, 0, 0, 19, 0, 1, 0, 9, 0, 10, 0, 1, 0, 7, 0, 0, 0, 27, 0, 1, 0, 1, 0, 0, 0, 3, 18,
            2, -80, 0, 0, 0, 1, 0, 8, 0, 0, 0, 6, 0, 1, 0, 0, 0, 22, 0, 1, 0, 11, 0, 0, 0, 2, 0, 12 };

    static final class DefiningLoader extends ClassLoader {
        DefiningLoader() { super(null); } // bootstrap parent -> no delegation
        Class<?> define(String dotted, byte[] b) {
            return defineClass(dotted, b, 0, b.length);
        }
    }

    static String run(String label, byte[] bytes) throws Exception {
        DefiningLoader cl = new DefiningLoader();
        Class<?> c = cl.define(DOTTED, bytes);
        Method m = c.getMethod("doMethod");
        Object o = c.getConstructor().newInstance();
        Class<?> oc = o.getClass();
        System.out.println("  [" + label + "] c.id=" + System.identityHashCode(c)
                + " c.loader=" + c.getClassLoader()
                + " o.class.id=" + System.identityHashCode(oc)
                + " o.class==c? " + (oc == c)
                + " m.declClass.id=" + System.identityHashCode(m.getDeclaringClass()));
        return (String) m.invoke(o);
    }

    public static void main(String[] args) throws Exception {
        byte[] orig = Files.readAllBytes(Paths.get("scratch_weave/TesterUnweavedClass.class"));

        String a = run("A", WEAVED_REPLACEMENT_1);
        String b = run("B", orig);
        String c = run("C", WEAVED_REPLACEMENT_1);

        System.out.println("CASE A (fresh loader, weaved bytes): " + a + "   [expect: Hello, Weaver #1!]");
        System.out.println("CASE B (fresh loader, orig bytes):   " + b + "   [expect: Hello, Unweaved World!]");
        System.out.println("CASE C (fresh loader, weaved again): " + c + "   [expect: Hello, Weaver #1!]");

        boolean ok = a.equals("Hello, Weaver #1!")
                && b.equals("Hello, Unweaved World!")
                && c.equals("Hello, Weaver #1!");
        System.out.println(ok ? "RESULT: PASS" : "RESULT: FAIL");
    }
}
