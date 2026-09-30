// sun.nio.ch.Util temp direct buffers handed to two threads at once (shared carrier-thread-local map). Needs --add-exports java.base/sun.nio.ch=ALL-UNNAMED at compile and run.
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D2)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] UtilShareProbe <threads> <iterations>
// Compare with the same command on HotSpot (java -cp ...).
import java.nio.ByteBuffer;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

public class UtilShareProbe {
    public static void main(String[] a) throws Exception {
        int threads = Integer.parseInt(a[0]); int iters = Integer.parseInt(a[1]);
        ConcurrentHashMap<Long, Integer> owner = new ConcurrentHashMap<>();
        AtomicInteger dup = new AtomicInteger(), bad = new AtomicInteger(), npe = new AtomicInteger();
        AtomicReference<Throwable> first = new AtomicReference<>();
        List<Thread> ts = new ArrayList<>();
        for (int t = 0; t < threads; t++) {
            final int id = t + 1;
            ts.add(new Thread(() -> {
                Random r = new Random(id);
                for (int i = 0; i < iters; i++) {
                    int size = 1 + r.nextInt(64 * 1024);
                    ByteBuffer b;
                    try { b = sun.nio.ch.Util.getTemporaryDirectBuffer(size); }
                    catch (Throwable e) { npe.incrementAndGet(); first.compareAndSet(null, e); continue; }
                    long addr = ((sun.nio.ch.DirectBuffer) b).address();
                    Integer prev = owner.putIfAbsent(addr, id);
                    if (prev != null) { dup.incrementAndGet(); }
                    try {
                        for (int k = 0; k < Math.min(size, 64); k++) b.put(k, (byte) id);
                        Thread.yield();
                        for (int k = 0; k < Math.min(size, 64); k++) if (b.get(k) != (byte) id) { bad.incrementAndGet(); break; }
                    } catch (Throwable e) { bad.incrementAndGet(); first.compareAndSet(null, e); }
                    if (prev == null) owner.remove(addr, id);
                    sun.nio.ch.Util.releaseTemporaryDirectBuffer(b);
                }
            }, "u" + id));
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        System.out.println("threads=" + threads + " dupOwner=" + dup + " badData=" + bad + " getFail=" + npe);
        if (first.get() != null) first.get().printStackTrace(System.out);
    }
}
