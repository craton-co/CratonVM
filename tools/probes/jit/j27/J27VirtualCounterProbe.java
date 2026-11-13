// j27 wave 3 (lane DISPATCH2): a virtual call-heavy kernel over the blind
// virtual dispatch arm (megamorphic sites, so no inline cache), whose
// VIRTUAL_DISPATCH_COUNTER crossing now only PROBES the callee door under
// CRATONVM_JIT_VIRTUAL_DISPATCH_COUNTER_PROBE_ONLY (default on; =0 restores the
// door's defer/compile). The answers must not depend on the mode: an override
// per receiver, an inherited method (the declaring-class body), a synchronized
// override (the door's monitor filter) and a callee first reached only after
// the caller is hot.
// Page: docs/feature-designs/native-dispatch-architecture.md
//
// Expected HotSpot 25 output (facts only):
//   fact mega-sum 2000000
//   fact inherited-sum 1200000
//   fact sync-sum 1000000
//   fact sync-counter 200000
//   fact late-sum 1800000
//   fact mixed-sum 3000000
// -Dj27.timing=true adds `time mega-ns-per-call <n>` (not compared).
public class J27VirtualCounterProbe {
    public abstract static class Shape {
        public abstract int v(int x);

        public int inherited(int x) {
            return x + 1;
        }
    }

    public static final class A extends Shape {
        public int v(int x) {
            return x + 1;
        }
    }

    public static final class B extends Shape {
        public int v(int x) {
            return x + 2;
        }

        public int inherited(int x) {
            return x + 3;
        }
    }

    public static final class C extends Shape {
        public int v(int x) {
            return x + 3;
        }
    }

    public static final class D extends Shape {
        public int v(int x) {
            return x + 4;
        }
    }

    public static class Locked {
        int hits;

        public synchronized int bump(int x) {
            hits++;
            return x + 5;
        }
    }

    public static final class Locked2 extends Locked {
        public synchronized int bump(int x) {
            hits++;
            return x + 5;
        }
    }

    public static class Late {
        public int late(int x) {
            return x + 9;
        }
    }

    public static final class Late2 extends Late {
        public int late(int x) {
            return x + 9;
        }
    }

    static final int N = 200_000;

    // One round: A..D answer 1, 2, 3, 4 (10 per round).
    static long mega(Shape[] s, int rounds) {
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < s.length; i++) {
                sum += s[i].v(0);
            }
        }
        return sum;
    }

    static long inherited(Shape[] s, int rounds) {
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < s.length; i++) {
                sum += s[i].inherited(0);
            }
        }
        return sum;
    }

    static long sync(Locked[] l, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += l[i & 1].bump(0);
        }
        return sum;
    }

    static long late(Late[] l, int n, int lateFrom) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            // The second receiver class first appears after the loop is hot.
            sum += l[i < lateFrom ? 0 : 1].late(0);
        }
        return sum;
    }

    public static void main(String[] args) {
        Shape[] shapes = {new A(), new B(), new C(), new D()};
        // Warm-up: same answers each pass; only the last is printed.
        long megaSum = 0;
        for (int w = 0; w < 5; w++) {
            megaSum = mega(shapes, N);
        }
        System.out.println("fact mega-sum " + megaSum);

        long inh = 0;
        for (int w = 0; w < 5; w++) {
            inh = inherited(shapes, N);
        }
        // A, C, D inherit (answer 1), B overrides (answer 3): 6 per round.
        System.out.println("fact inherited-sum " + inh);

        Locked[] locks = {new Locked(), new Locked2()};
        long syncSum = 0;
        for (int w = 0; w < 5; w++) {
            locks[0].hits = 0;
            locks[1].hits = 0;
            syncSum = sync(locks, N);
        }
        System.out.println("fact sync-sum " + syncSum);
        System.out.println("fact sync-counter " + (locks[0].hits + locks[1].hits));

        Late[] lates = {new Late(), new Late2()};
        long lateSum = 0;
        for (int w = 0; w < 5; w++) {
            lateSum = late(lates, N, w == 4 ? N / 2 : N);
        }
        System.out.println("fact late-sum " + lateSum);

        long mixed = mega(shapes, N / 2) + inherited(shapes, N / 2)
                + sync(locks, N / 2) + late(lates, N / 2, 0);
        System.out.println("fact mixed-sum " + mixed);

        if (Boolean.getBoolean("j27.timing")) {
            long t0 = System.nanoTime();
            long s = mega(shapes, N);
            long dt = System.nanoTime() - t0;
            System.out.println("time mega-ns-per-call " + (dt / (4L * N)) + " (" + s + ")");
        }
    }
}
