import java.lang.ref.WeakReference;
import java.security.MessageDigest;
import java.util.*;
import javax.crypto.Cipher;
import javax.crypto.Mac;

/**
 * SBR round 2, wave 8, lane TLOCAL: where does ThreadLocal.get's time go.
 * Rows print min / median / max ns per op.   java Sbr2W8TlocalBench [rounds] [regex]
 */
public class Sbr2W8TlocalBench {
    static final ThreadLocal<Integer> TL = new ThreadLocal<>();
    static final ThreadLocal<Integer> TL2 = ThreadLocal.withInitial(() -> 1);
    static final InheritableThreadLocal<String> ITL = new InheritableThreadLocal<>();
    static volatile Object sink;
    static String filter = ".*";
    interface Body { void run(int n) throws Exception; }

    static void bench(String name, int n, int rounds, Body b) throws Exception {
        if (!name.matches(filter)) return;
        b.run(Math.min(n, 5000)); b.run(Math.min(n, 5000));
        double[] t = new double[rounds];
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            b.run(n);
            t[r] = (System.nanoTime() - t0) / (double) n;
        }
        Arrays.sort(t);
        System.out.printf("%-28s min %9.1f  med %9.1f  max %9.1f ns/op%n", name, t[0], t[rounds / 2], t[rounds - 1]);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 7;
        if (args.length > 1) filter = args[1];
        TL.set(5); ITL.set("x");
        Object o = new Object();
        WeakReference<Object> wr = new WeakReference<>(o);
        bench("empty loop", 1_000_000, rounds, n -> { int s = 0; for (int i = 0; i < n; i++) s += i; sink = s; });
        bench("Thread.currentThread", 1_000_000, rounds, n -> { Thread t = null; for (int i = 0; i < n; i++) t = Thread.currentThread(); sink = t; });
        bench("Thread.isVirtual", 1_000_000, rounds, n -> { boolean b = false; for (int i = 0; i < n; i++) b ^= Thread.currentThread().isVirtual(); sink = b; });
        bench("WeakReference.get", 500_000, rounds, n -> { Object x = null; for (int i = 0; i < n; i++) x = wr.get(); sink = x; });
        bench("WeakReference.refersTo", 500_000, rounds, n -> { boolean b = false; for (int i = 0; i < n; i++) b ^= wr.refersTo(o); sink = b; });
        bench("Object.hashCode", 500_000, rounds, n -> { int s = 0; for (int i = 0; i < n; i++) s += o.hashCode(); sink = s; });
        bench("tl.get (set)", 200_000, rounds, n -> { int s = 0; for (int i = 0; i < n; i++) s += TL.get(); sink = s; });
        bench("tl.get (withInitial)", 200_000, rounds, n -> { int s = 0; for (int i = 0; i < n; i++) s += TL2.get(); sink = s; });
        bench("itl.get", 200_000, rounds, n -> { Object s = null; for (int i = 0; i < n; i++) s = ITL.get(); sink = s; });
        bench("tl.set", 200_000, rounds, n -> { for (int i = 0; i < n; i++) TL.set(i); });
        bench("tl.set+remove", 100_000, rounds, n -> { for (int i = 0; i < n; i++) { TL.set(i); TL.remove(); } TL.set(5); });
        java.util.concurrent.ConcurrentHashMap<String, String> chm = new java.util.concurrent.ConcurrentHashMap<>();
        for (int i = 0; i < 100; i++) chm.put("key" + i, "v" + i);
        HashMap<String, String> hm = new HashMap<>(chm);
        bench("chm.get", 200_000, rounds, n -> { Object s = null; for (int i = 0; i < n; i++) s = chm.get("key50"); sink = s; });
        bench("hm.get", 200_000, rounds, n -> { Object s = null; for (int i = 0; i < n; i++) s = hm.get("key50"); sink = s; });
        bench("md.getInstance SHA-256", 2_000, rounds, n -> { Object s = null; for (int i = 0; i < n; i++) s = MessageDigest.getInstance("SHA-256"); sink = s; });
        bench("mac.getInstance HmacSHA256", 2_000, rounds, n -> { Object s = null; for (int i = 0; i < n; i++) s = Mac.getInstance("HmacSHA256"); sink = s; });
        bench("cipher.getInstance GCM", 1_000, rounds, n -> { Object s = null; for (int i = 0; i < n; i++) s = Cipher.getInstance("AES/GCM/NoPadding"); sink = s; });
        bench("cipher.getInstance CBC", 1_000, rounds, n -> { Object s = null; for (int i = 0; i < n; i++) s = Cipher.getInstance("AES/CBC/PKCS5Padding"); sink = s; });
    }
}
