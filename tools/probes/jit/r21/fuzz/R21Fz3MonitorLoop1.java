// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13, lane fuzz3: monitor shapes that the round's
// `monitor_balance` work (foreign-hold proof predicate, operand alias tracking
// through copies and joins, sparse lock-carrying slots, budget refusals) must
// compile -- or refuse -- without leaking or double-releasing a monitor.
//
// Shapes, each a separate method run cold, hot (OSR in its own loop, then a
// full compile on the second call), and after a deopt trigger (a never-loaded
// Worker class reaches the call made while the monitors are held):
//   throwLoop  synchronized block in a loop; explicit throws and an implicit NPE
//              inside, caught outside the block;
//   nested     a -> b -> a (recursive) with a throw at depth 3, caught between
//              depth 1 and 2;
//   alias      the lock reaches monitorenter through a copy and through a join
//              (x = cond ? a : b), with an inner synchronized(a);
//   rotate     the lock changes every iteration (locks[i & 3]);
//   method     synchronized instance and static methods that throw;
//   handler    synchronized inside a catch handler inside a synchronized block,
//              rethrowing out of both;
//   waitIn     Object.wait(1) at recursion depth 2 (the recursion count must
//              come back);
//   imse       notify() outside the monitor (IllegalMonitorStateException text).
// After every shape: Thread.holdsLock for every lock (false), and another
// thread must acquire every lock (`free`; a leaked monitor prints `stuck`).
// A 4-thread row counts increments made under one shared lock through the
// throwing shape: the total is exact only with mutual exclusion.
//
// Run:
//   javac -d out R21Fz3MonitorLoop1.java
//   java -Xmx256m -cp out R21Fz3MonitorLoop1                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz3MonitorLoop1              # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz3MonitorLoop1
//   cratonvm --nojit -Xmx256m -cp out R21Fz3MonitorLoop1
// HotSpot expectation: every line identical on every arm.

public class R21Fz3MonitorLoop1 {
    static final Object A = new Object();
    static final Object B = new Object();
    static final Object[] LOCKS = {new Object(), new Object(), new Object(), new Object()};
    static final Object[] MAYBE_NULL = {new Object(), null};
    static int inside;
    static int caught;
    static int shared;

    interface Worker {
        int work(int i);
    }

    static final class W1 implements Worker {
        public int work(int i) {
            return i & 7;
        }
    }

    /** Loaded first after the hot pass: the deopt trigger inside the locked regions. */
    static final class WLate implements Worker {
        public int work(int i) {
            if (i % 5 == 0) {
                throw new UnsupportedOperationException("late " + i);
            }
            return (i & 3) + 1;
        }
    }

    static Worker worker = new W1();

    static int throwLoop(Object lock, int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            try {
                synchronized (lock) {
                    inside += worker.work(i);
                    if (i % 7 == 0) {
                        throw new IllegalStateException("t" + i);
                    }
                    if (i % 11 == 0) {
                        inside += MAYBE_NULL[i & 1].hashCode() & 0;
                    }
                    inside++;
                }
            } catch (RuntimeException e) {
                c++;
            }
        }
        return c;
    }

    static int nested(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            synchronized (A) {
                try {
                    synchronized (B) {
                        synchronized (A) {
                            inside += worker.work(i);
                            if (i % 5 == 0) {
                                throw new IllegalArgumentException("n" + i);
                            }
                        }
                        inside++;
                    }
                } catch (RuntimeException e) {
                    c++;
                    if (!Thread.holdsLock(A) || Thread.holdsLock(B)) {
                        c += 1_000_000;
                    }
                }
            }
        }
        return c;
    }

    static int alias(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            Object x = (i & 1) == 0 ? A : B;
            Object y = x;
            try {
                synchronized (y) {
                    Object z = A;
                    synchronized (z) {
                        inside += worker.work(i);
                        if (i % 3 == 0) {
                            throw new IllegalStateException("a" + i);
                        }
                    }
                    if (i % 4 == 1) {
                        throw new IllegalStateException("b" + i);
                    }
                }
            } catch (RuntimeException e) {
                c++;
            }
        }
        return c;
    }

    static int rotate(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            Object l = LOCKS[i & 3];
            try {
                synchronized (l) {
                    inside += worker.work(i);
                    if ((i & 3) == 2 && (i & 8) == 0) {
                        throw new IllegalStateException("r" + i);
                    }
                }
            } catch (RuntimeException e) {
                c++;
            }
        }
        return c;
    }

    int field;

    synchronized int syncInst(int i) {
        field += worker.work(i);
        if (i % 9 == 0) {
            throw new IllegalStateException("m" + i);
        }
        return field;
    }

    static synchronized int syncStatic(int i) {
        inside += worker.work(i);
        if (i % 6 == 0) {
            throw new IllegalArgumentException("s" + i);
        }
        return inside;
    }

    static final R21Fz3MonitorLoop1 INST = new R21Fz3MonitorLoop1();

    static int method(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            try {
                INST.syncInst(i);
            } catch (RuntimeException e) {
                c++;
            }
            try {
                syncStatic(i);
            } catch (RuntimeException e) {
                c++;
            }
        }
        return c;
    }

    static int handler(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            try {
                synchronized (A) {
                    try {
                        inside += worker.work(i);
                        if (i % 4 == 0) {
                            throw new IllegalStateException("h" + i);
                        }
                    } catch (IllegalStateException e) {
                        synchronized (B) {
                            inside++;
                            if (i % 8 == 0) {
                                throw new IllegalArgumentException("hh" + i, e);
                            }
                        }
                    }
                }
            } catch (IllegalArgumentException e) {
                c++;
            } catch (RuntimeException e) {
                c += 1000;
            }
        }
        return c;
    }

    static String waitIn() throws InterruptedException {
        StringBuilder sb = new StringBuilder();
        synchronized (A) {
            synchronized (A) {
                A.wait(1);
                sb.append(Thread.holdsLock(A));
            }
            sb.append(',').append(Thread.holdsLock(A));
        }
        sb.append(',').append(Thread.holdsLock(A));
        return sb.toString();
    }

    static String imse() {
        try {
            B.notify();
            return "none";
        } catch (IllegalMonitorStateException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static String state() throws InterruptedException {
        Object[] all = {A, B, LOCKS[0], LOCKS[1], LOCKS[2], LOCKS[3], INST, R21Fz3MonitorLoop1.class};
        StringBuilder held = new StringBuilder();
        for (Object o : all) {
            held.append(Thread.holdsLock(o) ? 'H' : '-');
        }
        final boolean[] got = new boolean[all.length];
        Thread t = new Thread(() -> {
            for (int i = 0; i < all.length; i++) {
                synchronized (all[i]) {
                    got[i] = true;
                }
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(5000);
        StringBuilder free = new StringBuilder();
        synchronized (got) {
            for (boolean g : got) {
                free.append(g ? 'F' : 'X');
            }
        }
        return "held=" + held + " free=" + (t.isAlive() ? "stuck:" : "") + free;
    }

    static String all(String tag, int n) throws InterruptedException {
        inside = 0;
        INST.field = 0;
        int a = throwLoop(LOCKS[0], n);
        int b = nested(n);
        int c = alias(n);
        int d = rotate(n);
        int e = method(n);
        int f = handler(n);
        return tag + " throwLoop=" + a + " nested=" + b + " alias=" + c + " rotate=" + d + " method=" + e
                + " handler=" + f + " inside=" + inside + " field=" + INST.field + " " + state();
    }

    static int threaded(int perThread) throws InterruptedException {
        shared = 0;
        Thread[] ts = new Thread[4];
        for (int k = 0; k < ts.length; k++) {
            ts[k] = new Thread(() -> {
                for (int i = 0; i < perThread; i++) {
                    try {
                        synchronized (B) {
                            shared++;
                            if (i % 13 == 0) {
                                throw new IllegalStateException();
                            }
                        }
                    } catch (IllegalStateException e) {
                        // counted by the increment above
                    }
                }
            });
            ts[k].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        synchronized (B) {
            return shared;
        }
    }

    public static void main(String[] args) throws InterruptedException {
        System.out.println(all("cold", 200));
        System.out.println(all("osr", 60_000));
        System.out.println(all("hot", 60_000));
        System.out.println("wait " + waitIn() + " " + state());
        System.out.println("imse " + imse());
        worker = new WLate();
        System.out.println(all("late", 60_000));
        System.out.println(all("late-again", 60_000));
        System.out.println("threaded " + threaded(50_000) + " " + state());
        System.out.println("wait " + waitIn() + " " + state());
        System.out.println("imse " + imse());
    }
}
