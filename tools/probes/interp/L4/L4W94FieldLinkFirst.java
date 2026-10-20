// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L4: `CRATONVM_MH_FIELD_LINK_FIRST`
// (default off; page
// `i71-L4-proposal-a-linked-field-accessor-is-served-before-the-real-handle-lanes`).
// With the switch on, the `invokeExact` / `invoke` natives serve a LINKED
// synthetic field getter / setter at its declared site before the real-handle
// lanes. The gain is `--compatible`'s only (under `--jdk-only` the finders
// answer real JDK handles, which carry no link). This file is both the
// behaviour probe (every answer and refusal must be the same in both arms and
// both modes) and the A/B bench (rows `getter-warm-exact`,
// `getter-warm-invoke`, `setter-warm-exact`, `bound-getter-warm-exact`).
//
// Output, all on STDOUT:
//   "<case> <answer>"                       behaviour lines (diffed vs HotSpot)
//   "TIME <name> min <ns> med <ns> ns/op"   timings (filter TIME lines before diffing;
//                                           wave 103 moved them off "row" lines so the
//                                           battery can compare the rest with HotSpot)
//   "checksum <name> <value>"               deterministic
// Args: [time] [reps], each rep 200000 calls per row. Wave 105: the TIME lines
// are printed only with the `time` argument (reps then default 5; without it
// 1), so a plain run prints exactly HotSpot's lines. A/B: pass `time`.
//
// Run: javac -d . L4W94FieldLinkFirst.java
//      java -cp . L4W94FieldLinkFirst                                  (HotSpot)
//      cratonvm --compatible --nojit -cp . L4W94FieldLinkFirst
//      CRATONVM_MH_FIELD_LINK_FIRST=1 cratonvm --compatible --nojit -cp . L4W94FieldLinkFirst
//      (and both arms with --jdk-only --nojit as the off-state cost check, and
//      with the JIT)
//
// Expected values derived from JDK 25's `DirectMethodHandle$Accessor` semantics:
// a getter / setter at its declared type reads / writes the field; `invoke`
// at the declared type is `invokeExact`; `invokeExact` at another type throws
// `WrongMethodTypeException` (only the class is printed, the message is not
// claimed here); a null receiver throws `NullPointerException` (`checkBase`);
// a subclass receiver is admitted by the declared `(Box)int` type.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// getter-exact 7
// getter-invoke 7
// setter-exact 9
// setter-invoke 10
// static-getter-exact 5
// static-setter-exact 6
// subclass-getter-exact 11
// bound-getter-exact 10
// wrong-type java.lang.invoke.WrongMethodTypeException
// null-receiver java.lang.NullPointerException
// invoke-boxing-site 10
// checksum getter-warm-exact 1400000
// checksum getter-warm-invoke 1400000
// checksum setter-warm-exact 200000
// checksum bound-getter-warm-exact 2000000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L4W94FieldLinkFirst {
    public static class Box {
        public static int S = 5;
        public int v = 7;
    }

    public static class Sub extends Box {
        public Sub() {
            v = 11;
        }
    }

    static final MethodHandles.Lookup LOOKUP = MethodHandles.lookup();

    /** Wave 105: print TIME lines (the `time` argument). */
    static boolean timing;

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static long min(long[] xs) {
        long best = Long.MAX_VALUE;
        for (long x : xs) best = Math.min(best, x);
        return best;
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static void row(String name, int reps, int n, Body body) throws Throwable {
        long[] t = new long[reps];
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = body.run(n);
            t[r] = (System.nanoTime() - t0) / n;
            if (r == 0) {
                sum = s;
            }
        }
        if (timing) System.out.printf(Locale.ROOT, "TIME %s min %d med %.1f ns/op%n", name, min(t), median(t));
        System.out.println("checksum " + name + " " + sum);
    }

    public static void main(String[] args) throws Throwable {
        // Wave 105: timings only with the `time` argument, so the default run
        // prints exactly HotSpot's lines (the battery compares raw output).
        int reps = -1;
        for (String a : args) {
            if (a.equals("time")) {
                timing = true;
            } else {
                reps = Integer.parseInt(a);
            }
        }
        if (reps < 0) {
            reps = timing ? 5 : 1;
        }
        MethodHandle getter = LOOKUP.findGetter(Box.class, "v", int.class);
        MethodHandle setter = LOOKUP.findSetter(Box.class, "v", int.class);
        MethodHandle sGetter = LOOKUP.findStaticGetter(Box.class, "S", int.class);
        MethodHandle sSetter = LOOKUP.findStaticSetter(Box.class, "S", int.class);
        Box box = new Box();

        // Behaviour. The first exact call links the handle; the second is the
        // linked call the switch serves early.
        int a = (int) getter.invokeExact(box);
        a = (int) getter.invokeExact(box);
        System.out.println("getter-exact " + a);
        System.out.println("getter-invoke " + (int) getter.invoke(box));
        setter.invokeExact(box, 8);
        setter.invokeExact(box, 9);
        System.out.println("setter-exact " + box.v);
        setter.invoke(box, 10);
        System.out.println("setter-invoke " + box.v);
        int s = (int) sGetter.invokeExact();
        s = (int) sGetter.invokeExact();
        System.out.println("static-getter-exact " + s);
        sSetter.invokeExact(6);
        sSetter.invokeExact(6);
        System.out.println("static-setter-exact " + Box.S);
        Box sub = new Sub();
        System.out.println("subclass-getter-exact " + (int) getter.invokeExact(sub));
        MethodHandle bound = getter.bindTo(box);
        int b = (int) bound.invokeExact();
        b = (int) bound.invokeExact();
        System.out.println("bound-getter-exact " + b);
        try {
            long wrong = (long) getter.invokeExact(box);
            System.out.println("wrong-type no-throw " + wrong);
        } catch (Throwable t) {
            System.out.println("wrong-type " + t.getClass().getName());
        }
        try {
            int n = (int) getter.invokeExact((Box) null);
            System.out.println("null-receiver no-throw " + n);
        } catch (Throwable t) {
            System.out.println("null-receiver " + t.getClass().getName());
        }
        // An `invoke` at a boxing site is an `asType`, never the link.
        Object boxed = getter.invoke(box);
        System.out.println("invoke-boxing-site " + boxed);

        // Bench rows (box.v is 10 from here on; Sub's 11 is not used).
        box.v = 7;
        row("getter-warm-exact", reps, 200000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) getter.invokeExact(box);
            }
            return acc;
        });
        row("getter-warm-invoke", reps, 200000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) getter.invoke(box);
            }
            return acc;
        });
        row("setter-warm-exact", reps, 200000, n -> {
            for (int i = 0; i < n; i++) {
                setter.invokeExact(box, i);
            }
            return box.v + 1;
        });
        box.v = 10;
        row("bound-getter-warm-exact", reps, 200000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) bound.invokeExact();
            }
            return acc;
        });
    }
}
