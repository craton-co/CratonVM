// JIT round 18 wave 11 (lane osrfollow): loops the optimizing OSR door
// compiles, under the narrowed opaque policy and the fused class-test branch.
// Every line is deterministic and compared with HotSpot's.
public final class OsrFollowProbe {
    interface Shape { int area(); }
    static final class Sq implements Shape { final int s; Sq(int s) { this.s = s; } public int area() { return s * s; } }
    static final class Re implements Shape { final int a, b; Re(int a, int b) { this.a = a; this.b = b; } public int area() { return a * b; } }
    static final class Tr implements Shape { final int b, h; Tr(int b, int h) { this.b = b; this.h = h; } public int area() { return b * h / 2; } }

    // Exact guards only: the strict guard fact holds.
    static long exact(int n) {
        int[] a = new int[8];
        for (int k = 0; k < a.length; k++) a[k] = k * 3 + 1;
        long s = 0;
        for (int i = 0; i < n; i++) s += a[i & 7] ^ i;
        return s;
    }

    // A pruned arm that fires after the OSR entry.
    static long lateBranch(int n) {
        long s = 0;
        int bias = 1;
        for (int i = 0; i < n; i++) {
            if (i == n - 1000) bias = 1000;
            s += (i & 15) * bias;
        }
        return s;
    }

    // A catch that continues the loop.
    static long handlerLoop(int n) {
        int[] d = { 1, 2, 3, 0, 5, 6, 7 };
        long s = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += (i & 1023) / d[i % 7];
            } catch (ArithmeticException e) {
                s -= 3;
            }
        }
        return s;
    }

    // A catch that rewrites a local the loop reads.
    static long handlerRewrites(int n) {
        int[] d = { 4, 0, 2, 8 };
        long s = 0;
        int k = 1;
        for (int i = 0; i < n; i++) {
            try {
                s += (i & 255) * k / d[i & 3];
            } catch (ArithmeticException e) {
                k = (k % 5) + 1;
            }
        }
        return s + k;
    }

    // Three receiver classes, then a fourth instance class seen only late.
    static long polyLate(int n) {
        Shape[] sh = { new Sq(3), new Re(2, 5), new Tr(4, 6), new Sq(7) };
        Shape late = new Tr(10, 10);
        long s = 0;
        for (int i = 0; i < n; i++) {
            Shape x = (i >= n - 500) ? late : sh[i & 3];
            s += x.area();
        }
        return s;
    }

    // Three classes at 50/25/25 % (R16Bench poly).
    static long poly3(int n) {
        Shape[] sh = { new Sq(3), new Re(2, 5), new Tr(4, 6), new Sq(7) };
        long s = 0;
        for (int i = 0; i < n; i++) s += sh[i & 3].area();
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 3_000_000;
        System.out.println("exact " + exact(n));
        System.out.println("lateBranch " + lateBranch(n));
        // The handler rows throw every 7th / 4th iteration; ~25 us per caught
        // exception here, so they run a fifteenth of the others' size.
        int h = Math.max(10_000, n / 15);
        System.out.println("handlerLoop " + handlerLoop(h));
        System.out.println("handlerRewrites " + handlerRewrites(h));
        System.out.println("polyLate " + polyLate(n));
        System.out.println("poly3 " + poly3(n));
        System.out.println("OSR-FOLLOW-OK");
    }
}
