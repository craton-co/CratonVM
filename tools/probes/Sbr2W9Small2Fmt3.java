import java.util.*;
public class Sbr2W9Small2Fmt3 {
    static volatile Object sink;
    static volatile int isink;
    interface Body { void run(int n); }
    static void bench(String name, Body b) {
        int n = 50000;
        b.run(n); b.run(n);
        long[] r = new long[5];
        for (int rep = 0; rep < 5; rep++) { long t = System.nanoTime(); b.run(n); r[rep] = (System.nanoTime() - t) / n; }
        java.util.Arrays.sort(r);
        System.out.println(String.format("%-34s median %6d ns/op", name, r[2]));
    }
    static final Object LOCK = new Object();
    public static void main(String[] a) throws Throwable {
        Module m = Object.class.getModule();
        Set<Module> copy = Set.copyOf(ModuleLayer.boot().modules());
        Set<Module> hs = new HashSet<>(ModuleLayer.boot().modules());
        bench("Set.copyOf.contains(module)", n -> { int k = 0; for (int i = 0; i < n; i++) if (copy.contains(m)) k++; isink = k; });
        bench("HashSet.contains(module)", n -> { int k = 0; for (int i = 0; i < n; i++) if (hs.contains(m)) k++; isink = k; });
        bench("synchronized empty", n -> { int k = 0; for (int i = 0; i < n; i++) synchronized (LOCK) { k++; } isink = k; });
        bench("synchronized(this-like) + try/finally", n -> { int k = 0; for (int i = 0; i < n; i++) { synchronized (LOCK) { try { k++; } finally { k++; } } } isink = k; });
        bench("String.class.getClassLoader()==null", n -> { int k = 0; for (int i = 0; i < n; i++) if (String.class.getClassLoader() == null) k++; isink = k; });
        bench("instanceof BuiltinClassLoader-ish", n -> { int k = 0; Object o = ClassLoader.getSystemClassLoader(); for (int i = 0; i < n; i++) if (o instanceof ClassLoader) k++; isink = k; });
        bench("Object.hashCode", n -> { int k = 0; for (int i = 0; i < n; i++) k += m.hashCode(); isink = k; });
        bench("m.equals(m)", n -> { int k = 0; for (int i = 0; i < n; i++) if (m.equals(m)) k++; isink = k; });
    }
}
