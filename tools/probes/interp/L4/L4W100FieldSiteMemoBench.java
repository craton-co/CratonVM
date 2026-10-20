// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L4: bench and behaviour probe for
// `CRATONVM_MH_REAL_FIELD_SITE_MEMO` (default off; page
// `docs/internal/fixed-bugs/interpreter-i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-FIXED-20261018.md`,
// "Wave 100").
//
// A warm REAL field accessor called by `invokeExact` / `invoke` at a site of
// its own type is served by `lang_invoke::mh_real_field_before_linker`. Each
// call still read the handle's `type` by name, built the site-type key string,
// probed the site-type table under its lock, and classified every class the
// site names (`mh_site_access_refusal`). With the switch on, a (handle,
// caller, site) that passed those checks in this collection epoch skips them.
//
// Behaviour lines (every arm, both modes, switch on and off, must equal
// HotSpot): the memo's hazards -- two handles alternating at one site, one
// handle from two caller classes, a collection between calls (the epoch
// changes), a null base after warm calls (still the NPE), another type at a
// warm handle (still the WrongMethodTypeException).
//
// Timing lines start with "TIME " (filter them before diffing):
//   TIME <row> med <ns> ns/op      (median of reps, 20000 calls per rep)
//
// Run: javac -d . L4W100FieldSiteMemoBench.java
//      java -cp . L4W100FieldSiteMemoBench                                  (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W100FieldSiteMemoBench [reps]
//      CRATONVM_MH_REAL_FIELD_SITE_MEMO=1 cratonvm --jdk-only [--nojit] -cp . L4W100FieldSiteMemoBench [reps]
//      (the same with --compatible; interleave the arms, take medians)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources; TIME lines removed)
// getter-exact 7
// alternate 7/70 7/70 7/70
// two-callers 7 7
// after-gc 7 9
// null-base java.lang.NullPointerException
// wrong-type java.lang.invoke.WrongMethodTypeException
// static-getter 5
// checksum getter-warm-exact 140000
// checksum getter-warm-invoke 140000
// checksum setter-warm-exact 20000
// checksum static-getter-warm-exact 100000
// checksum alternate-warm-exact 770000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L4W100FieldSiteMemoBench {
    public static class Box {
        public static int S = 5;
        public int v = 7;
        public int w = 70;
    }

    /** A second caller class: its own sites, the same handles. */
    static final class Other {
        static int read(MethodHandle getter, Box box) throws Throwable {
            return (int) getter.invokeExact(box);
        }
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
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
        System.out.printf(Locale.ROOT, "TIME %s med %.1f ns/op%n", name, median(t));
        System.out.println("checksum " + name + " " + sum);
    }

    static int read(MethodHandle getter, Box box) throws Throwable {
        return (int) getter.invokeExact(box);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle getter = lookup.findGetter(Box.class, "v", int.class);
        MethodHandle getterW = lookup.findGetter(Box.class, "w", int.class);
        MethodHandle setter = lookup.findSetter(Box.class, "v", int.class);
        MethodHandle sGetter = lookup.findStaticGetter(Box.class, "S", int.class);
        Box box = new Box();

        int a = (int) getter.invokeExact(box);
        a = (int) getter.invokeExact(box);
        System.out.println("getter-exact " + a);

        // Two handles alternating at ONE site (`read`): each answer is its own.
        StringBuilder alt = new StringBuilder("alternate");
        for (int i = 0; i < 3; i++) {
            alt.append(' ').append(read(getter, box)).append('/').append(read(getterW, box));
        }
        System.out.println(alt);

        // One handle from two caller classes.
        System.out.println("two-callers " + read(getter, box) + " " + Other.read(getter, box));

        // A collection between warm calls: the value written after it is read.
        read(getter, box);
        System.gc();
        box.v = 9;
        int afterGc = read(getter, box);
        box.v = 7;
        System.out.println("after-gc " + read(getter, box) + " " + afterGc);

        // Warm, then a null base: `checkBase`'s NPE every time.
        try {
            read(getter, null);
            System.out.println("null-base no-throw");
        } catch (NullPointerException e) {
            System.out.println("null-base " + e.getClass().getName());
        }

        // Warm, then another call-site type: the exact check still refuses.
        try {
            Object o = (Object) getter.invokeExact((Object) box);
            System.out.println("wrong-type no-throw " + o);
        } catch (Throwable t) {
            System.out.println("wrong-type " + t.getClass().getName());
        }

        int s = (int) sGetter.invokeExact();
        s = (int) sGetter.invokeExact();
        System.out.println("static-getter " + s);

        final int n = 20000;
        row("getter-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                sum += (int) getter.invokeExact(box);
            }
            return sum;
        });
        row("getter-warm-invoke", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                sum += (int) getter.invoke(box);
            }
            return sum;
        });
        row("setter-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                setter.invokeExact(box, 7);
                sum += 1;
            }
            return sum;
        });
        row("static-getter-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i++) {
                sum += (int) sGetter.invokeExact();
            }
            return sum;
        });
        row("alternate-warm-exact", reps, n, k -> {
            long sum = 0;
            for (int i = 0; i < k; i += 2) {
                sum += read(getter, box);
                sum += read(getterW, box);
            }
            return sum;
        });
    }
}
