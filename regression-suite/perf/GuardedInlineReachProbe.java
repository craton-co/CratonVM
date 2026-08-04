// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// A workload for `guarded-inline-reach.sh` that is actually made of the thing
// guarded inlining targets: ORDINARY VIRTUAL AND INTERFACE DISPATCH.
//
// CratonBench is arithmetic, array and recursion microbenchmarks; it has
// almost no virtual dispatch, so measuring guarded-inline reach against it
// answers a question nobody asked. This probe is deliberately unexciting Java:
// an interface with a few implementations called through the interface type,
// a supertype-typed field whose runtime class overrides, `java.util`
// collections walked through their interfaces, and `StringBuilder`. That mix
// is what a framework's hot path looks like.
//
// Each shape is called enough times to pass the invocation-count compile
// threshold AND `INLINE_MIN_SPECULATION_OBSERVATIONS` (250 receiver
// observations), because a site below either one is refused for a reason that
// has nothing to do with reach.
public class GuardedInlineReachProbe {

    interface Op {
        int apply(int x);
    }

    static final class AddOne implements Op {
        public int apply(int x) {
            return x + 1;
        }
    }

    static final class Doubler implements Op {
        public int apply(int x) {
            return x * 2;
        }
    }

    static final class Negate implements Op {
        public int apply(int x) {
            return -x;
        }
    }

    static class Shape {
        int area(int n) {
            return n;
        }
    }

    static class Square extends Shape {
        @Override
        int area(int n) {
            return n * n;
        }
    }

    static class Circle extends Shape {
        @Override
        int area(int n) {
            return 3 * n * n;
        }
    }

    // Monomorphic interface site: one implementation, forever.
    static int monomorphicInterface(Op op, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.apply(i);
        }
        return acc;
    }

    // Bimorphic virtual site: two overriding classes, evenly mixed.
    static int bimorphicVirtual(Shape a, Shape b, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += ((i & 1) == 0 ? a : b).area(i & 0xff);
        }
        return acc;
    }

    // Megamorphic-ish: three implementations in rotation. Under the 92%
    // two-type bar, so it must be REFUSED — its presence in the histogram is
    // as informative as the sites that are admitted.
    static int polymorphicInterface(Op[] ops, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += ops[i % ops.length].apply(i & 0xffff);
        }
        return acc;
    }

    static int collections(int n) {
        java.util.List<Integer> list = new java.util.ArrayList<>();
        for (int i = 0; i < 512; i++) {
            list.add(i);
        }
        java.util.Map<Integer, Integer> map = new java.util.HashMap<>();
        int acc = 0;
        for (int round = 0; round < n; round++) {
            for (int i = 0; i < list.size(); i++) {
                acc += list.get(i);
            }
            map.put(round & 0xff, acc);
            Integer got = map.get(round & 0xff);
            if (got != null) {
                acc += got & 1;
            }
        }
        return acc;
    }

    static int strings(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            StringBuilder sb = new StringBuilder();
            sb.append("row-").append(i).append('/').append(i & 0xff);
            String s = sb.toString();
            acc += s.length() + s.indexOf('/');
        }
        return acc;
    }

    public static void main(String[] args) {
        int scale = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        Op only = new AddOne();
        Op[] three = {new AddOne(), new Doubler(), new Negate()};
        Shape sq = new Square();
        Shape ci = new Circle();

        long acc = 0;
        for (int round = 0; round < 40; round++) {
            acc += monomorphicInterface(only, scale);
            acc += bimorphicVirtual(sq, ci, scale);
            acc += polymorphicInterface(three, scale);
            acc += collections(scale / 40);
            acc += strings(scale / 4);
        }
        // Printed so the whole thing cannot be optimised away, and so a run
        // that produced no work is visibly different from one that did.
        System.out.println("GuardedInlineReachProbe acc=" + acc);
    }
}
