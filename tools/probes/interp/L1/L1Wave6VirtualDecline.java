// Interpreter round i1, wave 6, lane L1 — `invokevirtual` sites the virtual
// fast door declines or hands off.
//
// Wave 6 removed the dispatch loop's second (non-virtual) door from the
// `invokevirtual` arm: after a virtual-door decline it could only re-probe the
// same inline-cache key and decline again, and a private (nestmate) target is
// served by the virtual door's hand-off to the same body. This probe drives
// the shapes that path covers and checks the answers are unchanged.
//
// stdout: deterministic, must match HotSpot 25 exactly (`--compatible`, with
// and without `--nojit`):
//   mega       a site with five receiver classes (the door declines most calls)
//   private    a private instance method called through invokevirtual
//   privsync   the same, synchronized
//   nullrecv   a null receiver at a warmed private-call site (NPE from the
//              general path)
//
// stderr: ns/call per shape, for an interleaved before/after comparison
// (`--nojit`, take medians of several runs: in-JVM timings here swing).
public class L1Wave6VirtualDecline {
    static abstract class Shape {
        abstract int area(int k);
    }

    static final class A extends Shape {
        int area(int k) { return k + 1; }
    }

    static final class B extends Shape {
        int area(int k) { return k * 2; }
    }

    static final class C extends Shape {
        int area(int k) { return k ^ 5; }
    }

    static final class D extends Shape {
        int area(int k) { return k - 3; }
    }

    static final class E extends Shape {
        int area(int k) { return k >>> 1; }
    }

    private int bias = 7;

    private int priv(int k) {
        return k + bias;
    }

    private synchronized int privSync(int k) {
        return k - bias;
    }

    static long mega(Shape[] shapes, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += shapes[i % shapes.length].area(i);
        }
        return sum;
    }

    long privLoop(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += priv(i);
        }
        return sum;
    }

    long privSyncLoop(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += privSync(i);
        }
        return sum;
    }

    static int callPriv(L1Wave6VirtualDecline o, int k) {
        return o.priv(k);
    }

    public static void main(String[] args) {
        Shape[] shapes = {new A(), new B(), new C(), new D(), new E()};
        L1Wave6VirtualDecline self = new L1Wave6VirtualDecline();
        int n = 200_000;
        for (int round = 0; round < 3; round++) {
            long t0 = System.nanoTime();
            long m = mega(shapes, n);
            long t1 = System.nanoTime();
            long p = self.privLoop(n);
            long t2 = System.nanoTime();
            long s = self.privSyncLoop(n);
            long t3 = System.nanoTime();
            System.out.println("mega " + m);
            System.out.println("private " + p);
            System.out.println("privsync " + s);
            System.err.printf(
                    "round %d: mega %.1f ns/call, private %.1f ns/call, privsync %.1f ns/call%n",
                    round,
                    (t1 - t0) / (double) n,
                    (t2 - t1) / (double) n,
                    (t3 - t2) / (double) n);
        }
        int warm = 0;
        for (int i = 0; i < 1000; i++) {
            warm += callPriv(self, i);
        }
        System.out.println("warm " + warm);
        try {
            callPriv(null, 1);
            System.out.println("nullrecv no exception");
        } catch (NullPointerException e) {
            System.out.println("nullrecv NullPointerException");
        }
    }
}
