import java.lang.reflect.Constructor;
import java.util.*;

/**
 * Interpreter round i4 wave 98, lane L3: getDeclaredConstructors() /
 * getConstructors() ORDER (unsorted), which HotSpot takes from its methods
 * array as `Method::sort_methods`' unstable quicksort left the `<init>`
 * overloads. JDK classes (every method name in the CDS archive) and a few
 * application classes (run-time names).
 */
public class L3W98CtorOrder {
    enum En { X; En(String s) {} En() {} }
    static class Three { Three() {} Three(int a) {} Three(String s) {} void run() {} int size() { return 0; } }
    static class Many {
        Many() {} Many(int a) {} Many(long a) {} Many(String a) {} Many(int a, int b) {}
        void alpha() {} void beta() {} static void gamma() {} public String toString() { return ""; }
    }
    record Rec(int a, String b) { Rec(int a) { this(a, ""); } Rec() { this(0); } }

    static String list(Constructor<?>[] cs) {
        List<String> out = new ArrayList<>();
        for (Constructor<?> c : cs) out.add(Arrays.toString(c.getParameterTypes()).replace("class ", "").replace("interface ", ""));
        return out.toString();
    }

    public static void main(String[] args) {
        Class<?>[] classes = {
            En.class, Three.class, Many.class, Rec.class,
            ArrayList.class, HashMap.class, Thread.class, StringBuilder.class, java.io.File.class,
            Locale.class, Integer.class, Object.class, Exception.class, RuntimeException.class,
            java.util.concurrent.ConcurrentHashMap.class, TreeMap.class, java.math.BigInteger.class,
            java.io.FileInputStream.class, java.net.URL.class, Thread.State.class,
        };
        for (Class<?> c : classes) {
            System.out.println(c.getName() + " declared: " + list(c.getDeclaredConstructors()));
            System.out.println(c.getName() + " public: " + list(c.getConstructors()));
        }
    }
}
