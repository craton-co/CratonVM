// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: deep splice chains through JDK code at
// `CRATONVM_JIT_IR_INLINE_MAX_NEST=12` (the frame clamp,
// `CRATONVM_JIT_IR_INLINE_MAX_NEST_FRAME_CLAMP`, default ON), where every
// level is an interface or lambda call and the chain's frames are JDK frames.
//
// Risk targeted: chains the knob was raised for (netty compression): a
// `Function.andThen` / `compose` tower 14 deep, an `Optional.map` chain, a
// `Comparator.thenComparing` tower used by a sort, default methods calling
// default methods across an interface hierarchy, each level bimorphic in the
// warm-up. A throw from the innermost lambda must carry every frame (the
// JDK's `andThen` lambda frames included, by method name), a catch at an
// intermediate level must resume there, and a NEW lambda class at the leaf
// (the deopt trigger) must give the same results as the interpreter.
//
// Run:
//   javac -g -d out R21Fz2SpliceDeep2.java
//   java -Xmx256m -cp out R21Fz2SpliceDeep2                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2SpliceDeep2
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=12 cratonvm -Xmx256m -cp out R21Fz2SpliceDeep2
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=12 CRATONVM_JIT_IR_INLINE_MAX_NEST_FRAME_CLAMP=0 cratonvm ... (kill)
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=12 CRATONVM_JIT_IR_SPLICE_LEAF_PAST_DEPTH=0 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Optional;
import java.util.function.Function;
import java.util.function.IntUnaryOperator;

public class R21Fz2SpliceDeep2 {
    static final int WARM = 60_000;

    interface L0 {
        int base(int x);

        default int d1(int x) {
            return base(x) + 1;
        }
    }

    interface L1 extends L0 {
        default int d2(int x) {
            return d1(x) * 2;
        }
    }

    interface L2 extends L1 {
        default int d3(int x) {
            return d2(x) - 3;
        }

        default int d4(int x) {
            return d3(x) ^ 4;
        }
    }

    interface L3 extends L2 {
        default int d5(int x) {
            return d4(x) + d1(x);
        }

        default int d6(int x) {
            return d5(x) * 3;
        }
    }

    static final class Ka implements L3 {
        public int base(int x) {
            if (x == -999) {
                throw new IllegalArgumentException("ka" + x);
            }
            return x + 10;
        }
    }

    static final class Kb implements L3 {
        public int base(int x) {
            return x * 7;
        }

        @Override
        public int d3(int x) {
            return d2(x) + 33;
        }
    }

    static Function<Integer, Integer> tower(IntUnaryOperator leaf) {
        Function<Integer, Integer> f = x -> leaf.applyAsInt(x);
        for (int i = 0; i < 14; i++) {
            final int k = i;
            if ((i & 1) == 0) {
                Function<Integer, Integer> add = y -> y + k;
                f = f.andThen(add);
            } else {
                Function<Integer, Integer> mul = y -> y * 2 - k;
                f = f.compose(mul);
            }
        }
        return f;
    }

    static int optChain(int x) {
        return Optional.of(x).map(v -> v + 1).map(v -> v * 3).filter(v -> v != 0).map(v -> v - 2).map(v -> v ^ 5)
                .map(v -> v + 7).map(v -> v / 2).map(v -> v * 11).map(v -> v - 13).map(v -> v + 17).orElse(-1);
    }

    record Rec(int a, int b, String c, long d) {
    }

    static final Comparator<Rec> CMP = Comparator.comparingInt(Rec::a).thenComparingInt(Rec::b)
            .thenComparing(Rec::c).thenComparingLong(Rec::d).thenComparing(Comparator.comparing(Rec::c).reversed())
            .thenComparingInt(r -> r.a() ^ r.b());

    static String sorted(int seed) {
        List<Rec> l = new ArrayList<>();
        for (int i = 0; i < 12; i++) {
            int v = (i * 7 + seed) % 5;
            l.add(new Rec(v % 3, v, "s" + (v % 2), i % 4));
        }
        l.sort(CMP);
        StringBuilder sb = new StringBuilder();
        for (Rec r : l) {
            sb.append(r.a()).append(r.b()).append(r.c()).append(r.d());
        }
        return sb.toString();
    }

    static String frames(Throwable t) {
        StringBuilder sb = new StringBuilder(t.getClass().getSimpleName() + ":" + t.getMessage() + " [");
        StackTraceElement[] st = t.getStackTrace();
        for (int i = 0; i < st.length && i < 24; i++) {
            String m = st[i].getMethodName();
            sb.append(i == 0 ? "" : ",").append(m.startsWith("lambda$") ? "lambda" : m);
        }
        return sb.append(']').toString();
    }

    static String row(String tag, Function<Integer, Integer> tw, L3 a, L3 b, int x) {
        String t;
        try {
            t = Integer.toString(tw.apply(x));
        } catch (RuntimeException e) {
            t = frames(e);
        }
        String d;
        try {
            d = a.d6(x) + "/" + b.d6(x);
        } catch (RuntimeException e) {
            d = frames(e);
        }
        return "row " + tag + " x=" + x + " tower=" + t + " opt=" + optChain(x) + " defaults=" + d + " sort="
                + sorted(x);
    }

    public static void main(String[] args) {
        IntUnaryOperator leafA = v -> {
            if (v == 4242) {
                throw new IllegalStateException("leaf" + v);
            }
            return v + 1;
        };
        IntUnaryOperator leafB = v -> v * 3;
        Function<Integer, Integer> ta = tower(leafA);
        Function<Integer, Integer> tb = tower(leafB);
        L3 ka = new Ka();
        L3 kb = new Kb();
        int[] xs = {0, 1, 5, -999};
        for (int x : xs) {
            System.out.println(row("cold", ta, ka, kb, x));
        }
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            Function<Integer, Integer> t = (r & 1) == 0 ? ta : tb;
            L3 l = (r & 1) == 0 ? ka : kb;
            h = h * 31 + t.apply(r & 1023) + optChain(r) + l.d6(r & 1023);
            if ((r & 63) == 0) {
                h = h * 31 + sorted(r).length();
            }
        }
        System.out.println("warm hash " + h);
        for (int x : xs) {
            System.out.println(row("hot", ta, ka, kb, x));
        }
        // A leaf that always throws: every tower frame on the trace.
        System.out.println(row("leaf-throw", tower(v -> {
            throw new IllegalStateException("direct" + v);
        }), ka, kb, 2));
        // Deopt trigger: a third leaf lambda class and a third L3 class.
        IntUnaryOperator leafC = v -> v - 100;
        L3 kc = x -> x - 1;
        Function<Integer, Integer> tc = tower(leafC);
        for (int x : xs) {
            System.out.println(row("late", tc, kc, ka, x));
        }
        for (int x : xs) {
            System.out.println(row("after", ta, ka, kb, x));
        }
    }
}
