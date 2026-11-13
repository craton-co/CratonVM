// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 1 (lane TYPECHK): page
// docs/feature-designs/jit-ir-optimizations.md
// ThreadLocal subclasses whose covariant get()/set() overrides are reached through
// their bridges. Facts are deterministic; timing lines only with -Dj27.time=true.
public class J27TlOverrideBind {
    static int gets;
    static int sets;

    static final class GetOverride extends ThreadLocal<int[]> {
        @Override public int[] get() { gets++; return super.get(); }
        @Override protected int[] initialValue() { return new int[] {3}; }
    }

    static final class SetOverride extends ThreadLocal<int[]> {
        @Override public void set(int[] v) { sets++; super.set(v); }
    }

    static final ThreadLocal<int[]> G = new GetOverride();
    static final ThreadLocal<int[]> S = new SetOverride();
    static final ThreadLocal<int[]> PLAIN = ThreadLocal.withInitial(() -> new int[] {5});

    static int getKernel(ThreadLocal<int[]> t) {
        int n = 0;
        for (int i = 0; i < 16; i++) n += t.get()[0];
        return n;
    }

    static void setKernel(ThreadLocal<int[]> t, int[] v) {
        for (int i = 0; i < 16; i++) t.set(v);
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        int[] v = {9};
        long sink = 0;
        for (int w = 0; w < 100_000; w++) {
            sink += getKernel(G) + getKernel(PLAIN);
            setKernel(S, v);
            setKernel(PLAIN, v);
        }
        System.out.println("fact override-gets " + gets);
        System.out.println("fact override-sets " + sets);
        System.out.println("fact g-value " + getKernel(G));
        System.out.println("fact s-value " + S.get()[0]);
        System.out.println("fact plain-value " + PLAIN.get()[0]);
        if (time) {
            int reps = 200_000;
            long t0 = System.nanoTime();
            for (int i = 0; i < reps; i++) sink += getKernel(G);
            System.out.println("time tl-get-override " + (System.nanoTime() - t0) / reps + " ns/16");
            t0 = System.nanoTime();
            for (int i = 0; i < reps; i++) setKernel(S, v);
            System.out.println("time tl-set-override " + (System.nanoTime() - t0) / reps + " ns/16");
        }
        if (sink == 42) System.out.println("unlikely");
    }
}
