import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
public class Sbr2W9Small2Wait {
    static void p(String k, Object v) { System.out.println(k + " = " + v); }
    static long ms(long t0) { return (System.nanoTime() - t0) / 1_000_000; }
    public static void main(String[] a) throws Throwable {
        // 1: notify ends a timed wait early
        {
            Object o = new Object(); AtomicLong el = new AtomicLong(-1); CountDownLatch in = new CountDownLatch(1);
            Thread w = new Thread(() -> { synchronized (o) { in.countDown(); long t = System.nanoTime(); try { o.wait(5000); } catch (InterruptedException e) {} el.set(ms(t)); } });
            w.start(); in.await(); Thread.sleep(50); synchronized (o) { o.notify(); } w.join();
            p("notify early", el.get() < 2000);
        }
        // 2: notifyAll wakes all timed waiters
        {
            Object o = new Object(); AtomicInteger woke = new AtomicInteger(); CountDownLatch in = new CountDownLatch(3);
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < 3; i++) { Thread w = new Thread(() -> { synchronized (o) { in.countDown(); long t = System.nanoTime(); try { o.wait(5000); } catch (InterruptedException e) {} if (ms(t) < 3000) woke.incrementAndGet(); } }); ts.add(w); w.start(); }
            in.await(); Thread.sleep(100); synchronized (o) { o.notifyAll(); } for (Thread t : ts) t.join();
            p("notifyAll early", woke.get());
        }
        // 3: interrupt ends a timed wait with InterruptedException, flag cleared
        {
            Object o = new Object(); AtomicReference<String> r = new AtomicReference<>(); CountDownLatch in = new CountDownLatch(1);
            Thread w = new Thread(() -> { synchronized (o) { in.countDown(); long t = System.nanoTime(); try { o.wait(5000); r.set("normal"); } catch (InterruptedException e) { r.set("IE flag=" + Thread.currentThread().isInterrupted() + " early=" + (ms(t) < 3000)); } } });
            w.start(); in.await(); Thread.sleep(50); w.interrupt(); w.join();
            p("interrupt", r.get());
        }
        // 4: timeout accuracy
        {
            Object o = new Object(); long worst = 0, best = Long.MAX_VALUE;
            synchronized (o) { for (int i = 0; i < 5; i++) { long t = System.nanoTime(); o.wait(30); long e = (System.nanoTime() - t) / 1_000_000; worst = Math.max(worst, e); best = Math.min(best, e); } }
            p("timeout >= 29ms", best >= 29);
            p("timeout < 120ms", worst < 120);
        }
        // 5: notify() wakes exactly one of two waiters
        {
            Object o = new Object(); AtomicInteger early = new AtomicInteger(); CountDownLatch in = new CountDownLatch(2);
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < 2; i++) { Thread w = new Thread(() -> { synchronized (o) { in.countDown(); long t = System.nanoTime(); try { o.wait(1200); } catch (InterruptedException e) {} if (ms(t) < 800) early.incrementAndGet(); } }); ts.add(w); w.start(); }
            in.await(); Thread.sleep(100); synchronized (o) { o.notify(); } for (Thread t : ts) t.join();
            p("notify one of two", early.get());
        }
        // 6: join(ms) returns when the thread ends
        {
            Thread t = new Thread(() -> { try { Thread.sleep(30); } catch (InterruptedException e) {} });
            t.start(); long s = System.nanoTime(); t.join(5000); p("join early", ms(s) < 2000 && !t.isAlive());
        }
        // 7: pre-set interrupt flag
        {
            Object o = new Object(); Thread.currentThread().interrupt();
            try { synchronized (o) { o.wait(100); } p("preset", "no exception"); } catch (InterruptedException e) { p("preset", "IE flag=" + Thread.currentThread().isInterrupted()); }
        }
        // 8: wait(1) loop
        {
            Object o = new Object(); long t = System.nanoTime();
            synchronized (o) { for (int i = 0; i < 40; i++) o.wait(1); }
            p("40 x wait(1) < 3000ms", ms(t) < 3000);
        }
        // 9: ping-pong with timed waits
        {
            Object o = new Object(); int[] turn = {0}; int N = 200;
            Thread b = new Thread(() -> { synchronized (o) { for (int i = 0; i < N; i++) { while (turn[0] != 1) { try { o.wait(20); } catch (InterruptedException e) { return; } } turn[0] = 0; o.notifyAll(); } } });
            b.start(); long t = System.nanoTime();
            synchronized (o) { for (int i = 0; i < N; i++) { turn[0] = 1; o.notifyAll(); while (turn[0] != 0) o.wait(20); } }
            b.join(); p("pingpong done", true); p("pingpong fast", ms(t) < 3000);
        }
        // 10: wait(0) untimed still wakes on notify
        {
            Object o = new Object(); CountDownLatch in = new CountDownLatch(1); AtomicBoolean ok = new AtomicBoolean();
            Thread w = new Thread(() -> { synchronized (o) { in.countDown(); try { o.wait(0); ok.set(true); } catch (InterruptedException e) {} } });
            w.start(); in.await(); Thread.sleep(50); synchronized (o) { o.notify(); } w.join(); p("untimed notify", ok.get());
        }
        // 11: Condition / BlockingQueue timed poll accuracy and wake
        {
            BlockingQueue<Integer> q = new LinkedBlockingQueue<>(); long t = System.nanoTime();
            Integer x = q.poll(30, TimeUnit.MILLISECONDS); p("poll timeout", x == null && ms(t) >= 29);
            new Thread(() -> { try { Thread.sleep(40); q.put(7); } catch (InterruptedException e) {} }).start();
            p("poll wake", q.poll(5, TimeUnit.SECONDS));
        }
    }
}
