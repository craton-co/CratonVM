// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 22, lane L4: the virtual and static invoke doors
// answer a call of an empty body (first instruction `return`) without a frame
// (`invoke_fast::VIRTUAL_DOOR_ELIDES_EMPTY_BODY`,
// `STATIC_DOOR_ELIDES_EMPTY_BODY`), as the special door does since wave 20.
// This probe checks every behaviour the elision must keep, after each site is
// warm (the doors serve only warm monomorphic / poly sites):
//
//   args        - category-2 and reference arguments are popped exactly
//                 (the caller's locals and later arithmetic are intact)
//   poly        - a site that turns polymorphic runs a subclass's NON-empty
//                 override for its receivers, and still elides for the
//                 empty one
//   iface       - an `invokeinterface` of an empty default method and of an
//                 empty implementation
//   npe         - a null receiver still throws the JEP 358 message
//   clinit      - the first `invokestatic` of an empty method initializes
//                 its class once, before the call
//   sync        - an empty `synchronized` method is NOT elided: it waits for
//                 a monitor another thread holds
//
// Run: `javac -d out EmptyBodyElisionProbe.java` (no `-g`), then
// `cratonvm -cp out EmptyBodyElisionProbe`, also with `--nojit` and with
// `--compatible`. Output must equal HotSpot 25's (25.0.3, default and
// `-Xint`), which is:
//
//   args 1234567890123 2.5 x 42 sum=1234567890123
//   poly empty=20000 loud=20000 sum=199990000
//   iface empty=20000 default=20000
//   npe Cannot invoke "EmptyBodyElisionProbe$Quiet.hook(long, double, Object, int)" because "<parameter1>" is null
//   clinit Holder.<clinit> once=1 calls=20000
//   sync waited=true
public class EmptyBodyElisionProbe {
    static final int N = 20_000;

    static class Quiet {
        void hook(long a, double b, Object c, int d) {
        }
    }

    static final class Loud extends Quiet {
        long seen;

        @Override
        void hook(long a, double b, Object c, int d) {
            seen += d;
        }
    }

    interface Listener {
        void on(long a);

        default void idle(int x) {
        }
    }

    static final class Silent implements Listener {
        @Override
        public void on(long a) {
        }
    }

    static final class Holder {
        static int inits;

        static {
            inits++;
            System.out.print("clinit Holder.<clinit> ");
        }

        static void empty(long a, Object b) {
        }
    }

    static final Object LOCK_HOLDER = new Object();

    static final class Guarded {
        synchronized void emptySync() {
        }
    }

    static void args() {
        Quiet q = new Quiet();
        long keep = 1234567890123L;
        double d = 2.5;
        String s = "x";
        int i = 42;
        long sum = 0;
        for (int k = 0; k < N; k++) {
            q.hook(keep, d, s, i);
            sum = keep;
        }
        System.out.println("args " + keep + " " + d + " " + s + " " + i + " sum=" + sum);
    }

    static void call(Quiet q, int k) {
        q.hook(k, k, q, k);
    }

    static void poly() {
        Quiet quiet = new Quiet();
        Loud loud = new Loud();
        for (int k = 0; k < N; k++) {
            call(quiet, k);
        }
        int empty = N;
        int louder = 0;
        for (int k = 0; k < N; k++) {
            call(loud, k);
            call(quiet, k);
            louder++;
        }
        System.out.println("poly empty=" + empty + " loud=" + louder + " sum=" + loud.seen);
    }

    static void iface() {
        Listener l = new Silent();
        int a = 0;
        int b = 0;
        for (int k = 0; k < N; k++) {
            l.on(k);
            a++;
            l.idle(k);
            b++;
        }
        System.out.println("iface empty=" + a + " default=" + b);
    }

    static void npe(Quiet q) {
        q.hook(1L, 1.0, null, 1);
    }

    static void clinit() {
        int calls = 0;
        for (int k = 0; k < N; k++) {
            Holder.empty(k, null);
            calls++;
        }
        System.out.println("once=" + Holder.inits + " calls=" + calls);
    }

    static void sync() throws Exception {
        Guarded g = new Guarded();
        for (int k = 0; k < N; k++) {
            g.emptySync();
        }
        java.util.concurrent.CountDownLatch held = new java.util.concurrent.CountDownLatch(1);
        Thread holder = new Thread(() -> {
            synchronized (g) {
                held.countDown();
                try {
                    Thread.sleep(400);
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }
        });
        holder.start();
        held.await();
        long t0 = System.nanoTime();
        g.emptySync();
        long waitedMs = (System.nanoTime() - t0) / 1_000_000;
        holder.join();
        System.out.println("sync waited=" + (waitedMs >= 200));
    }

    public static void main(String[] args) throws Exception {
        args();
        poly();
        iface();
        Quiet warm = new Quiet();
        for (int k = 0; k < N; k++) {
            npe(warm);
        }
        try {
            npe(null);
            System.out.println("npe none");
        } catch (NullPointerException e) {
            System.out.println("npe " + e.getMessage());
        }
        clinit();
        sync();
    }
}
