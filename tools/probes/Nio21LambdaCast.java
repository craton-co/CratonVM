// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 18, lane L2: compiled SAM calls whose bridge replays a
// `checkcast` (a generic functional interface instantiated with a narrower
// type) enter the impl through a per-call-site CAST thunk: the inline cache
// tests the argument's class id against the one the VM proved passes and
// takes the site's own resolving helper on anything else
// (`r21-lambda-ic-thunk-cannot-replay-a-cast-FIXED-20261010.md`). Every answer must
// be HotSpot 25's: the passing calls, the `ClassCastException` type and
// message of a failing one (also after the site is hot), a null argument, an
// array argument (its header names its component class), a second argument
// class at a site that proved another, an exception thrown by the impl on the
// thunk's fast path and on its miss path, and a `long` return of
// `Long.MIN_VALUE` (the deopt sentinel's bit pattern).
// Switches:
//   CRATONVM_JIT_LAMBDA_CAST_THUNK=0   the Rust path for every cast site (control)
//   CRATONVM_LAMBDA_ARRAY_ARG_CAST=0   the old array verdict (positive control:
//                                      the `array` lines then differ from HotSpot)
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes)
//   rows   `<row> <N> ns/op` lines (-Dnio21.rows= selects):
//          castapply    a hot `Function<Integer,Integer>.apply` through an erased call site
//          castcapture  a hot capturing `BiConsumer<String,Integer>.accept` (TreeMap.forEach shape)
//          castbipred   a hot `BiPredicate<String,String>.test` (two replayed casts)
//
//   java Nio21LambdaCast conf
//   java -Dnio21.rows=castapply,castcapture,castbipred Nio21LambdaCast rows

import java.io.Serializable;
import java.util.ArrayList;
import java.util.List;
import java.util.function.BiConsumer;
import java.util.function.BiPredicate;
import java.util.function.Function;
import java.util.function.ToLongFunction;

public class Nio21LambdaCast {
    static final int WARM = 60_000;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "castapply,castcapture,castbipred"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- erased call sites

    @SuppressWarnings({"rawtypes", "unchecked"})
    static Object apply(Function f, Object x) {
        return f.apply(x);
    }

    @SuppressWarnings({"rawtypes", "unchecked"})
    static void accept(BiConsumer c, Object a, Object b) {
        c.accept(a, b);
    }

    @SuppressWarnings({"rawtypes", "unchecked"})
    static boolean test(BiPredicate p, Object a, Object b) {
        return p.test(a, b);
    }

    @SuppressWarnings({"rawtypes", "unchecked"})
    static long applyAsLong(ToLongFunction f, Object x) {
        return f.applyAsLong(x);
    }

    // ------------------------------------------------------------- lambdas

    static final Function<Integer, Integer> TWICE = i -> i * 2;
    static final Function<Number, Long> AS_LONG = n -> n.longValue() + 1;
    static final Function<CharSequence, Integer> LEN = s -> {
        if (s instanceof StringBuilder sb && sb.length() == 3) {
            throw new IllegalStateException("builder " + sb);
        }
        return s.length();
    };
    static final Function<String, Integer> PARSE = Integer::parseInt;
    static final Function<String, Integer> PARSE_LAMBDA = s -> Integer.parseInt(s);
    static final Function<Serializable, String> SER = s -> s.getClass().getName();
    static final Function<Comparable<?>, String> CMP = c -> "cmp " + c;
    static final ToLongFunction<String> MIN = s -> s.isEmpty() ? Long.MIN_VALUE : s.length();
    static final BiPredicate<String, String> SAME_LEN = (a, b) -> a.length() == b.length();

    static String ccMessage(Runnable r) {
        try {
            r.run();
            return "no-throw";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static void say(String s) {
        System.out.println(s);
    }

    static void conf() {
        // A hot monomorphic site, then a wrong type through the SAME site.
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += (Integer) apply(TWICE, i & 1023);
        }
        say("twice warm " + sum);
        say("twice string " + ccMessage(() -> apply(TWICE, "7")));
        say("twice array " + ccMessage(() -> apply(TWICE, new Integer[] {1})));
        say("twice intarray " + ccMessage(() -> apply(TWICE, new int[] {1})));
        // The unboxing NPE inside the impl: its type only (the helpful
        // message names a javac parameter, which is not this probe's subject).
        say("twice null " + ccMessage(() -> apply(TWICE, null)).split(":")[0]);
        say("twice after " + apply(TWICE, 21));

        // A site that proved Integer, then sees Long and Double (the miss
        // edge, where the full verdict passes them).
        sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += (Long) apply(AS_LONG, i);
        }
        say("number warm " + sum);
        say("number long " + apply(AS_LONG, 5L) + " double " + apply(AS_LONG, 2.5d));
        say("number string " + ccMessage(() -> apply(AS_LONG, "x")));
        say("number array " + ccMessage(() -> apply(AS_LONG, new Long[] {1L})));

        // An interface target; an exception thrown by the impl on the fast
        // path (a String) and on the miss edge (a StringBuilder).
        sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += (Integer) apply(LEN, (i & 1) == 0 ? "ab" : "abcd");
        }
        say("len warm " + sum);
        say("len builder " + apply(LEN, new StringBuilder("abcde")));
        say("len builder3 " + ccMessage(() -> apply(LEN, new StringBuilder("abc"))));
        say("len integer " + ccMessage(() -> apply(LEN, 12)));
        say("len array " + ccMessage(() -> apply(LEN, new String[] {"a"})));
        say("len after " + apply(LEN, "xyz"));

        // A method reference and a lambda over the same throwing body.
        sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += (Integer) apply(PARSE, "12") + (Integer) apply(PARSE_LAMBDA, "3");
        }
        say("parse warm " + sum);
        say("parse bad " + ccMessage(() -> apply(PARSE_LAMBDA, "x")));
        say("parse ref bad " + ccMessage(() -> apply(PARSE, "y")));
        say("parse ref wrong " + ccMessage(() -> apply(PARSE, 4)));

        // Serializable and Comparable targets: an array passes `Serializable`
        // and fails `Comparable`.
        StringBuilder ser = new StringBuilder();
        for (int i = 0; i < WARM; i++) {
            ser.setLength(0);
            ser.append(apply(SER, (i & 1) == 0 ? "s" : Integer.valueOf(i)));
        }
        say("ser warm " + ser);
        say("ser intarray " + apply(SER, new int[0]));
        say("ser strarray " + apply(SER, new String[0]));
        say("ser object " + ccMessage(() -> apply(SER, new Object())));
        say("cmp string " + apply(CMP, "s"));
        say("cmp array " + ccMessage(() -> apply(CMP, new String[0])));

        // `Long.MIN_VALUE` through a cast thunk: a real value, not a sentinel.
        long acc = 0;
        for (int i = 0; i < WARM; i++) {
            acc += applyAsLong(MIN, (i & 7) == 0 ? "" : "abc");
        }
        say("min warm " + acc);
        say("min value " + applyAsLong(MIN, "") + " " + applyAsLong(MIN, "ab"));
        say("min wrong " + ccMessage(() -> applyAsLong(MIN, 9)));

        // A capturing BiConsumer<String,List<String>> (the HttpHeaders shape)
        // and a two-cast BiPredicate.
        List<String> seen = new ArrayList<>();
        BiConsumer<String, List<String>> collect = (k, v) -> {
            if (v.size() > 0) {
                seen.add(k + "=" + v.get(0));
            }
        };
        List<String> one = List.of("v");
        for (int i = 0; i < WARM; i++) {
            seen.clear();
            accept(collect, "k" + (i & 3), one);
        }
        say("collect warm " + seen);
        say("collect wrong " + ccMessage(() -> accept(collect, "k", "notalist")));
        say("collect wrongkey " + ccMessage(() -> accept(collect, 3, one)));
        say("collect nulls " + ccMessage(() -> accept(collect, null, one)) + " " + seen);
        int hits = 0;
        for (int i = 0; i < WARM; i++) {
            hits += test(SAME_LEN, "ab", (i & 1) == 0 ? "cd" : "e") ? 1 : 0;
        }
        say("bipred warm " + hits);
        say("bipred wrong " + ccMessage(() -> test(SAME_LEN, "ab", 'c')));
        say("bipred builder " + ccMessage(() -> test(SAME_LEN, new StringBuilder("ab"), "cd")));
    }

    // ------------------------------------------------------------- rows

    static void rows(String which) {
        long sink = 0;
        List<String> out = new ArrayList<>();
        BiConsumer<String, Integer> capture = (k, v) -> {
            if (v > 1000) {
                out.add(k);
            }
        };
        String[] keys = {"accept", "content-length", "date", "host", "server"};
        for (String row : which.split(",")) {
            int n = 2_000_000;
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (row) {
                    case "castapply" -> {
                        for (int i = 0; i < n; i++) sink += (Integer) apply(TWICE, i & 127);
                    }
                    case "castcapture" -> {
                        for (int i = 0; i < n; i++) accept(capture, keys[i % 5], i & 511);
                        sink += out.size();
                    }
                    case "castbipred" -> {
                        for (int i = 0; i < n; i++) sink += test(SAME_LEN, keys[i % 5], "date") ? 1 : 0;
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
