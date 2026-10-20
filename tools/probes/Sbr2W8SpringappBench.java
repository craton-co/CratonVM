import java.beans.Introspector;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.*;
import java.util.*;
import java.util.concurrent.CopyOnWriteArraySet;

/** SBR2 W8 SPRINGAPP: ns/op of the retired rows, native (UNRETIRE) vs real bytecode. One line per row: "name ns/op". */
public class Sbr2W8SpringappBench {
    interface G<A> {}
    static class B<X> {}
    static class S0 extends B<String> implements G<Integer>, Comparable<S0> { public int compareTo(S0 o) { return 0; } public void m(int alpha, String beta) {} }
    static Object sink;
    static Class<?>[] many;

    static long time(Runnable r, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) r.run();
        return (System.nanoTime() - t0) / n;
    }
    static void row(String name, Runnable r, int n) {
        for (int w = 0; w < 3; w++) time(r, Math.max(1, n / 5));
        long[] s = new long[5];
        for (int i = 0; i < s.length; i++) s[i] = time(r, n);
        Arrays.sort(s);
        System.out.println(name + " " + s[2]);
    }

    public static void main(String[] a) throws Throwable {
        Class<?> c = S0.class;
        Method m = S0.class.getMethod("m", int.class, String.class);
        Constructor<?> k = Arrays.class.getDeclaredConstructors()[0];
        var L = MethodHandles.lookup();
        var cow = new CopyOnWriteArraySet<>(List.of("a", "b", "c"));
        Class<?>[] pool = { String.class, Integer.class, ArrayList.class, HashMap.class, TreeMap.class, Thread.class, Object.class, S0.class, B.class, Long.class, Double.class };
        row("gsuper hot", () -> sink = c.getGenericSuperclass(), 20000);
        row("gifaces hot", () -> sink = c.getGenericInterfaces(), 20000);
        row("gsuper pool", () -> { for (Class<?> p : pool) sink = p.getGenericSuperclass(); }, 5000);
        row("arrayType", () -> sink = c.arrayType(), 50000);
        row("decapitalize", () -> sink = Introspector.decapitalize("FooBar"), 50000);
        row("console", () -> sink = System.console(), 50000);
        row("getSecurityManager", () -> sink = System.getSecurityManager(), 200000);
        row("findClass", () -> { try { sink = L.findClass("java.lang.String"); } catch (Exception e) { throw new RuntimeException(e); } }, 5000);
        row("method.getParameters", () -> sink = m.getParameters(), 20000);
        row("cowas new", () -> sink = new CopyOnWriteArraySet<String>(), 50000);
        row("cowas isEmpty", () -> sink = cow.isEmpty(), 100000);
        row("cowas iterator", () -> sink = cow.iterator(), 100000);
        row("cowas add+iter", () -> { var s = new CopyOnWriteArraySet<String>(); s.add("x"); for (String z : s) sink = z; }, 20000);
        row("finder findAll", () -> sink = java.lang.module.ModuleFinder.ofSystem().findAll(), 500);
    }
}
