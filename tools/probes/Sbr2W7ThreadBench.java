import java.security.MessageDigest;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.locks.*;
import javax.crypto.Cipher;
import javax.crypto.Mac;

/**
 * SBR round 2, wave 7, lane THREAD: A/B numbers. Each row runs `rounds` timed
 * rounds and prints min / median / max in ns per operation.
 *
 *   java Sbr2W7ThreadBench [rounds]
 */
public class Sbr2W7ThreadBench {
    static final ThreadLocal<Integer> TL = new ThreadLocal<>();
    static final ThreadLocal<Integer> TL2 = ThreadLocal.withInitial(() -> 1);
    static final InheritableThreadLocal<String> ITL = new InheritableThreadLocal<>();
    static volatile Object sink;

    interface Body {
        void run(int n) throws Exception;
    }

    static String filter = ".*";

    static void bench(String name, int n, int rounds, Body b) throws Exception {
        if (!name.matches(filter)) {
            return;
        }
        b.run(Math.min(n, 2000));
        b.run(Math.min(n, 2000));
        double[] t = new double[rounds];
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            b.run(n);
            t[r] = (System.nanoTime() - t0) / (double) n;
        }
        Arrays.sort(t);
        System.out.printf("%-28s min %10.1f  med %10.1f  max %10.1f ns/op%n", name, t[0], t[rounds / 2], t[rounds - 1]);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 7;
        // optional second argument: a regex the row names must match (the
        // control build's `new Thread(r)` rows take minutes at the default n)
        if (args.length > 1) {
            filter = args[1];
        }
        TL.set(5);
        ITL.set("x");
        bench("tl.get (set)", 200_000, rounds, n -> {
            int s = 0;
            for (int i = 0; i < n; i++) {
                s += TL.get();
            }
            sink = s;
        });
        bench("tl.get (withInitial)", 200_000, rounds, n -> {
            int s = 0;
            for (int i = 0; i < n; i++) {
                s += TL2.get();
            }
            sink = s;
        });
        bench("tl.set", 200_000, rounds, n -> {
            for (int i = 0; i < n; i++) {
                TL.set(i);
            }
        });
        bench("tl.set+remove", 100_000, rounds, n -> {
            for (int i = 0; i < n; i++) {
                TL.set(i);
                TL.remove();
            }
            TL.set(5);
        });
        bench("itl.get", 200_000, rounds, n -> {
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = ITL.get();
            }
            sink = s;
        });
        bench("new ThreadLocal+set+get", 50_000, rounds, n -> {
            Object s = null;
            for (int i = 0; i < n; i++) {
                ThreadLocal<Integer> t = new ThreadLocal<>();
                t.set(i);
                s = t.get();
            }
            sink = s;
        });
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        bench("rrwl read lock/unlock", 200_000, rounds, n -> {
            for (int i = 0; i < n; i++) {
                rw.readLock().lock();
                rw.readLock().unlock();
            }
        });
        bench("Thread.currentThread", 500_000, rounds, n -> {
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = Thread.currentThread();
            }
            sink = s;
        });
        bench("isAlive (main)", 500_000, rounds, n -> {
            boolean s = false;
            Thread m = Thread.currentThread();
            for (int i = 0; i < n; i++) {
                s ^= m.isAlive();
            }
            sink = s;
        });
        bench("getState (main)", 200_000, rounds, n -> {
            Object s = null;
            Thread m = Thread.currentThread();
            for (int i = 0; i < n; i++) {
                s = m.getState();
            }
            sink = s;
        });
        bench("new Thread(r)", 20_000, rounds, n -> {
            Runnable r = () -> {};
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = new Thread(r);
            }
            sink = s;
        });
        bench("new Thread(r) w/ ITL set", 20_000, rounds, n -> {
            Runnable r = () -> {};
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = new Thread(r);
            }
            sink = s;
        });
        bench("thread start+join", 1_500, rounds, n -> {
            Runnable r = () -> {};
            for (int i = 0; i < n; i++) {
                Thread t = new Thread(r);
                t.start();
                t.join();
            }
        });
        bench("virtual start+join", 1_500, rounds, n -> {
            Runnable r = () -> {};
            for (int i = 0; i < n; i++) {
                Thread t = Thread.ofVirtual().unstarted(r);
                t.start();
                t.join();
            }
        });
        ArrayBlockingQueue<Integer> a = new ArrayBlockingQueue<>(1);
        ArrayBlockingQueue<Integer> b = new ArrayBlockingQueue<>(1);
        bench("ABQ ping-pong (park/unpark)", 5_000, rounds, n -> {
            Thread echo = new Thread(() -> {
                try {
                    for (int i = 0; i < n; i++) {
                        b.put(a.take());
                    }
                } catch (InterruptedException e) {
                }
            });
            echo.start();
            for (int i = 0; i < n; i++) {
                a.put(i);
                b.take();
            }
            echo.join();
        });
        bench("Object.wait(1)", 300, rounds, n -> {
            Object o = new Object();
            for (int i = 0; i < n; i++) {
                synchronized (o) {
                    o.wait(1);
                }
            }
        });
        bench("Thread.sleep(0)", 20_000, rounds, n -> {
            for (int i = 0; i < n; i++) {
                Thread.sleep(0);
            }
        });
        bench("MessageDigest.getInstance", 2_000, rounds, n -> {
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = MessageDigest.getInstance("SHA-256");
            }
            sink = s;
        });
        bench("Mac.getInstance", 1_000, rounds, n -> {
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = Mac.getInstance("HmacSHA256");
            }
            sink = s;
        });
        bench("Cipher.getInstance", 500, rounds, n -> {
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = Cipher.getInstance("AES/GCM/NoPadding");
            }
            sink = s;
        });
        bench("Cipher.getInstance(CBC)", 500, rounds, n -> {
            Object s = null;
            for (int i = 0; i < n; i++) {
                s = Cipher.getInstance("AES/CBC/PKCS5Padding");
            }
            sink = s;
        });
    }
}
