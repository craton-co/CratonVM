// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s3/fp3 (2026-10-14): Java-visible answers of the per-class structures
// whose in-VM layout the footprint lane changed
// (docs/internal/zgc-round-zs-20261014/s3-fp3-report.md):
//   - the VM's own vtables: an inherited slot shares its declaring class's
//     entry, and a class that only overrides shares its ancestor's signature
//     index. Covered: override-only classes at several depths, a class that
//     inherits everything, a class that appends a slot after an override-only
//     parent, interface default methods (slots appended for an interface),
//     an inherited abstract slot filled lower down, inherited Object methods
//     (equals / hashCode / toString / getClass) on every class, and
//     a megamorphic loop that warms the compiled callers;
//   - the verifier type maps, now kept as per-method verdicts without
//     per-pc rows: the interpreter fast path, exception handlers, and
//     references held in locals and on the operand stack across collections
//     in deep interpreted and compiled frames.
// Every stdout line is deterministic and must equal HotSpot's byte for byte.
//
// Usage: Zs3Fp3Dispatch [rounds]   (default 20000)
import java.util.ArrayList;
import java.util.List;

public class Zs3Fp3Dispatch {
    interface Named {
        default String tag() {
            return "named:" + getClass().getSimpleName();
        }

        String name();
    }

    interface Sized {
        default int size() {
            return 1;
        }
    }

    abstract static class Base {
        int a() {
            return 1;
        }

        int b() {
            return 10;
        }

        abstract int c();

        @Override
        public String toString() {
            return "Base(" + a() + "," + b() + "," + c() + ")";
        }
    }

    // Override-only: same slot layout as Base.
    static class Mid extends Base {
        @Override
        int a() {
            return 2;
        }

        @Override
        int c() {
            return 100;
        }
    }

    // Inherits everything from Mid.
    static class Leaf extends Mid {}

    // Override-only again, two levels down.
    static class Leaf1 extends Leaf {
        @Override
        int b() {
            return 20;
        }
    }

    // Appends slots (a new method and an interface's defaults).
    static class Leaf2 extends Mid implements Named, Sized {
        int d() {
            return 1000;
        }

        @Override
        public String name() {
            return "leaf2";
        }
    }

    // Override-only under a class that appended slots.
    static class Leaf3 extends Leaf2 {
        @Override
        int d() {
            return 2000;
        }

        @Override
        public int size() {
            return 3;
        }

        @Override
        public String name() {
            return "leaf3";
        }
    }

    // Overrides equals / hashCode deterministically.
    static class Keyed extends Leaf3 {
        final int k;

        Keyed(int k) {
            this.k = k;
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof Keyed && ((Keyed) o).k == k;
        }

        @Override
        public int hashCode() {
            return k * 31;
        }

        @Override
        public String tag() {
            return "keyed:" + k;
        }
    }

    static long score(Base x) {
        long s = x.a() + x.b() + x.c();
        if (x instanceof Leaf2) {
            s += ((Leaf2) x).d();
        }
        if (x instanceof Named) {
            s += ((Named) x).tag().length() + ((Named) x).name().length();
        }
        if (x instanceof Sized) {
            s += ((Sized) x).size();
        }
        return s;
    }

    // References in locals and on the operand stack across a collection,
    // with an exception handler in the frame.
    static long deep(int depth, Object held) {
        Object local = new int[] {depth};
        if (depth == 0) {
            System.gc();
            try {
                throw new IllegalStateException("bottom");
            } catch (IllegalStateException e) {
                int h = held instanceof int[] ? ((int[]) held)[0] : held.hashCode();
                return e.getMessage().length() + ((int[]) local)[0] + h % 7;
            }
        }
        String s = "d" + depth;
        long below = deep(depth - 1, local);
        return below + s.length() + ((int[]) local)[0];
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        Base[] all = {new Mid(), new Leaf(), new Leaf1(), new Leaf2(), new Leaf3(), new Keyed(7)};
        for (Base x : all) {
            System.out.println(
                    "zs3fp3 class="
                            + x.getClass().getName()
                            + " str="
                            + x
                            + " score="
                            + score(x)
                            + " eqSelf="
                            + x.equals(x)
                            + " super="
                            + x.getClass().getSuperclass().getSimpleName());
        }
        Keyed k1 = new Keyed(7);
        Keyed k2 = new Keyed(7);
        System.out.println(
                "zs3fp3 keyed eq=" + k1.equals(k2) + " hash=" + k1.hashCode() + " tag=" + k1.tag());
        Object plain = new Leaf();
        System.out.println(
                "zs3fp3 plain eqOther=" + plain.equals(new Leaf()) + " cls=" + plain.getClass().getSimpleName());

        // Megamorphic: warms the compiled callers of every slot kind.
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            Base x = all[r % all.length];
            sum += score(x);
            sum += x.toString().length();
        }
        System.out.println("zs3fp3 mega rounds=" + rounds + " sum=" + sum);

        // Deep frames across collections, interpreted then warmed.
        long d = 0;
        for (int r = 0; r < 200; r++) {
            d += deep(12, "h" + (r % 3));
        }
        System.out.println("zs3fp3 deep sum=" + d);

        // Many small classes' worth of dispatch through a list of lambdas
        // (hidden classes: their own vtables).
        List<java.util.function.IntUnaryOperator> ops = new ArrayList<>();
        for (int i = 0; i < 8; i++) {
            final int c = i;
            ops.add(v -> v * 3 + c);
        }
        long acc = 1;
        for (int r = 0; r < rounds; r++) {
            acc = (acc + ops.get(r & 7).applyAsInt((int) (acc & 0xffff))) & 0xffffffL;
        }
        System.out.println("zs3fp3 lambdas acc=" + acc);
        System.out.println("zs3fp3 done");
    }
}
