import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.*;
import java.lang.ref.*;
public class Sbr2W9Small2Park {
    static long ms(long t0) { return (System.nanoTime() - t0) / 1_000_000; }
    public static void main(String[] a) throws Throwable {
        int n = 300;
        for (int rep = 0; rep < 2; rep++) {
            long t = System.nanoTime();
            for (int i = 0; i < n; i++) LockSupport.parkNanos(1_000_000L);
            System.out.println("parkNanos(1ms) x" + n + " = " + ms(t) + " ms");
            Object o = new Object();
            t = System.nanoTime();
            synchronized (o) { for (int i = 0; i < n; i++) o.wait(1); }
            System.out.println("Object.wait(1) x" + n + " = " + ms(t) + " ms");
            ReentrantLock l = new ReentrantLock(); Condition c = l.newCondition();
            t = System.nanoTime();
            l.lock(); try { for (int i = 0; i < n; i++) c.await(1_000_000L, TimeUnit.NANOSECONDS); } finally { l.unlock(); }
            System.out.println("Condition.await(1ms) x" + n + " = " + ms(t) + " ms");
            ReferenceQueue<Object> q = new ReferenceQueue<>();
            t = System.nanoTime();
            for (int i = 0; i < n; i++) q.remove(1);
            System.out.println("RQ.remove(1) x" + n + " = " + ms(t) + " ms");
            t = System.nanoTime();
            for (int i = 0; i < n; i++) Thread.sleep(1);
            System.out.println("Thread.sleep(1) x" + n + " = " + ms(t) + " ms");
        }
    }
}
