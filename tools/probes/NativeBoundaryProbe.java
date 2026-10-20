import java.util.concurrent.atomic.AtomicInteger;

/**
 * NativeBoundaryProbe — ns/op for natives that go through the FULL compiled
 * dispatch path: the site cache, the argument decode, and the non-leaf funnel.
 *
 * Every kernel lives in its own static method, so the OSR artifact compiles it
 * (a kernel in `main` is bailed by main's invokedynamic string concat — see
 * bench/StringRegexOnly.java). Each rung prints its own checksum: a fast wrong
 * answer is a bug, not a result.
 *
 * Rungs are chosen by ARGUMENT SHAPE, because that is what the boundary cost
 * scales with — a receiver plus N slots, decoded and forwarded per call.
 */
public class NativeBoundaryProbe {

    static long idHash(Object[] objs, int rounds) {
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < objs.length; i++) {
                acc += System.identityHashCode(objs[i]);
            }
        }
        return acc;
    }

    static long atomicCas(AtomicInteger a, int rounds) {
        long ok = 0;
        for (int r = 0; r < rounds; r++) {
            int v = a.get();
            if (a.compareAndSet(v, v + 1)) {
                ok++;
            }
        }
        return ok;
    }

    static long atomicGet(AtomicInteger a, int rounds) {
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            acc += a.get();
        }
        return acc;
    }

    static long sbAppendInt(int rounds) {
        StringBuilder sb = new StringBuilder();
        for (int r = 0; r < rounds; r++) {
            sb.append(r & 7);
            if (sb.length() > 4096) {
                sb.setLength(0);
            }
        }
        return sb.length();
    }

    static long stringLength(String[] ss, int rounds) {
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < ss.length; i++) {
                acc += ss[i].length();
            }
        }
        return acc;
    }

    static void rung(String name, int ops, long t0, long t1, long checksum) {
        double ns = (t1 - t0) * 1e6 / ops;
        System.out.printf("%-28s %9.1f ns/op   [%d]%n", name, ns, checksum);
    }

    public static void main(String[] args) {
        int scale = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 3;

        Object[] objs = new Object[64];
        for (int i = 0; i < objs.length; i++) {
            objs[i] = new Object();
        }
        String[] ss = new String[64];
        for (int i = 0; i < ss.length; i++) {
            ss[i] = "s" + i;
        }
        AtomicInteger a = new AtomicInteger();

        int outer = 40000 * scale;
        for (int p = 1; p <= passes; p++) {
            System.out.println("-- pass " + p);
            long t0, t1, c;

            t0 = System.currentTimeMillis();
            c = idHash(objs, outer / 64);
            t1 = System.currentTimeMillis();
            rung("System.identityHashCode", (outer / 64) * 64, t0, t1, c != 0 ? 1 : 0);

            t0 = System.currentTimeMillis();
            c = atomicGet(a, outer);
            t1 = System.currentTimeMillis();
            rung("AtomicInteger.get", outer, t0, t1, c);

            t0 = System.currentTimeMillis();
            c = atomicCas(a, outer);
            t1 = System.currentTimeMillis();
            rung("AtomicInteger.compareAndSet", outer, t0, t1, c);

            t0 = System.currentTimeMillis();
            c = sbAppendInt(outer);
            t1 = System.currentTimeMillis();
            rung("StringBuilder.append(int)", outer, t0, t1, c);

            t0 = System.currentTimeMillis();
            c = stringLength(ss, outer / 64);
            t1 = System.currentTimeMillis();
            rung("String.length", (outer / 64) * 64, t0, t1, c);
        }
    }
}
