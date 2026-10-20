import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/**
 * SBR round 2, wave 8, lane TLOCAL: GC stress for the ThreadLocal.get/set thin
 * helpers and the Unsafe reference-array reads. Several threads each overwrite
 * their own ThreadLocal with a fresh object every iteration (the helper's SATB
 * pre-barrier and card-marking store), read it back and verify its contents,
 * while a garbage thread forces collections and a ConcurrentHashMap churns
 * (tabAt reads). Prints "bad = 0" when every read returned what was stored.
 *
 *   java [-XX:+UseG1GC|UseSerialGC|UseParallelGC|UseZGC -Xmx64m] Sbr2W8TlocalStress [seconds]
 */
public class Sbr2W8TlocalStress {
    static final ThreadLocal<Object[]> TL = new ThreadLocal<>();
    static final InheritableThreadLocal<Object[]> ITL = new InheritableThreadLocal<>();
    static final ThreadLocal<Object[]> SUP = ThreadLocal.withInitial(() -> new Object[] { "init" });

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        AtomicLong bad = new AtomicLong();
        AtomicLong ops = new AtomicLong();
        ConcurrentHashMap<Integer, Object[]> chm = new ConcurrentHashMap<>();
        int T = 6;
        Thread[] ts = new Thread[T];
        for (int t = 0; t < T; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long local = 0;
                int i = 0;
                TL.set(new Object[] { "t" + id, 0 });
                ITL.set(new Object[] { "i" + id, 0 });
                while (System.nanoTime() < end) {
                    Object[] a = new Object[] { "t" + id, i, new int[4] };
                    TL.set(a);
                    Object[] b = new Object[] { "i" + id, i };
                    ITL.set(b);
                    Object[] ra = TL.get();
                    Object[] rb = ITL.get();
                    if (ra != a || rb != b || !("t" + id).equals(ra[0]) || (Integer) ra[1] != i
                            || !("i" + id).equals(rb[0])) {
                        bad.incrementAndGet();
                    }
                    Object[] s = SUP.get();
                    if (s == null || s.length == 0) {
                        bad.incrementAndGet();
                    }
                    // garbage and map churn
                    byte[] junk = new byte[256 + (i & 1023)];
                    junk[0] = (byte) i;
                    chm.put(id * 100000 + (i & 4095), ra);
                    Object[] r = chm.get(id * 100000 + (i & 4095));
                    if (r != ra) {
                        bad.incrementAndGet();
                    }
                    if ((i & 8191) == 0) {
                        TL.remove();
                        if (TL.get() != null) {
                            bad.incrementAndGet();
                        }
                        TL.set(a);
                    }
                    i++;
                    local++;
                }
                ops.addAndGet(local);
            });
            ts[t].start();
        }
        Thread gc = new Thread(() -> {
            while (System.nanoTime() < end) {
                List<byte[]> keep = new ArrayList<>();
                for (int i = 0; i < 200; i++) {
                    keep.add(new byte[64 * 1024]);
                }
                if (keep.size() > 1_000_000) {
                    System.out.println(keep);
                }
                System.gc();
                try {
                    Thread.sleep(50);
                } catch (InterruptedException e) {
                    return;
                }
            }
        });
        gc.start();
        for (Thread t : ts) {
            t.join();
        }
        gc.join();
        System.out.println("bad = " + bad.get());
        System.out.println("ops>0 = " + (ops.get() > 0));
    }
}
