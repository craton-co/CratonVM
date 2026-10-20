import java.lang.reflect.*;
public class Sbr2W9Small2Fmt2 {
    static volatile Object sink;
    static volatile int isink;
    interface Body { void run(int n); }
    static void bench(String name, Body b) {
        int n = 20000;
        b.run(n); b.run(n);
        long[] r = new long[5];
        for (int rep = 0; rep < 5; rep++) { long t = System.nanoTime(); b.run(n); r[rep] = (System.nanoTime() - t) / n; }
        java.util.Arrays.sort(r);
        System.out.println(String.format("%-34s median %6d ns/op", name, r[2]));
    }
    public static void main(String[] a) throws Throwable {
        Module m = Object.class.getModule();
        Class<?> hm = Class.forName("java.lang.StackTraceElement$HashedModules");
        Method c = hm.getDeclaredMethod("contains", Module.class); c.setAccessible(true);
        java.lang.invoke.MethodHandle mh = java.lang.invoke.MethodHandles.privateLookupIn(hm, java.lang.invoke.MethodHandles.lookup()).unreflect(c);
        Class<?> s = String.class;
        bench("Class.getModule", n -> { Object x = null; for (int i = 0; i < n; i++) x = s.getModule(); sink = x; });
        bench("ModuleLayer.boot()", n -> { Object x = null; for (int i = 0; i < n; i++) x = ModuleLayer.boot(); sink = x; });
        bench("Module.getLayer()", n -> { Object x = null; for (int i = 0; i < n; i++) x = m.getLayer(); sink = x; });
        bench("HashedModules.contains (mh)", n -> { int k = 0; try { for (int i = 0; i < n; i++) if ((boolean) mh.invoke(m)) k++; } catch (Throwable t) { throw new RuntimeException(t); } isink = k; });
        bench("Class.getClassLoader (String)", n -> { Object x = null; for (int i = 0; i < n; i++) x = s.getClassLoader(); sink = x; });
        bench("Module.hashCode", n -> { int k = 0; for (int i = 0; i < n; i++) k += m.hashCode(); isink = k; });
        bench("new Throwable().getStackTrace", n -> { Object x = null; for (int i = 0; i < n; i++) x = new Throwable().getStackTrace(); sink = x; });
        bench("new Throwable()", n -> { Object x = null; for (int i = 0; i < n; i++) x = new Throwable(); sink = x; });
    }
}
