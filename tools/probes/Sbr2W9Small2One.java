public class Sbr2W9Small2One {
    static volatile Object sink;
    static volatile int isink;
    public static void main(String[] a) {
        Class<?> s = String.class;
        Object o = new Object();
        String mode = a.length > 0 ? a[0] : "isArray";
        int n = 300000;
        for (int rep = 0; rep < 8; rep++) {
            long t = System.nanoTime();
            if (mode.equals("isArray")) { int x = 0; for (int i = 0; i < n; i++) if (s.isArray()) x++; isink = x; }
            else if (mode.equals("getClass")) { Object x = null; for (int i = 0; i < n; i++) x = o.getClass(); sink = x; }
            else if (mode.equals("ihc")) { int x = 0; for (int i = 0; i < n; i++) x += System.identityHashCode(o); isink = x; }
            else if (mode.equals("mods")) { int x = 0; for (int i = 0; i < n; i++) x += s.getModifiers(); isink = x; }
            System.out.println(mode + " ns/op " + (System.nanoTime() - t) / n);
        }
    }
}
