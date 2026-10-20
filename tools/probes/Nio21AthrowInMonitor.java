import java.util.concurrent.ConcurrentHashMap;

/**
 * nio21 wave 18 lane L3: an {@code athrow} reached while the method holds a {@code synchronized}
 * block's lock deopts to the interpreter at the {@code athrow}
 * ({@code CRATONVM_JIT_IR_ATHROW_IN_MONITOR_TRAP}, default ON). Before this, such a method had no
 * optimizing body at all ({@code ConcurrentHashMap.putVal}'s "Recursive update" arm).
 *
 * <p>Conformance (default, prints answers only; must match HotSpot 25 byte for byte). Each method
 * is warmed on its non-throwing path, then made to throw. The probe prints the exception class and
 * message, the top stack frame, whether the lock is still held after the catch, whether another
 * thread can take the lock, and the state the block committed before the throw. The
 * {@code ConcurrentHashMap} rows warm {@code put} hot, then call {@code put} of the same key from
 * inside {@code computeIfAbsent}'s mapping function, which reaches {@code putVal}'s "Recursive
 * update" throw under the bin's lock.
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21AthrowInMonitor.java
 *   java -cp /tmp/p21 Nio21AthrowInMonitor                                # HotSpot facts
 *   cratonvm -cp /tmp/p21 Nio21AthrowInMonitor                            # --jdk-only
 *   cratonvm --compatible -cp /tmp/p21 Nio21AthrowInMonitor
 *   CRATONVM_JIT_IR_ATHROW_IN_MONITOR_TRAP=0 cratonvm -cp /tmp/p21 Nio21AthrowInMonitor
 *   CRATONVM_DBG=ir-bailout cratonvm -cp /tmp/p21 Nio21AthrowInMonitor 2>&1 | grep 'athrow inside'
 * </pre>
 */
public class Nio21AthrowInMonitor {
    static final Object LOCK = new Object();
    static int counter;
    static final int WARM = Integer.getInteger("nio21.warm", 200_000);

    static int step(int i, int bad) {
        synchronized (LOCK) {
            counter += i;
            if (i == bad) {
                throw new IllegalStateException("Recursive update " + i);
            }
            return counter;
        }
    }

    /** The CHM shape: a loop around a lock on a per-iteration object, the throw inside it. */
    static int loopStep(Object[] locks, int n, int bad) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            Object f = locks[i & (locks.length - 1)];
            synchronized (f) {
                s += i;
                if (s == bad) {
                    throw new IllegalArgumentException("bad " + s);
                }
            }
        }
        return s;
    }

    /** Nested locks: the throw holds two. */
    static int nested(Object a, Object b, int x) {
        synchronized (a) {
            synchronized (b) {
                counter++;
                if (x < 0) {
                    throw new ArithmeticException("negative " + x);
                }
            }
            return counter;
        }
    }

    static void report(String what, Throwable e, Object... locks) {
        StackTraceElement top = e.getStackTrace()[0];
        StringBuilder held = new StringBuilder();
        for (Object l : locks) {
            held.append(Thread.holdsLock(l) ? 'H' : '-');
        }
        System.out.println(what + " " + e.getClass().getName() + ": " + e.getMessage()
                + " top=" + top.getClassName() + "." + top.getMethodName()
                + " held=" + held);
    }

    static boolean otherThreadTakes(Object lock) throws InterruptedException {
        boolean[] ok = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (lock) {
                ok[0] = true;
            }
        });
        t.start();
        t.join(10_000);
        return ok[0];
    }

    public static void main(String[] args) throws Exception {
        long sum = 0;
        Object[] locks = new Object[4];
        for (int j = 0; j < locks.length; j++) {
            locks[j] = new Object();
        }
        Object a = new Object();
        Object b = new Object();
        for (int i = 0; i < WARM; i++) {
            sum += step(i & 1023, -1);
            sum += loopStep(locks, 16, -1);
            sum += nested(a, b, i);
        }
        System.out.println("warm " + sum + " counter=" + counter);

        for (int k = 0; k < 6; k++) {
            int before = counter;
            try {
                step(7, 7);
                System.out.println("step: no throw");
            } catch (IllegalStateException e) {
                report("step", e, LOCK);
            }
            System.out.println("step committed " + (counter - before));
        }
        System.out.println("step other thread " + otherThreadTakes(LOCK));

        int[] tri = {28, 36, 45, 55, 66, 78};
        for (int k = 0; k < 6; k++) {
            try {
                loopStep(locks, 16, tri[k]);
                System.out.println("loopStep: no throw " + k);
            } catch (IllegalArgumentException e) {
                report("loopStep", e, locks);
            }
        }
        System.out.println("loopStep other thread " + otherThreadTakes(locks[0])
                + " " + otherThreadTakes(locks[3]));

        for (int k = 0; k < 6; k++) {
            int before = counter;
            try {
                nested(a, b, -k - 1);
                System.out.println("nested: no throw");
            } catch (ArithmeticException e) {
                report("nested", e, a, b);
            }
            System.out.println("nested committed " + (counter - before));
        }
        System.out.println("nested other thread " + otherThreadTakes(a) + " " + otherThreadTakes(b));

        ConcurrentHashMap<Integer, Integer> map = new ConcurrentHashMap<>();
        long h = 0;
        for (int i = 0; i < WARM; i++) {
            map.put(i & 4095, i);
            h += map.get(i & 4095);
        }
        System.out.println("chm warm " + h + " size=" + map.size());
        for (int k = 0; k < 4; k++) {
            ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
            Integer key = 1000 + k;
            try {
                m.computeIfAbsent(key, kk -> {
                    m.put(kk, 1);
                    return 2;
                });
                System.out.println("chm: no throw " + m);
            } catch (IllegalStateException e) {
                report("chm", e);
                System.out.println("chm after " + m + " size=" + m.size());
            }
            m.put(key, 3);
            System.out.println("chm reusable " + m);
        }
    }
}
