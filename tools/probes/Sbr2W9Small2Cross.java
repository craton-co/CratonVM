import java.lang.reflect.*;
public class Sbr2W9Small2Cross {
    static volatile Object sink;
    static volatile int isink;
    interface Body { void run(int n); }
    static void bench(String name, Body b) {
        int n = 100000;
        b.run(n); b.run(n); b.run(n);
        long best = Long.MAX_VALUE;
        long[] r = new long[5];
        for (int rep = 0; rep < 5; rep++) {
            long t = System.nanoTime();
            b.run(n);
            r[rep] = (System.nanoTime() - t) / n;
        }
        java.util.Arrays.sort(r);
        System.out.println(String.format("%-28s median %5d ns/op (min %d)", name, r[2], r[0]));
    }
    public static void main(String[] a) throws Throwable {
        Class<?> s = String.class, arrC = int[].class;
        Object o = new Object();
        bench("empty loop", n -> { int x = 0; for (int i = 0; i < n; i++) x += i; isink = x; });
        bench("Modifier.isPublic", n -> { int x = 0; for (int i = 0; i < n; i++) if (Modifier.isPublic(i)) x++; isink = x; });
        bench("o.getClass()", n -> { Object x = null; for (int i = 0; i < n; i++) x = o.getClass(); sink = x; });
        bench("Class.isArray", n -> { int x = 0; for (int i = 0; i < n; i++) if (s.isArray()) x++; isink = x; });
        bench("Class.isInterface", n -> { int x = 0; for (int i = 0; i < n; i++) if (s.isInterface()) x++; isink = x; });
        bench("Class.getModifiers", n -> { int x = 0; for (int i = 0; i < n; i++) x += s.getModifiers(); isink = x; });
        bench("Class.getModifiers(arr)", n -> { int x = 0; for (int i = 0; i < n; i++) x += arrC.getModifiers(); isink = x; });
        bench("Class.getName", n -> { Object x = null; for (int i = 0; i < n; i++) x = s.getName(); sink = x; });
        bench("Class.getModule", n -> { Object x = null; for (int i = 0; i < n; i++) x = s.getModule(); sink = x; });
        bench("Class.isHidden", n -> { int x = 0; for (int i = 0; i < n; i++) if (s.isHidden()) x++; isink = x; });
        bench("Class.getSuperclass", n -> { Object x = null; for (int i = 0; i < n; i++) x = s.getSuperclass(); sink = x; });
        bench("Class.getComponentType", n -> { Object x = null; for (int i = 0; i < n; i++) x = arrC.getComponentType(); sink = x; });
        bench("Class.getPackageName", n -> { Object x = null; for (int i = 0; i < n; i++) x = s.getPackageName(); sink = x; });
        bench("Class.isAssignableFrom", n -> { int x = 0; for (int i = 0; i < n; i++) if (Object.class.isAssignableFrom(s)) x++; isink = x; });
        bench("System.identityHashCode", n -> { int x = 0; for (int i = 0; i < n; i++) x += System.identityHashCode(o); isink = x; });
        bench("Reflection.getCallerClass", n -> { Object x = null; for (int i = 0; i < n; i++) x = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).getCallerClass(); sink = x; });
        bench("Math.abs (intrinsic)", n -> { int x = 0; for (int i = 0; i < n; i++) x += Math.abs(i - 5); isink = x; });
        bench("Thread.currentThread", n -> { Object x = null; for (int i = 0; i < n; i++) x = Thread.currentThread(); sink = x; });
        bench("System.nanoTime", n -> { long x = 0; for (int i = 0; i < n; i++) x += System.nanoTime(); isink = (int) x; });
    }
}
