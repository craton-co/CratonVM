import java.io.*;
import java.lang.ref.*;

/** SBR2 W8 SMALL: A/B timing of the retired rows (ms per loop; run ON vs CRATONVM_UNRETIRE_NATIVE_SHADOW=... interleaved). */
public class Sbr2W8SmallHot {
    static long sink;
    public static void main(String[] a) throws Throwable {
        Object strong = new Object();
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            var q = new ReferenceQueue<Object>();
            for (int i = 0; i < 300000; i++) if (q.poll() == null) sink++;
            long t1 = System.nanoTime();
            for (int i = 0; i < 100000; i++) { var s = new SoftReference<>(strong); s.clear(); if (s.get() == null) sink++; }
            long t2 = System.nanoTime();
            for (int i = 0; i < 300000; i++) Reference.reachabilityFence(strong);
            long t3 = System.nanoTime();
            for (int i = 0; i < 20000; i++) { var w = new WeakReference<>(strong, q); w.enqueue(); if (q.poll() == w) sink++; }
            long t4 = System.nanoTime();
            byte[] buf = new byte[64];
            for (int i = 0; i < 20000; i++) { var b = new BufferedOutputStream(new ByteArrayOutputStream(), 32); b.write(buf, 0, 8); b.close(); }
            long t5 = System.nanoTime();
            var f = new FilterInputStream(new ByteArrayInputStream(new byte[1 << 20])) { };
            for (int i = 0; i < 200000; i++) sink += f.skip(3);
            long t6 = System.nanoTime();
            for (int i = 0; i < 300; i++) if (q.remove(1) == null) sink++; // timed remove on empty queue would block; use poll-equivalent below
            long t7 = System.nanoTime();
            System.out.println("poll=" + (t1 - t0) / 1000000 + " clear=" + (t2 - t1) / 1000000 + " fence=" + (t3 - t2) / 1000000 + " enq=" + (t4 - t3) / 1000000 + " bosclose=" + (t5 - t4) / 1000000 + " fisskip=" + (t6 - t5) / 1000000 + " remove1=" + (t7 - t6) / 1000000);
        }
    }
}
