// JIT round 21 wave 4, lane concat: string concatenation (invokedynamic makeConcatWithConstants) and
// StringBuilder chains in compiled code -- correctness facts plus time rows.
//
// Every `fact` kernel is called WARM times first (both tiers compile it), then the printed answer is
// the kernel's answer for the fact's own arguments, so the line is what COMPILED code builds (the
// promotion facts warm the kernel on LATIN1 operands and then hand it UTF16 ones, the shape whose
// inline concat used to decline to the bridge). Text is printed escaped (backslash, `u`, four hex digits for every unit
// outside 0x20..0x7e) with its length and hashCode, so the lines are console-encoding independent.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21ConcatRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx4g -cp $P R21ConcatRows                     # jdk-only (launcher default)
//   cratonvm --compatible -Xmx4g -cp $P R21ConcatRows        # both modes, same lines
//   java -Xmx4g -cp $P R21ConcatRows                         # HotSpot 25
//   -Xmx4g is for the two overflow facts (one 1 GiB LATIN1 String); with less than 3 GiB of max heap
//   they print `fact overflow-* skipped` on every VM (diff-neutral).
// Arms (cratonvm; every non-`time` line must equal HotSpot's and the other arms'):
//   default
//   CRATONVM_CONCAT_LENGTH_OVERFLOW_OOME=0   kill switch of the r21 overflow fix: the overflow facts
//                                            then show the old answers (a plain heap OOME / a 2^30+1
//                                            unit UTF16 String HotSpot never builds) -- the control
//   CRATONVM_JIT_CONCAT_CODER_PROMOTE=1      CONCAT21-1 (opt-in): a run-time UTF16 operand of a
//                                            LATIN1 site is built inline by the wide twin; time rows
//                                            concat-wide-char / concat-wide-str move, facts must not
//   CRATONVM_JIT_CONCAT_CODER_PROMOTE=1 CRATONVM_JIT_IR_INDY_BRIDGE_WITH_INLINE=1   the IR tier's twin
//   CRATONVM_JIT_CONCAT_UTF16=1              wide plans (literal text above 0xFF): time concat-utf16-lit
//   CRATONVM_JIT_CONCAT_UTF16_FAST=1         CONCAT21-2 (wave 5, opt-in): the bridge builds a UTF16
//                                            result from the TLAB; time concat-utf16-lit (default arm:
//                                            its wide plan is refused, so the bridge serves it),
//                                            concat-wide-char / concat-wide-str move
//   CRATONVM_JIT_CONCAT_FP_INLINE=1          CONCAT21-3 (wave 5, opt-in): float/double operands
//                                            inline; time concat-double; fact fp-vs-jdk must stay at
//                                            0 mismatches on every arm
//   CRATONVM_JIT_CONCAT_VALUE_OF_FUSION=1    CONCAT21-4 (wave 5, opt-in): an exact Integer through
//                                            String.valueOf into the concat; time concat-boxed-int,
//                                            facts fused-* unchanged
//   (each wave-5 arm also with CRATONVM_JIT_STRING_VALUE_OF_THIN=1, and the FP arm with
//    CRATONVM_JIT_IR_INDY_BRIDGE_WITH_INLINE=1 for the IR tier)
//   CRATONVM_JIT_STRING_VALUE_OF_THIN=1 CRATONVM_JIT_STRING_LITERAL_SLOTS=1
//                                            r21 review (rev-x64) regression arm: facts valueof-*
//                                            (the prefix answered "null" for 42 before the fix)
//   CRATONVM_JIT_CONCAT_INLINE=0 CRATONVM_JIT_CONCAT_VALUE_OF_FUSION=1
//                                            the fused Integer reaches the bridge (no inline concat):
//                                            facts fused-* unchanged; fused-fp-declined exercises the
//                                            same path on every fusion arm without the FP switch
//   --nojit                                  interpreter reference
// Engagement (NOT for timing): CRATONVM_DBG_JITC=1 prints `concat-inline EMITTED|DECLINED <m> pc=N`
// and, with the promote switch, `concat-inline coder-promote <m> pc=N edges=K` (single-pass) /
// `ir concat-inline coder-promote` (IR).
//
// HotSpot 25 expectation (by reading JDK 25 StringConcatFactory / StringConcatHelper; confirm on the
// host): every fact line below.
//   fact null-concat a|null|null|null| len=17
//   fact null-sb nullnullnull len=12
//   fact char-vs-int xA66B len=5 / fact sb-char-vs-int A65B66 len=6
//   fact promote-latin1 ab7a| / fact promote-char ab7\u03a9| / fact promote-str \u03a9z7a| (len, hash)
//   fact promote-str-last ab7\u03a9z| / fact promote-null null-7\u03a9z| / promote-char-mixed
//   \u00e9-2147483648\u4e2d|
//   fact sb-promote ab1\u03a9\u00e9-9 len=7
//   fact order-javac ab a5b / fact mutate-javac mx! xm / fact throw-javac c boom / fact null-tostring null|
//   fact mh-const-tags [\u000142\u0002] len=6
//   fact overflow-order g Overflow: String length out of range
//   fact fp-vs-jdk double-mismatches=0 float-mismatches=0 first=-
//   fact fused-integer [-2147483648|7|2147483647] / fact fused-array true true
//   fact fused-fp-declined [-42|1.5][-2147483648|1.0E7]
//   fact valueof-42 42 / valueof-s s true / valueof-null null true / valueof-tostring custom /
//   valueof-long -7 false
//   fact sb-overflow OutOfMemoryError Required array length 1073741840 + 1073741808 is too large
//   (JDK 25 ArraysSupport.hugeLength via AbstractStringBuilder.newCapacity, by reading; confirm)
//   fact mh-mutate mx! xm / fact mh-order-throw e boom / fact mh-char-int A65
//   fact tags \u0001 7 \u0002 s \u0001\u0002 (len 9) / fact numbers ... MIN values ...
//   fact floats 1.0|1.0E7|1.0E-5|1.4E-45|1.7976931348623157E308|0.30000000000000004
//   fact surrogate x\ud800y len=3 / fact unary-literal true false / fact fresh false
//   fact overflow-latin1 OutOfMemoryError Overflow: String length out of range
//   fact overflow-utf16 OutOfMemoryError Overflow: String length out of range
// Time rows (C2, order of magnitude): s+int ~10 ns, s+long+s ~15 ns, mixed5 ~25 ns, char ~8 ns,
// UTF16 rows ~15 ns, sb-chain4 ~20 ns.

import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.StringConcatFactory;
import java.util.Arrays;

public class R21ConcatRows {
    static final int WARM = 30_000;
    static final int N = 200_000;
    static final int REPS = 7;
    static long sink;

    static String esc(String s) {
        if (s == null) {
            return "<null>";
        }
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c <= 0x7e && c != '\\') {
                b.append(c);
            } else {
                b.append(String.format("\\u%04x", (int) c));
            }
        }
        return b.toString();
    }

    static void fact(String row, String value) {
        System.out.println("fact " + row + " " + esc(value) + " len=" + value.length() + " hash=" + value.hashCode());
    }

    // ---- kernels (each compiled by WARM calls before the fact's call) ----

    static String kNull(String s, Object o, String t) {
        return "a|" + s + "|" + o + "|" + t + "|";
    }

    static String kNullSb(String s, Object o, CharSequence cs) {
        return new StringBuilder().append(s).append(o).append(cs).toString();
    }

    static String kCharInt(char c) {
        return "x" + c + (c + 1) + (char) (c + 1);
    }

    static String kSbCharInt(char c) {
        return new StringBuilder().append(c).append((int) c).append((char) (c + 1)).append(c + 1).toString();
    }

    static String kPromote(String s, int i, char c) {
        return s + i + c + "|";
    }

    static String kPromoteStr(String w, int i, String s) {
        return w + i + s + "|";
    }

    static String kSbPromote(int i, char c, String s, long l) {
        return new StringBuilder("ab").append(i).append(c).append(s).append(l).toString();
    }

    static final StringBuilder ORDER = new StringBuilder();

    static final class Tick {
        final String n;

        Tick(String n) {
            this.n = n;
        }

        @Override
        public String toString() {
            ORDER.append(n);
            return n;
        }
    }

    static String kOrder(Object a, int i, Object b) {
        return "" + a + i + b;
    }

    static String kTwo(Object a, Object b) {
        return "" + a + b;
    }

    static String kThree(Object a, Object b, Object c) {
        return "" + a + b + c;
    }

    static String kTags(int i, String s) {
        return "\u0001" + i + "\u0002" + s + "\u0001\u0002";
    }

    static String kNumbers(int i, long l, short sh, byte by, boolean z) {
        return String.valueOf(i) + "/" + Integer.toString(i) + "/" + l + "/" + Long.toString(l) + "/" + sh + "/" + by
                + "/" + z + "/" + i;
    }

    static String kFloats(float f, double d, double e, float g, double h, double k) {
        return f + "|" + d + "|" + e + "|" + g + "|" + h + "|" + k;
    }

    static String kSurrogate(char c) {
        return "x" + c + "y";
    }

    static String kUnary(boolean b) {
        return "" + b;
    }

    static String kD(double d) {
        return "<" + d;
    }

    static String kF(float f) {
        return "<" + f;
    }

    static String kBox(Object o, int i, Object p) {
        return "[" + o + "|" + i + "|" + p + "]";
    }

    // The fused Integer reaches the BRIDGE: an FP operand makes the plan one the
    // compilers refuse without CRATONVM_JIT_CONCAT_FP_INLINE (the fusion screens the
    // plan, not the compile-time inline verdict).
    static String kBoxD(Object o, double d) {
        return "[" + o + "|" + d + "]";
    }

    static int valueOfLen;

    // A direct String.valueOf(Object) call site in a hot loop; the length() read keeps
    // the method in the single-pass tier (the String intrinsic pin).
    static String kValueOf(Object o) {
        String s = String.valueOf(o);
        valueOfLen += s.length();
        return s;
    }

    static final Object TO_STRING_OBJ = new Object() {
        @Override
        public String toString() {
            return "custom";
        }
    };

    static String tDouble(String s, double d) {
        return s + d;
    }

    static String tBoxed(String s, Object o) {
        return s + o;
    }

    static String kUnaryS(String s) {
        return "" + s;
    }

    // ---- time kernels ----

    static String tSInt(String s, int i) {
        return s + i;
    }

    static String tSLongS(String s, long l, String t) {
        return s + l + t;
    }

    static String tMixed5(String s, int i, long l, char c, Object o) {
        return s + i + "x" + l + c + o;
    }

    static String tChar(char a, char b) {
        return "k" + a + b;
    }

    static String tUtf16Lit(int i) {
        return "\u03a9=" + i;
    }

    static String tWideChar(String s, char c) {
        return s + c;
    }

    static String tWideStr(String s, String w) {
        return s + w;
    }

    static String tSbChain4(String s, int i, char c, long l) {
        return new StringBuilder().append(s).append(i).append(c).append(l).toString();
    }

    interface Kernel {
        int run(int i);
    }

    static void time(String row, Kernel k) {
        for (int w = 0; w < 3; w++) {
            long acc = 0;
            for (int i = 0; i < N; i++) {
                acc += k.run(i);
            }
            sink += acc;
        }
        long[] t = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long acc = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < N; i++) {
                acc += k.run(i);
            }
            t[r] = System.nanoTime() - t0;
            sink += acc;
        }
        Arrays.sort(t);
        System.out.println("time " + row + " " + (t[REPS / 2] / N) + " ns/op");
    }

    public static void main(String[] args) throws Throwable {
        final String W = "\u03a9z";
        final String L = "ab";
        Object nul = null;
        String ns = null;

        // null handling: "null" for a null String, a null Object, a null CharSequence.
        for (int i = 0; i < WARM; i++) {
            sink += kNull(L, i, "t").length() + kNullSb(L, i, "c").length();
        }
        fact("null-concat", kNull(ns, nul, ns));
        fact("null-sb", kNullSb(ns, nul, (CharSequence) null));

        // char vs int overloads.
        for (int i = 0; i < WARM; i++) {
            sink += kCharInt((char) ('a' + (i & 15))).length() + kSbCharInt((char) ('a' + (i & 15))).length();
        }
        fact("char-vs-int", kCharInt('A'));
        fact("sb-char-vs-int", kSbCharInt('A'));

        // LATIN1 -> UTF16 promotion mid-chain: warm LATIN1, then UTF16 operands.
        for (int i = 0; i < WARM; i++) {
            sink += kPromote(L, i, 'a').length() + kPromoteStr(L, i, "a").length() + kSbPromote(i, 'c', L, i).length();
        }
        fact("promote-latin1", kPromote(L, 7, 'a'));
        fact("promote-char", kPromote(L, 7, '\u03a9'));
        fact("promote-str", kPromoteStr(W, 7, "a"));
        fact("promote-str-last", kPromoteStr(L, 7, W));
        fact("promote-null", kPromoteStr(null, -7, W));
        fact("sb-promote", kSbPromote(1, '\u03a9', "\u00e9", -9));
        // Mixed warm: both builds stay hot.
        for (int i = 0; i < WARM; i++) {
            sink += kPromote(L, i, (i & 1) == 0 ? 'a' : '\u03a9').length();
        }
        fact("promote-char-mixed", kPromote("\u00e9", Integer.MIN_VALUE, '\u4e2d'));

        // Evaluation order (JLS 15.18.1; javac 25 stringifies each operand in order).
        for (int i = 0; i < WARM; i++) {
            sink += kOrder("p", i, "q").length() + kTwo("p", "q").length() + kThree("p", "q", "r").length();
        }
        ORDER.setLength(0);
        String r = kOrder(new Tick("a"), 5, new Tick("b"));
        System.out.println("fact order-javac " + ORDER + " " + r);
        StringBuilder sb = new StringBuilder("x");
        Object m = new Object() {
            @Override
            public String toString() {
                sb.append('!');
                return "m";
            }
        };
        String r1 = kTwo(m, sb);
        sb.setLength(0);
        sb.append('x');
        String r2 = kTwo(sb, m);
        System.out.println("fact mutate-javac " + r1 + " " + r2);
        Object boom = new Object() {
            @Override
            public String toString() {
                throw new IllegalStateException("boom");
            }
        };
        ORDER.setLength(0);
        try {
            System.out.println("fact throw-javac no-throw " + kThree(new Tick("c"), boom, new Tick("d")));
        } catch (IllegalStateException e) {
            System.out.println("fact throw-javac " + ORDER + " " + e.getMessage());
        }
        Object nullStr = new Object() {
            @Override
            public String toString() {
                return null;
            }
        };
        System.out.println("fact null-tostring " + kTwo(nullStr, "|"));

        // The same through StringConcatFactory with Object operands (no javac valueOf): the factory
        // stringifies every argument in order before it concatenates.
        try {
            MethodType two = MethodType.methodType(String.class, Object.class, Object.class);
            CallSite cs2 = StringConcatFactory.makeConcatWithConstants(MethodHandles.lookup(), "concat", two, "\u0001\u0001");
            MethodHandle mh2 = cs2.dynamicInvoker();
            sb.setLength(0);
            sb.append('x');
            String a1 = (String) mh2.invokeExact((Object) m, (Object) sb);
            sb.setLength(0);
            sb.append('x');
            String a2 = (String) mh2.invokeExact((Object) sb, (Object) m);
            System.out.println("fact mh-mutate " + a1 + " " + a2);
            MethodType three = MethodType.methodType(String.class, Object.class, Object.class, Object.class);
            MethodHandle mh3 = StringConcatFactory.makeConcatWithConstants(MethodHandles.lookup(), "concat", three,
                    "<\u0001\u0001\u0001>").dynamicInvoker();
            ORDER.setLength(0);
            try {
                System.out.println("fact mh-order-throw no-throw " + (String) mh3.invokeExact((Object) new Tick("e"), boom,
                        (Object) new Tick("f")));
            } catch (IllegalStateException e) {
                System.out.println("fact mh-order-throw " + ORDER + " " + e.getMessage());
            }
            MethodType ci = MethodType.methodType(String.class, char.class, int.class);
            MethodHandle mhci = StringConcatFactory.makeConcatWithConstants(MethodHandles.lookup(), "concat", ci,
                    "\u0001\u0001").dynamicInvoker();
            System.out.println("fact mh-char-int " + (String) mhci.invokeExact('A', 65));
            // A \u0002 tag with a constant that itself holds the two tag characters.
            MethodType one = MethodType.methodType(String.class, int.class);
            MethodHandle mhk = StringConcatFactory.makeConcatWithConstants(MethodHandles.lookup(), "concat", one,
                    "\u0002\u0001\u0002", "[\u0001", "\u0002]").dynamicInvoker();
            fact("mh-const-tags", (String) mhk.invokeExact(42));
        } catch (Throwable t) {
            System.out.println("fact mh-concat threw " + t.getClass().getName());
        }

        // Constants holding the recipe's tag characters (javac passes them as \u0002 constants).
        for (int i = 0; i < WARM; i++) {
            sink += kTags(i, "s").length();
        }
        fact("tags", kTags(7, "s"));

        // String.valueOf / Integer.toString / Long.toString in concats, the MIN edges, short/byte/boolean.
        for (int i = 0; i < WARM; i++) {
            sink += kNumbers(i, -i, (short) i, (byte) i, (i & 1) == 0).length();
        }
        fact("numbers-min", kNumbers(Integer.MIN_VALUE, Long.MIN_VALUE, Short.MIN_VALUE, Byte.MIN_VALUE, true));
        fact("numbers-max", kNumbers(Integer.MAX_VALUE, Long.MAX_VALUE, Short.MAX_VALUE, Byte.MAX_VALUE, false));
        fact("numbers-zero", kNumbers(0, 0L, (short) -1, (byte) -1, false));

        for (int i = 0; i < WARM; i++) {
            sink += kFloats(i, i, i, i, i, i).length();
        }
        fact("floats", kFloats(1.0f, 1e7, 1.0e-5, Float.MIN_VALUE, Double.MAX_VALUE, 0.1 + 0.2));
        fact("floats-special", kFloats(Float.NaN, Double.NEGATIVE_INFINITY, -0.0, 1e8f, 100.0, 1e-3));

        for (int i = 0; i < WARM; i++) {
            sink += kSurrogate((char) i).length();
        }
        fact("surrogate", kSurrogate('\ud800'));
        fact("surrogate-low", kSurrogate('\udfff'));

        for (int i = 0; i < WARM; i++) {
            sink += kUnary((i & 1) == 0).length() + kUnaryS("u").length();
        }
        System.out.println("fact unary-literal " + (kUnary(true) == "true") + " " + (kUnary(false) == "false"));
        String fresh = "fresh";
        System.out.println("fact fresh " + (kUnaryS(fresh) == fresh) + " " + kUnaryS(fresh).equals(fresh));

        // float / double operands (CONCAT21-3): the concat's text against the JDK's own
        // Double.toString / Float.toString (JDK bytecode under --jdk-only) over an edge corpus
        // and 200000 random bit patterns each; HotSpot prints 0 mismatches.
        for (int i = 0; i < WARM; i++) {
            sink += kD(i * 0.37).length() + kF(i * 0.37f).length();
        }
        double[] dEdges = {0.0, -0.0, 1.0, -1.0, 1e7, 9999999.999999998, 1e-3, 9.999999999999998e-4, 0.1 + 0.2,
            Double.MAX_VALUE, Double.MIN_VALUE, Double.MIN_NORMAL, -Double.MIN_NORMAL, 4.9e-324 * 3,
            Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, 2e-323, 1e23, 8.41e21, 5.0e-324,
            Double.longBitsToDouble(0x43099999_9999999aL), Math.scalb(1.0, -24), 123456789012345678.0};
        float[] fEdges = {0f, -0f, 1f, 1e7f, 1e-3f, Float.MAX_VALUE, Float.MIN_VALUE, Float.MIN_NORMAL, 1e10f,
            Float.NaN, Float.NEGATIVE_INFINITY, 3.4e-45f, 1.0E-44f, 0.1f + 0.2f, 16777216f};
        java.util.SplittableRandom rnd = new java.util.SplittableRandom(20261018L);
        int dBad = 0;
        int fBad = 0;
        String firstBad = "-";
        for (int i = 0; i < 200_000 + dEdges.length; i++) {
            double d = i < dEdges.length ? dEdges[i] : Double.longBitsToDouble(rnd.nextLong());
            if (!kD(d).equals("<" + Double.toString(d))) {
                if (dBad++ == 0) {
                    firstBad = Long.toHexString(Double.doubleToRawLongBits(d)) + ":" + kD(d) + "/" + Double.toString(d);
                }
            }
        }
        for (int i = 0; i < 200_000 + fEdges.length; i++) {
            float f = i < fEdges.length ? fEdges[i] : Float.intBitsToFloat(rnd.nextInt());
            if (!kF(f).equals("<" + Float.toString(f))) {
                if (fBad++ == 0 && "-".equals(firstBad)) {
                    firstBad = Integer.toHexString(Float.floatToRawIntBits(f)) + ":" + kF(f) + "/" + Float.toString(f);
                }
            }
        }
        System.out.println("fact fp-vs-jdk double-mismatches=" + dBad + " float-mismatches=" + fBad + " first=" + firstBad);
        fact("fp-edges", kD(Double.MIN_VALUE) + kD(1e7) + kD(1e-3) + kF(1.0E-44f) + kD(0.1 + 0.2));

        // valueOf fusion (CONCAT21-4): an exact Integer through String.valueOf into the concat,
        // next to non-fused objects (a Long, an Integer[], null).
        for (int i = 0; i < WARM; i++) {
            sink += kBox(Integer.valueOf(i - 500), i, Integer.valueOf(-i)).length();
        }
        fact("fused-integer", kBox(Integer.valueOf(Integer.MIN_VALUE), 7, Integer.valueOf(Integer.MAX_VALUE)));
        fact("fused-mixed", kBox(Long.valueOf(-5L), 0, null) + kBox(Integer.valueOf(0), -1, Boolean.TRUE));
        String arr = kBox(new Integer[] {1}, 1, Integer.valueOf(1));
        System.out.println("fact fused-array " + arr.startsWith("[[Ljava.lang.Integer;@") + " " + arr.endsWith("|1|1]"));
        for (int i = 0; i < WARM; i++) {
            sink += kBoxD(Integer.valueOf(i), i * 0.5).length();
        }
        fact("fused-fp-declined", kBoxD(Integer.valueOf(-42), 1.5) + kBoxD(Integer.valueOf(Integer.MIN_VALUE), 1e7));

        // String.valueOf(Object) in a hot single-pass loop (r21 review rev-x64: under
        // CRATONVM_JIT_STRING_VALUE_OF_THIN=1 + CRATONVM_JIT_STRING_LITERAL_SLOTS=1 the prefix
        // answered "null" for every non-String): 42, a String, null, a toString() object.
        Object[] valueOfArgs = {Integer.valueOf(42), "s", null, TO_STRING_OBJ};
        for (int i = 0; i < WARM; i++) {
            sink += kValueOf(valueOfArgs[i & 3]).length();
        }
        String vs = "s";
        System.out.println("fact valueof-42 " + kValueOf(Integer.valueOf(42)));
        System.out.println("fact valueof-s " + kValueOf(vs) + " " + (kValueOf(vs) == vs));
        System.out.println("fact valueof-null " + kValueOf(null) + " " + (kValueOf(null) == "null"));
        System.out.println("fact valueof-tostring " + kValueOf(TO_STRING_OBJ));
        System.out.println("fact valueof-long " + kValueOf(Long.valueOf(-7L)) + " " + kValueOf(Boolean.FALSE));

        // Length overflow: StringConcatHelper's OutOfMemoryError, after every operand is stringified.
        if (Runtime.getRuntime().maxMemory() < (3L << 30)) {
            System.out.println("fact overflow-latin1 skipped");
            System.out.println("fact overflow-utf16 skipped");
            System.out.println("fact overflow-order skipped");
            System.out.println("fact sb-overflow skipped");
        } else {
            String big = "a".repeat(1 << 30);
            try {
                String o = big + big;
                System.out.println("fact overflow-latin1 no-throw " + o.length());
            } catch (OutOfMemoryError e) {
                System.out.println("fact overflow-latin1 OutOfMemoryError " + e.getMessage());
            }
            try {
                String o = big + '\u03a9';
                System.out.println("fact overflow-utf16 no-throw " + o.length());
            } catch (OutOfMemoryError e) {
                System.out.println("fact overflow-utf16 OutOfMemoryError " + e.getMessage());
            }
            ORDER.setLength(0);
            try {
                String o = big + new Tick("g") + big;
                System.out.println("fact overflow-order no-throw " + o.length());
            } catch (OutOfMemoryError e) {
                System.out.println("fact overflow-order " + ORDER + " " + e.getMessage());
            }
            // StringBuilder growth past int (AbstractStringBuilder / ArraysSupport.newLength's
            // message, JDK bytecode in both modes on a real JDK).
            try {
                StringBuilder sbBig = new StringBuilder(big);
                sbBig.append(big);
                System.out.println("fact sb-overflow no-throw " + sbBig.length());
            } catch (OutOfMemoryError e) {
                System.out.println("fact sb-overflow OutOfMemoryError " + e.getMessage());
            }
            big = null;
        }

        // ---- time rows ----
        final String S = "key-";
        final String T = "-tail";
        final Object O = "obj";
        time("concat-s-int", i -> tSInt(S, i).length());
        time("concat-s-long-s", i -> tSLongS(S, (long) i * 1_000_003L, T).length());
        time("concat-mixed5", i -> tMixed5(S, i, -i, (char) ('a' + (i & 7)), O).length());
        time("concat-char", i -> tChar((char) ('a' + (i & 7)), (char) ('A' + (i & 7))).length());
        time("concat-utf16-lit", i -> tUtf16Lit(i).length());
        time("concat-wide-char", i -> tWideChar(S, (char) (0x3a9 + (i & 7))).length());
        time("concat-wide-str", i -> tWideStr(S, W).length());
        time("sb-chain4", i -> tSbChain4(S, i, (char) ('a' + (i & 7)), i * 31L).length());
        time("concat-double", i -> tDouble(S, i * 0.125).length());
        final Integer[] boxes = new Integer[16];
        for (int b = 0; b < boxes.length; b++) {
            boxes[b] = Integer.valueOf(b * 1000 - 7000);
        }
        time("concat-boxed-int", i -> tBoxed(S, boxes[i & 15]).length());
        System.out.println("done " + (sink != 42));
    }
}
