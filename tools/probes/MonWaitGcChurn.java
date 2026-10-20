// HotSpot 25 (-Xmx64m): three "round r parties=4 n=20000 sum=22130832263" lines, then PASS.
// MonWaitGcChurn: wait/notifyAll hand-overs and virtual calls that hold references, while
// another thread churns the heap so collections land inside lazy waits, inside the
// running re-acquisition, and across compiled virtual-call cascades. Every object a
// worker holds across a wait or a call is re-validated; the printed lines are stable.
import java.util.ArrayList;
import java.util.List;

public class MonWaitGcChurn {
    static final Object LOCK = new Object();
    static int turn;
    static volatile boolean stop;

    interface Shape { long area(); Shape next(); }
    static final class Sq implements Shape {
        final long s; final Shape n; final long[] pad = new long[4];
        Sq(long s, Shape n) { this.s = s; this.n = n; pad[3] = s; }
        public long area() { return s * s + pad[3] - s; }
        public Shape next() { return n; }
    }
    static final class Rect implements Shape {
        final long w, h; final Shape n; final String tag;
        Rect(long w, long h, Shape n) { this.w = w; this.h = h; this.n = n; this.tag = "r" + w; }
        public long area() { return w * h + (tag.length() > 0 ? 0 : 1); }
        public Shape next() { return n; }
    }
    static final class Tri implements Shape {
        final long b, h; final Shape n; final Object[] box = new Object[1];
        Tri(long b, long h, Shape n) { this.b = b; this.h = h; this.n = n; box[0] = Long.valueOf(b); }
        public long area() { return b * h / 2 + ((Long) box[0] - b); }
        public Shape next() { return n; }
    }

    static Shape build(int me, int i) {
        Shape s = null;
        for (int k = 0; k < 6; k++) {
            int v = me * 1000 + i % 97 + k;
            switch ((i + k) % 3) {
                case 0: s = new Sq(v, s); break;
                case 1: s = new Rect(v, k + 1, s); break;
                default: s = new Tri(v, k + 2, s); break;
            }
        }
        return s;
    }

    // Megamorphic receiver chain walked through interface calls (the cascade).
    static long walk(Shape s) {
        long t = 0;
        while (s != null) { t += s.area(); s = s.next(); }
        return t;
    }

    static long worker(int me, int parties, int n) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            Shape held = build(me, i);             // live across the wait and the calls
            long expect = walk(held);
            synchronized (LOCK) {
                while (turn != me) LOCK.wait();
                turn = (me + 1) % parties;
                LOCK.notifyAll();
                long again = walk(held);           // after the hand-over: same answer
                if (again != expect) throw new IllegalStateException("me=" + me + " i=" + i + " " + again + " != " + expect);
                sum += again % 1_000_003;
            }
        }
        return sum;
    }

    public static void main(String[] a) throws Exception {
        int parties = a.length > 0 ? Integer.parseInt(a[0]) : 4;
        int n = a.length > 1 ? Integer.parseInt(a[1]) : 20_000;
        Thread churn = new Thread(() -> {
            List<byte[]> keep = new ArrayList<>();
            long x = 0;
            while (!stop) {
                byte[] b = new byte[512 + (int) (x++ % 1024)];
                b[0] = (byte) x;
                keep.add(b);
                if (keep.size() > 4000) keep.subList(0, 2000).clear();
            }
        });
        churn.setDaemon(true);
        churn.start();
        for (int round = 0; round < 3; round++) {
            turn = 0;
            long[] sums = new long[parties];
            Thread[] ts = new Thread[parties];
            for (int p = 0; p < parties; p++) {
                final int me = p;
                ts[p] = new Thread(() -> {
                    try { sums[me] = worker(me, parties, n); } catch (InterruptedException e) { throw new RuntimeException(e); }
                });
                ts[p].start();
            }
            for (Thread t : ts) t.join();
            long total = 0;
            for (long s : sums) total += s;
            System.out.println("round " + round + " parties=" + parties + " n=" + n + " sum=" + total);
        }
        stop = true;
        churn.join();
        System.out.println("PASS");
    }
}
