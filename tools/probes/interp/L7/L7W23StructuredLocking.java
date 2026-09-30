// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L7: javac-shaped `synchronized` blocks
// across every way an interpreted frame's monitors can change hands, as the
// regression guard for the per-frame structured-locking record
// (`Frame::held_monitors`, `interpreter::held_monitors`; page
// `interpreter-L7-unstructured-locking-is-not-detected`).
//
// The record is authoritative for `monitorexit`: a record that lost an entry
// it should have kept would make a structured `monitorexit` throw
// IllegalMonitorStateException, and javac's catch-any handler covers its own
// `monitorexit`, so that failure is a HANG, not a wrong line. Every row here
// must therefore terminate and print HotSpot's answer:
//
//   osrInBlock     a hot loop inside a block (OSR enters with the lock held;
//                  the compiled body releases it)
//   osrReentrant   the same inside a callee that re-enters its caller's lock
//   osrThrow       a throw out of a hot loop inside a block, caught outside
//   osrInLoopBody  a block inside a hot loop body (OSR enters between blocks)
//   nested         two nested blocks around a hot loop
//   recursive      a recursive method taking the same lock at every depth
//   calleeThrows   a callee's exception unwinding through the block handler
//   waitNotify     wait/notify ping-pong inside blocks
//   contended      four threads incrementing under one lock
//   syncMethod     a synchronized method calling a block on the same lock
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W23StructuredLocking
// Also with the JIT and a low OSR threshold, where the OSR rows matter most.
//
// HotSpot 25 (-Xint and default) prints exactly:
//     osrInBlock 19999900000 held=false
//     osrReentrant 19999900000 inside=true held=false
//     osrThrow caught=boom 199999 held=false
//     osrInLoopBody 19999900000 held=false
//     nested 19999900000 a=false b=false
//     recursive 500 held=false
//     calleeThrows caught=callee held=false
//     waitNotify 2000
//     contended 400000
//     syncMethod 19999900000 held=false
public class L7W23StructuredLocking {
    static final int N = 200_000;
    static final Object LOCK = new Object();
    static final Object A = new Object();
    static final Object B = new Object();

    static long osrInBlock() {
        long sum = 0;
        synchronized (LOCK) {
            for (int i = 0; i < N; i++) {
                sum += i;
            }
        }
        return sum;
    }

    static long reentrantCallee() {
        long sum = 0;
        synchronized (LOCK) {
            for (int i = 0; i < N; i++) {
                sum += i;
            }
        }
        return sum;
    }

    static int osrThrow() {
        int i = 0;
        synchronized (LOCK) {
            for (; i < N; i++) {
                if (i == N - 1) {
                    throw new IllegalStateException("boom " + i);
                }
            }
        }
        return i;
    }

    static long osrInLoopBody() {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            synchronized (LOCK) {
                sum += i;
            }
        }
        return sum;
    }

    static long nested() {
        long sum = 0;
        synchronized (A) {
            synchronized (B) {
                for (int i = 0; i < N; i++) {
                    sum += i;
                }
            }
        }
        return sum;
    }

    static int recursive(int n) {
        synchronized (LOCK) {
            return n == 0 ? 0 : 1 + recursive(n - 1);
        }
    }

    static void thrower() {
        throw new IllegalArgumentException("callee");
    }

    static void calleeThrows() {
        synchronized (LOCK) {
            thrower();
        }
    }

    static int turn;

    static int waitNotify() throws Exception {
        Object m = new Object();
        int[] count = new int[1];
        Thread other = new Thread(() -> {
            for (int k = 0; k < 1000; k++) {
                synchronized (m) {
                    while (turn != 1) {
                        try {
                            m.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                    count[0]++;
                    turn = 0;
                    m.notifyAll();
                }
            }
        });
        other.start();
        for (int k = 0; k < 1000; k++) {
            synchronized (m) {
                while (turn != 0) {
                    m.wait();
                }
                count[0]++;
                turn = 1;
                m.notifyAll();
            }
        }
        other.join();
        return count[0];
    }

    static long counter;

    static long contended() throws Exception {
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> {
                for (int i = 0; i < 100_000; i++) {
                    synchronized (LOCK) {
                        counter++;
                    }
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        return counter;
    }

    static synchronized long syncMethod() {
        long sum = 0;
        synchronized (L7W23StructuredLocking.class) {
            for (int i = 0; i < N; i++) {
                sum += i;
            }
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        long r = osrInBlock();
        System.out.println("osrInBlock " + r + " held=" + Thread.holdsLock(LOCK));

        boolean inside;
        synchronized (LOCK) {
            r = reentrantCallee();
            inside = Thread.holdsLock(LOCK);
        }
        System.out.println("osrReentrant " + r + " inside=" + inside + " held=" + Thread.holdsLock(LOCK));

        try {
            osrThrow();
            System.out.println("osrThrow no throw");
        } catch (IllegalStateException e) {
            System.out.println("osrThrow caught=" + e.getMessage() + " held=" + Thread.holdsLock(LOCK));
        }

        r = osrInLoopBody();
        System.out.println("osrInLoopBody " + r + " held=" + Thread.holdsLock(LOCK));

        r = nested();
        System.out.println("nested " + r + " a=" + Thread.holdsLock(A) + " b=" + Thread.holdsLock(B));

        int d = recursive(500);
        System.out.println("recursive " + d + " held=" + Thread.holdsLock(LOCK));

        try {
            calleeThrows();
        } catch (IllegalArgumentException e) {
            System.out.println("calleeThrows caught=" + e.getMessage() + " held=" + Thread.holdsLock(LOCK));
        }

        System.out.println("waitNotify " + waitNotify());
        System.out.println("contended " + contended());

        r = syncMethod();
        System.out.println("syncMethod " + r + " held=" + Thread.holdsLock(L7W23StructuredLocking.class));
    }
}
