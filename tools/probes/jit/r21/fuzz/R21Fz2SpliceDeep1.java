// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: deep splice chains at the knob's top
// (`CRATONVM_JIT_IR_INLINE_MAX_NEST=12`, clamped by
// `CRATONVM_JIT_IR_INLINE_MAX_NEST_FRAME_CLAMP` (default ON) to
// `ir_inline_max_nest_frame_cap`: `MAX_IR_SPLICE_DEPTH - 1` with the leaf tier,
// `MAX_IR_SPLICE_DEPTH` without; `lib.rs` `ir_inline_max_nest_depth`).
//
// Risk targeted: at 7 / 8 splice frames every frame-state chain is as long as
// the builder allows. A throw at depth d must report all d frames with
// their lines (compiled-frame decode of the chain), a catch at an
// intermediate depth must resume with that frame's locals, a deopt at the
// leaf must rebuild 8+ interpreter frames with each frame's locals (ints,
// longs, references), and a StackWalker at the leaf must list them all.
// With `_FRAME_CLAMP=0` the build of a 9-frame chain fails and the rebuild
// drops the outermost site: slower, still correct -- the kill arm must print
// the same lines. Chains: 14 static levels, 14 virtual levels on a
// monomorphic receiver (a late subclass deopts them), and a self-recursive
// method to depth 12. Lines need `javac -g`.
//
// Run:
//   javac -g -d out R21Fz2SpliceDeep1.java
//   java -Xmx256m -cp out R21Fz2SpliceDeep1                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2SpliceDeep1
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=12 cratonvm -Xmx256m -cp out R21Fz2SpliceDeep1
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=12 CRATONVM_JIT_IR_INLINE_MAX_NEST_FRAME_CLAMP=0 cratonvm ... (kill)
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=12 CRATONVM_JIT_IR_SPLICE_LEAF_PAST_DEPTH=0 cratonvm ...
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=8 cratonvm ..., CRATONVM_JIT_IR_INLINE_MAX_NEST=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.stream.Collectors;

public class R21Fz2SpliceDeep1 {
    static final int WARM = 60_000;
    static volatile int throwAt = -1;
    static volatile int walkAt = -1;
    static String walked = "";

    static long leaf(int d, long acc) {
        if (d == throwAt) {
            throw new IllegalStateException("at" + d);
        }
        if (d == walkAt) {
            walked = StackWalkerHolder.WALKER.walk(s -> s.limit(18).map(f -> f.getMethodName() + ":" + f.getLineNumber())
                    .collect(Collectors.joining(",")));
        }
        return acc * 3 + d;
    }

    static final class StackWalkerHolder {
        static final StackWalker WALKER = StackWalker.getInstance();
    }

    static long s13(int x, long a) { long b = a + 13; return leaf(13, b) + x; }
    static long s12(int x, long a) { long b = a ^ 12; return s13(x + 1, b) + leaf(12, b); }
    static long s11(int x, long a) { String t = "s11"; return s12(x, a + t.length()) - 11; }
    static long s10(int x, long a) { return s11(x * 2, a + 10) + leaf(10, a); }
    static long s9(int x, long a) {
        try {
            return s10(x, a - 9);
        } catch (IllegalStateException e) {
            return -9_000 - e.getMessage().length() + x;
        }
    }
    static long s8(int x, long a) { long keep = a * 7; return s9(x, a + 8) + keep; }
    static long s7(int x, long a) { return s8(x - 1, a + 7); }
    static long s6(int x, long a) { int[] arr = {x, 6}; return s7(arr[0], a + arr[1]); }
    static long s5(int x, long a) { return s6(x, a + 5) + leaf(5, a); }
    static long s4(int x, long a) {
        try {
            return s5(x, a + 4);
        } catch (IllegalStateException e) {
            return -4_000 - x;
        }
    }
    static long s3(int x, long a) { return s4(x, a + 3) * 2; }
    static long s2(int x, long a) { double d = x * 0.5; return s3(x, a + 2) + (long) d; }
    static long s1(int x, long a) { return s2(x, a + 1); }
    static long s0(int x) { return s1(x, x * 1_000_003L); }

    static class V {
        long v13(int x, long a) { return leaf(13, a) + x; }
        long v12(int x, long a) { return v13(x, a + 12); }
        long v11(int x, long a) { return v12(x, a + 11); }
        long v10(int x, long a) { return v11(x, a + 10); }
        long v9(int x, long a) { return v10(x, a + 9); }
        long v8(int x, long a) { return v9(x, a + 8); }
        long v7(int x, long a) { return v8(x, a + 7); }
        long v6(int x, long a) { return v7(x, a + 6); }
        long v5(int x, long a) { return v6(x, a + 5); }
        long v4(int x, long a) { return v5(x, a + 4); }
        long v3(int x, long a) { return v4(x, a + 3); }
        long v2(int x, long a) { return v3(x, a + 2); }
        long v1(int x, long a) { return v2(x, a + 1); }
        long v0(int x) { return v1(x, x); }
    }

    static class W extends V {
        @Override
        long v13(int x, long a) {
            return -leaf(13, a) - x;
        }

        @Override
        long v7(int x, long a) {
            return v8(x, a * 2);
        }
    }

    static long rec(int depth, long a) {
        if (depth == 0) {
            return leaf(0, a);
        }
        long here = a + depth;
        return rec(depth - 1, here) + here % 7;
    }

    static String trace(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder(t.getMessage() + " [");
        for (int i = 0; i < st.length && i < 18; i++) {
            sb.append(i == 0 ? "" : ",").append(st[i].getMethodName()).append(':').append(st[i].getLineNumber());
        }
        return sb.append(']').toString();
    }

    static String rows(String tag, V v) {
        StringBuilder sb = new StringBuilder("row " + tag);
        sb.append(" s=").append(s0(3)).append(" v=").append(v.v0(3)).append(" rec=").append(rec(12, 5));
        int[] depths = {13, 12, 10, 5, 0};
        for (int d : depths) {
            throwAt = d;
            try {
                sb.append(" t").append(d).append('=').append(s0(1)).append('/').append(v.v0(1)).append('/')
                        .append(rec(12, 1));
            } catch (IllegalStateException e) {
                sb.append(" t").append(d).append("!").append(trace(e));
            }
            throwAt = -1;
        }
        System.out.println(sb);
        walkAt = 13;
        s0(2);
        String w1 = walked;
        v.v0(2);
        String w2 = walked;
        walkAt = 0;
        rec(12, 2);
        String w3 = walked;
        walkAt = -1;
        return "walk " + tag + " s:" + w1 + " | v:" + w2 + " | rec:" + w3;
    }

    public static void main(String[] args) {
        V v = new V();
        System.out.println(rows("cold", v));
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + s0(r) + v.v0(r) + rec(12, r);
        }
        System.out.println("warm hash " + h);
        System.out.println(rows("hot", v));
        // Deopt trigger: a subclass overriding a middle and the leaf level.
        V w = new W();
        System.out.println(rows("late", w));
        for (int r = 0; r < WARM / 2; r++) {
            h = h * 31 + s0(r) + ((r & 1) == 0 ? v : w).v0(r) + rec(12, r);
        }
        System.out.println("rewarm hash " + h);
        System.out.println(rows("after", v));
        System.out.println(rows("after-w", w));
        System.out.println("done");
    }
}
