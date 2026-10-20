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

    // NOTE ON SHAPE. Every dispatching method below is a LEAF called many
    // times by `main`, not a loop that dispatches internally. Invocation-count
    // tiering is what compiles a method; a loop-bearing method called forty
    // times never reaches the threshold and would only be compiled through
    // OSR, which is a different door and not the one guarded inlining is
    // planned at. A first version of this probe got that wrong and measured
    // ten compiled bodies for a workload doing tens of millions of dispatches.

    // Monomorphic interface site: one implementation, forever.
    static int monomorphicInterface(Op op, int x) {
        return op.apply(x);
    }

    // Bimorphic virtual site: two overriding classes, evenly mixed.
    static int bimorphicVirtual(Shape a, Shape b, int x) {
        return ((x & 1) == 0 ? a : b).area(x & 0xff);
    }

    // Three implementations in rotation. Under the 92% two-type bar, so it
    // must be REFUSED — its presence in the histogram is as informative as
    // the sites that are admitted.
    static int polymorphicInterface(Op[] ops, int x) {
        return ops[x % ops.length].apply(x & 0xffff);
    }

    // Interface calls into `java.util` through the interface type — the shape
    // that dominates real framework code.
    static int collectionStep(java.util.List<Integer> list,
                              java.util.Map<Integer, Integer> map, int x) {
        int acc = list.get(x & 0x1ff);
        map.put(x & 0xff, acc);
        Integer got = map.get(x & 0xff);
        return got == null ? acc : acc + (got & 1);
    }

    static int stringStep(int x) {
        StringBuilder sb = new StringBuilder();
        sb.append("row-").append(x).append('/').append(x & 0xff);
        String s = sb.toString();
        return s.length() + s.indexOf('/');
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        Op only = new AddOne();
        Op[] three = {new AddOne(), new Doubler(), new Negate()};
        Shape sq = new Square();
        Shape ci = new Circle();
        java.util.List<Integer> list = new java.util.ArrayList<>();
        for (int i = 0; i < 512; i++) {
            list.add(i);
        }
        java.util.Map<Integer, Integer> map = new java.util.HashMap<>();

        long acc = 0;
        for (int i = 0; i < iterations; i++) {
            acc += monomorphicInterface(only, i);
            acc += bimorphicVirtual(sq, ci, i);
            acc += polymorphicInterface(three, i);
            acc += collectionStep(list, map, i);
            acc += stringStep(i);
        }
        // Printed so the whole thing cannot be optimised away, and so a run
        // that produced no work is visibly different from one that did.
        System.out.println("GuardedInlineReachProbe acc=" + acc);
    }
}
