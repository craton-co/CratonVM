// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 10, lane L3 (W9L2-1): a compiled non-capturing lambda answers its
// singleton from the site's constant word instead of calling `jit_indy_bridge`.
// HotSpot links a non-capturing `LambdaMetafactory` site to a constant: every
// evaluation of ONE instruction is the same instance, before and after
// tier-up; a capturing site allocates; two instructions are two instances.
// Switch:
//   CRATONVM_JIT_INDY_CONSTANT_LAMBDA=0   the wave-9 bridge call (control arm)
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes):
//          identity of non-capturing lambdas and method references (one site,
//          two sites, before / after the method is hot), capturing lambdas
//          and bound references (fresh each time), the stable parts of the
//          hidden proxy class (name up to the `/`, hidden, interfaces),
//          `Optional.map` chains, and a hot loop that would see a changed
//          instance after tier-up.
//   rows   `<row> <N> ns/op` lines:
//          nclambda   one non-capturing lambda creation (+ a call through it)
//          caplambda  one capturing lambda creation (+ a call through it)
//          caplambdamix  one lambda capturing an int, a long, a String and a
//                     double (nio21 wave 14 L1: the capture fast path;
//                     CRATONVM_JIT_INDY_CAPTURE_FAST=0 is the control arm)
//          optmap     Optional.of(s).map(String::length).map(i -> i * 2).orElse(-1)
//          splicelambda  a hot caller of a tiny callee that creates a
//                     non-capturing lambda (nio21 wave 11 L3: the optimizing
//                     tier splices the callee; CRATONVM_JIT_IR_SPLICE_INDY=0
//                     keeps it a call)
//
// nio21 wave 11, lane L3 adds the `splice` conf lines: the instance a spliced
// callee hands out is the interpreter's, before and after the caller is hot,
// also after a call in the callee and from two callers of one callee.
//
//   java Nio21LambdaSite conf
//   java -Dnio21.rows=nclambda,caplambda,caplambdamix,optmap,splicelambda Nio21LambdaSite rows

import java.util.Arrays;
import java.util.Optional;
import java.util.function.Function;
import java.util.function.IntSupplier;
import java.util.function.Supplier;
import java.util.function.ToIntFunction;

public class Nio21LambdaSite {
    static final int WARM = 200_000;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "nclambda,caplambda,caplambdamix,optmap,splicelambda"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- sites

    static Runnable nonCapturing() {
        return () -> { };
    }

    static Runnable nonCapturingOther() {
        return () -> { };
    }

    static ToIntFunction<String> lengthRef() {
        return String::length;
    }

    static ToIntFunction<String> lengthRefAgain() {
        return String::length;
    }

    static Supplier<String> mixed(int i, long l, String s, double d) {
        return () -> s + i + ":" + l + ":" + d;
    }

    static IntSupplier capturing(int x) {
        return () -> x;
    }

    static IntSupplier boundRef(String s) {
        return s::length;
    }

    /** Two instructions in one method sharing nothing: two instances. */
    static Object[] pairInOneMethod() {
        Supplier<String> a = () -> "a";
        Supplier<String> b = () -> "a";
        return new Object[] {a, b};
    }

    static int optionalChain(String s) {
        return Optional.ofNullable(s).map(String::length).map(i -> i * 2).orElse(-1);
    }

    /** The lambda escapes into a field read back by the caller. */
    static Function<Integer, Integer> twice() {
        return i -> i * 2;
    }

    // ------------------------------------------------------------- splice

    /** A tiny callee the optimizing tier may splice into its callers. */
    static Supplier<String> leafLambda() {
        return () -> "leaf";
    }

    static final StringBuilder SB = new StringBuilder();

    /** A call, then the lambda: the indy runs after a surviving call. */
    static Supplier<String> afterCall() {
        SB.setLength(0);
        return () -> "after";
    }

    static int spliceCallerA(Object first) {
        return leafLambda() == first ? 1 : 0;
    }

    static int spliceCallerB(Object first) {
        Supplier<String> s = leafLambda();
        return (s == first ? 2 : 0) + s.get().length();
    }

    static int spliceAfterCall(Object first) {
        return afterCall() == first ? 1 : 0;
    }

    static void spliceConf() {
        Supplier<String> leaf = leafLambda();
        Supplier<String> after = afterCall();
        System.out.println("splice cold same " + (leafLambda() == leaf) + " " + (afterCall() == after)
                + " distinct " + (leaf != (Object) after));
        long a = 0;
        long b = 0;
        long c = 0;
        for (int i = 0; i < WARM; i++) {
            a += spliceCallerA(leaf);
            b += spliceCallerB(leaf);
            c += spliceAfterCall(after);
        }
        System.out.println("splice hot " + a + " " + b + " " + c);
        System.out.println("splice values " + leafLambda().get() + " " + afterCall().get()
                + " same " + (leafLambda() == leaf));
    }

    // ------------------------------------------------------------- conf

    static String classShape(Object o) {
        Class<?> c = o.getClass();
        String n = c.getName();
        int slash = n.indexOf('/');
        String stable = slash < 0 ? n : n.substring(0, slash);
        String[] ifaces = Arrays.stream(c.getInterfaces()).map(Class::getName).toArray(String[]::new);
        return stable + " hidden=" + c.isHidden() + " slash=" + (slash >= 0)
                + " ifaces=" + Arrays.toString(ifaces);
    }

    static void conf() {
        // Cold: interpreted.
        Runnable r1 = nonCapturing();
        Runnable r2 = nonCapturing();
        System.out.println("nc cold same " + (r1 == r2));
        System.out.println("nc two sites same " + (nonCapturing() == nonCapturingOther()));
        System.out.println("mref cold same " + (lengthRef() == lengthRef()));
        System.out.println("mref two sites same " + (lengthRef() == lengthRefAgain()));
        System.out.println("cap cold same " + (capturing(1) == capturing(1)));
        System.out.println("bound cold same " + (boundRef("x") == boundRef("x")));
        Object[] pair = pairInOneMethod();
        Object[] pair2 = pairInOneMethod();
        System.out.println("pair distinct " + (pair[0] != pair[1])
                + " stable " + (pair[0] == pair2[0] && pair[1] == pair2[1]));

        // Hot: every site's method well past both tiers' thresholds; the
        // instance must stay the one the cold evaluation handed out.
        int changed = 0;
        int capShared = 0;
        int boundShared = 0;
        long acc = 0;
        IntSupplier lastCap = null;
        for (int i = 0; i < WARM; i++) {
            if (nonCapturing() != r1) changed++;
            if (lengthRef() != lengthRef()) changed++;
            Object[] p = pairInOneMethod();
            if (p[0] != pair[0] || p[1] != pair[1]) changed++;
            IntSupplier c = capturing(i);
            if (c == lastCap) capShared++;
            lastCap = c;
            acc += c.getAsInt();
            IntSupplier b = boundRef("abc");
            if (b == boundRef("abc")) boundShared++;
            acc += b.getAsInt();
        }
        System.out.println("nc hot changed " + changed);
        System.out.println("cap hot shared " + capShared + " bound hot shared " + boundShared);
        System.out.println("acc " + acc);
        // nio21 wave 14 L1: every capture kind survives the fast path, each
        // evaluation a fresh object of the site's one class.
        long mixLen = 0;
        int mixShared = 0;
        Supplier<String> lastMix = mixed(0, 0L, "m", 0.5);
        Class<?> mixClass = lastMix.getClass();
        for (int i = 0; i < WARM; i++) {
            Supplier<String> m = mixed(i, (long) i << 33, (i & 1) == 0 ? "e" : "o", i * 0.25);
            if (m == lastMix) mixShared++;
            if (m.getClass() != mixClass) mixShared += 1000;
            lastMix = m;
            mixLen += m.get().length();
        }
        System.out.println("mix " + mixLen + " shared " + mixShared + " last " + lastMix.get());
        System.out.println("nc hot same as cold " + (nonCapturing() == r1)
                + " other site differs " + (nonCapturingOther() != r1));

        // Stable parts of the proxy classes.
        System.out.println("shape nc " + classShape(r1));
        System.out.println("shape mref " + classShape(lengthRef()));
        System.out.println("shape cap " + classShape(capturing(3)));
        System.out.println("same class across sites " + (r1.getClass() == nonCapturingOther().getClass()));
        System.out.println("same class cap " + (capturing(1).getClass() == capturing(2).getClass()));

        // Optional.map chains, cold and hot.
        System.out.println("opt " + optionalChain("abc") + " " + optionalChain(null) + " "
                + optionalChain(""));
        long optSum = 0;
        for (int i = 0; i < WARM; i++) {
            optSum += optionalChain((i & 7) == 0 ? null : "abcd");
        }
        System.out.println("opt hot " + optSum);
        System.out.println("opt empty map present "
                + Optional.<String>empty().map(String::length).isPresent());

        // A lambda used as a value after tier-up.
        Function<Integer, Integer> f = twice();
        int sum = 0;
        for (int i = 0; i < WARM; i++) {
            Function<Integer, Integer> g = twice();
            if (g != f) changed++;
            sum += g.apply(i & 15);
        }
        System.out.println("twice changed " + changed + " sum " + sum);
        spliceConf();
    }

    // ------------------------------------------------------------- rows

    static int ncRow() {
        Runnable r = nonCapturing();
        r.run();
        return r == null ? 0 : 1;
    }

    static int capRow(int i) {
        return capturing(i).getAsInt();
    }

    static String mixS = "x";

    static int capMixRow(int i) {
        return mixed(i, i, mixS, 1.5) == null ? 0 : 1;
    }

    static void rows(String which) {
        long sink = 0;
        for (String row : which.split(",")) {
            int n = 2_000_000;
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (row) {
                    case "nclambda" -> { for (int i = 0; i < n; i++) sink += ncRow(); }
                    case "caplambda" -> { for (int i = 0; i < n; i++) sink += capRow(i); }
                    case "caplambdamix" -> { for (int i = 0; i < n; i++) sink += capMixRow(i); }
                    case "splicelambda" -> {
                        Supplier<String> first = leafLambda();
                        for (int i = 0; i < n; i++) sink += spliceCallerB(first);
                    }
                    case "optmap" -> {
                        for (int i = 0; i < n; i++) sink += optionalChain((i & 1) == 0 ? "ab" : "abc");
                    }
                    default -> throw new IllegalArgumentException(row);
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(row + " " + (best / n) + " ns/op");
        }
        System.out.println("sink " + (sink & 0xff));
    }
}
