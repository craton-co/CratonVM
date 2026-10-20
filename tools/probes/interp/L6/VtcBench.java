// Virtual-thread continuations: four throughput rows (start + join of 20 000
// trivial virtual threads, a 50 000-round park/unpark ping-pong, 5 000 threads
// each sleeping 1 ms five times, 20 000 tasks on a
// `newVirtualThreadPerTaskExecutor`), `rounds` times (default 2). Timings
// only; compare against HotSpot and a `CRATONVM_VT_CONTINUATIONS=0` run.
//
//   javac -d /tmp/vtc tools/probes/interp/L6/VtcBench.java
//   java     -cp /tmp/vtc VtcBench 2
//   cratonvm -cp /tmp/vtc VtcBench 2
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.concurrent.locks.LockSupport;
public class VtcBench {
    static long t0;
    static void start() { t0 = System.nanoTime(); }
    static void end(String n) { System.out.println(n + " ms=" + (System.nanoTime() - t0) / 1_000_000); }
    public static void main(String[] a) throws Exception {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 2;
        for (int r = 0; r < rounds; r++) {
            start();
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < 20_000; i++) ts.add(Thread.ofVirtual().start(() -> {}));
            for (Thread t : ts) t.join();
            end("startjoin20k");
            start();
            final int n = 50_000;
            AtomicInteger turn = new AtomicInteger();
            Thread[] p = new Thread[2];
            for (int k = 0; k < 2; k++) { final int me = k; p[k] = Thread.ofVirtual().unstarted(() -> {
                for (int i = 0; i < n; i++) { while (turn.get() % 2 != me) LockSupport.park(); turn.incrementAndGet(); LockSupport.unpark(p[1 - me]); } }); }
            p[0].start(); p[1].start(); p[0].join(); p[1].join();
            end("pingpong50k");
            start();
            ts.clear();
            for (int i = 0; i < 5_000; i++) ts.add(Thread.ofVirtual().start(() -> { for (int k = 0; k < 5; k++) { try { Thread.sleep(1); } catch (InterruptedException e) {} } }));
            for (Thread t : ts) t.join();
            end("sleep5k");
            start();
            try (ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor()) {
                List<Future<Integer>> fs = new ArrayList<>();
                for (int i = 0; i < 20_000; i++) { final int v = i; fs.add(ex.submit(() -> v)); }
                long s = 0; for (Future<Integer> f : fs) s += f.get();
            }
            end("executor20k");
        }
    }
}
