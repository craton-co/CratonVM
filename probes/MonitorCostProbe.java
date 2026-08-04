import java.util.Locale;

/** Uncontended monitorenter/monitorexit, alone. */
public class MonitorCostProbe {
    static final Object LOCK = new Object();
    static int sink;
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 2_000_000;
        for (int i = 0; i < n / 10; i++) { spin(); }
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { spin(); }
        long d = System.nanoTime() - t0;
        System.out.println(String.format(Locale.US, "PROBE synchronized enter+exit %9.1f ns/op (sink=%d)", (double) d / n, sink));
    }
    static void spin() { synchronized (LOCK) { sink++; } }
}
