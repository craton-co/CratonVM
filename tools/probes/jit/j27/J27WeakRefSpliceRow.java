// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 1 (lane TYPECHK): page
// docs/feature-designs/generative-gc-architecture.md
// Compiled WeakReference.get() / subclass get() / refersTo() on compact receivers.
// Facts are deterministic; timing lines only with -Dj27.time=true.
import java.lang.ref.WeakReference;

public class J27WeakRefSpliceRow {
    static final Object TARGET = new Object();
    static final WeakReference<Object> WEAK = new WeakReference<>(TARGET);

    static final class Sub extends WeakReference<Object> {
        final int tag;
        Sub(Object o, int tag) { super(o); this.tag = tag; }
    }

    static final Sub SUB = new Sub(TARGET, 7);

    static int getKernel(WeakReference<Object> r) {
        int n = 0;
        for (int i = 0; i < 16; i++) {
            if (r.get() == TARGET) n++;
        }
        return n;
    }

    static int refersKernel(WeakReference<Object> r) {
        int n = 0;
        for (int i = 0; i < 16; i++) {
            if (r.refersTo(TARGET)) n++;
        }
        return n;
    }

    static int clearedKernel() {
        WeakReference<Object> r = new WeakReference<>(new Object());
        r.clear();
        int n = 0;
        for (int i = 0; i < 16; i++) {
            if (r.get() == null && r.refersTo(null)) n++;
        }
        return n;
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        long sink = 0;
        for (int w = 0; w < 200_000; w++) {
            sink += getKernel(WEAK) + getKernel(SUB) + refersKernel(WEAK) + refersKernel(SUB);
            if ((w & 1023) == 0) sink += clearedKernel();
        }
        System.out.println("fact weak-get " + getKernel(WEAK));
        System.out.println("fact sub-get " + getKernel(SUB));
        System.out.println("fact weak-refersto " + refersKernel(WEAK));
        System.out.println("fact sub-refersto " + refersKernel(SUB));
        System.out.println("fact cleared " + clearedKernel());
        System.out.println("fact sub-tag " + SUB.tag);
        System.out.println("fact sink-positive " + (sink > 0));
        if (time) {
            String[] rows = {"weakref-get", "subref-get", "weakref-refersto"};
            for (String row : rows) {
                long t0 = System.nanoTime();
                int reps = 1_000_000;
                long before = sink;
                for (int i = 0; i < reps; i++) {
                    sink += row.equals("weakref-get") ? getKernel(WEAK)
                          : row.equals("subref-get") ? getKernel(SUB) : refersKernel(WEAK);
                }
                long ns = (System.nanoTime() - t0) / reps;
                long checksum = sink - before;
                if (checksum != 16L * reps) throw new AssertionError(row + " checksum " + checksum);
                System.out.println("fact timing-" + row + "-checksum " + checksum);
                System.out.println("time " + row + " " + ns + " ns/16");
            }
        }
        if (sink == 42) System.out.println("unlikely");
    }
}
