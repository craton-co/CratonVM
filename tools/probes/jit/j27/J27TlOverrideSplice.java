// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane TCARRAY): page
// docs/feature-designs/jit-ir-optimizations.md
// The SPLICED half: the ThreadLocal get()/set() calls sit in small leaves the caller
// splices, so their receiver profile is the callee's. Facts are deterministic; timing
// lines only with -Dj27.time=true.
public class J27TlOverrideSplice {
    static int gets;
    static int sets;

    static final class Over extends ThreadLocal<int[]> {
        @Override public int[] get() { gets++; return super.get(); }
        @Override public void set(int[] v) { sets++; super.set(v); }
    }

    static final ThreadLocal<int[]> OVER = new Over();
    static final ThreadLocal<int[]> PLAIN = new ThreadLocal<>();

    static int[] fetch(ThreadLocal<int[]> t) { return t.get(); }

    static void store(ThreadLocal<int[]> t, int[] v) { t.set(v); }

    static int overKernel(int i) {
        store(OVER, new int[] {i & 7});
        return fetch(OVER)[0];
    }

    static int plainKernel(int i) {
        storePlain(new int[] {i & 3});
        return fetchPlain()[0];
    }

    static int[] fetchPlain() { return PLAIN.get(); }

    static void storePlain(int[] v) { PLAIN.set(v); }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        long over = 0;
        long plain = 0;
        for (int i = 0; i < 400_000; i++) {
            over += overKernel(i);
            plain += plainKernel(i);
        }
        System.out.println("fact over-sum " + over);
        System.out.println("fact plain-sum " + plain);
        System.out.println("fact gets " + gets);
        System.out.println("fact sets " + sets);
        System.out.println("fact last " + overKernel(3));
        if (time) {
            int reps = 2_000_000;
            long sink = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < reps; i++) sink += overKernel(i);
            System.out.println("time spliced-override " + (System.nanoTime() - t0) / reps + " ns/op");
            if (sink == 42) System.out.println("unlikely");
        }
    }
}
