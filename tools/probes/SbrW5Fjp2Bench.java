// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Constructor;
import java.util.function.LongSupplier;

/**
 * SBR wave 5, lane {@code fjp2}: timing micro for the {@code StackWalker} rows
 * (walk / forEach / getCallerClass through the JDK's {@code StackStreamFactory})
 * and for the pieces that path is made of, each at a chosen stack depth -- a
 * cost that grows with the depth is a whole-stack walk somewhere.
 *
 * <pre>
 *   java SbrW5Fjp2Bench [rows] [div]
 *     rows: comma list of sw,sw0,swf,swc,swc40,gdc,gdc40,ni,ni40,nia,nia40,nip,nip40,new
 *           (default: the first nine)
 *     div divides every warm-up and rep count (default 1)
 * </pre>
 *
 * Each row prints microseconds per call and a checksum.
 */
public class SbrW5Fjp2Bench {
    static int div = 1;

    static void time(String name, int warm, int reps, LongSupplier op) {
        warm = Math.max(1, warm / div);
        reps = Math.max(1, reps / div);
        long cs = 0;
        for (int i = 0; i < warm; i++) cs += op.getAsLong();
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) cs += op.getAsLong();
        long t1 = System.nanoTime();
        System.out.printf("%-6s %10.2f us/op  reps=%d  cs=%d%n", name, (t1 - t0) / 1000.0 / reps, reps, cs);
    }

    /** Run {@code op} {@code n} frames deeper than the caller. */
    static long at(int n, LongSupplier op) {
        if (n == 0) {
            return op.getAsLong();
        }
        return at(n - 1, op);
    }

    static final StackWalker PLAIN = StackWalker.getInstance();
    static final StackWalker RETAIN = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static final class Leaf {
        final int v;

        Leaf(Integer v) {
            this.v = v;
        }
    }

    public static final class PubLeaf {
        final int v;

        public PubLeaf(Integer v) {
            this.v = v;
        }
    }

    static final Constructor<Leaf> LEAF;
    static final Constructor<Leaf> LEAF_ACC;
    static final Constructor<PubLeaf> PUB_LEAF;

    static {
        try {
            LEAF = Leaf.class.getDeclaredConstructor(Integer.class);
            LEAF_ACC = Leaf.class.getDeclaredConstructor(Integer.class);
            LEAF_ACC.setAccessible(true);
            PUB_LEAF = PubLeaf.class.getDeclaredConstructor(Integer.class);
        } catch (NoSuchMethodException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long nia() {
        try {
            return LEAF_ACC.newInstance(7).v;
        } catch (ReflectiveOperationException e) {
            return -1;
        }
    }

    static long nip() {
        try {
            return PUB_LEAF.newInstance(7).v;
        } catch (ReflectiveOperationException e) {
            return -1;
        }
    }

    static long walkMain() {
        return PLAIN.walk(s -> s.filter(f -> f.getMethodName().equals("main")).count());
    }

    static long forEachCount() {
        long[] n = new long[1];
        PLAIN.forEach(f -> n[0]++);
        return n[0];
    }

    static long caller() {
        return RETAIN.getCallerClass().getName().length();
    }

    static long gdc() {
        try {
            return Leaf.class.getDeclaredConstructor(Integer.class).getParameterCount();
        } catch (NoSuchMethodException e) {
            return -1;
        }
    }

    static long ni() {
        try {
            return LEAF.newInstance(7).v;
        } catch (ReflectiveOperationException e) {
            return -1;
        }
    }

    public static void main(String[] args) {
        String rows = args.length > 0 ? args[0] : "sw,sw0,swf,swc,swc40,gdc,gdc40,ni,ni40";
        div = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        for (String r : rows.split(",")) {
            switch (r) {
                case "sw" -> time("sw", 200, 2000, () -> at(20, SbrW5Fjp2Bench::walkMain));
                case "sw0" -> time("sw0", 200, 2000, SbrW5Fjp2Bench::walkMain);
                case "swf" -> time("swf", 200, 2000, () -> at(20, SbrW5Fjp2Bench::forEachCount));
                case "swc" -> time("swc", 2000, 20000, SbrW5Fjp2Bench::caller);
                case "swc40" -> time("swc40", 2000, 20000, () -> at(40, SbrW5Fjp2Bench::caller));
                case "gdc" -> time("gdc", 2000, 20000, SbrW5Fjp2Bench::gdc);
                case "gdc40" -> time("gdc40", 2000, 20000, () -> at(40, SbrW5Fjp2Bench::gdc));
                case "ni" -> time("ni", 2000, 20000, SbrW5Fjp2Bench::ni);
                case "ni40" -> time("ni40", 2000, 20000, () -> at(40, SbrW5Fjp2Bench::ni));
                case "nia" -> time("nia", 2000, 20000, SbrW5Fjp2Bench::nia);
                case "nia40" -> time("nia40", 2000, 20000, () -> at(40, SbrW5Fjp2Bench::nia));
                case "nip" -> time("nip", 2000, 20000, SbrW5Fjp2Bench::nip);
                case "nip40" -> time("nip40", 2000, 20000, () -> at(40, SbrW5Fjp2Bench::nip));
                case "new" -> time("new", 2000, 20000, () -> new Leaf(7).v);
                default -> System.out.println("unknown row " + r);
            }
        }
    }
}
