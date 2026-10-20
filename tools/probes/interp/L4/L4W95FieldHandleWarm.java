// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 95, lane L4: why a WARM field getter / setter
// handle costs ~40 us per `invokeExact` in both modes (`L4W94FieldLinkFirst`,
// HotSpot 3-4 ns), and the two default-off switches built for it (page
// `docs/internal/fixed-bugs/interpreter-i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-FIXED-20261018.md`).
//
// The `invokeExact` / `invoke` natives run the JDK's invoker linker
// (`lang_invoke::mh_linked_invoker`) for every REAL handle, ahead of every
// fast lane. A call site naming an application class (`(LBox;)I`) never gets
// a linker-table row, so EVERY call runs `Invokers.methodHandleInvokeLinkerMethod`
// and three full root refreshes before the handle's whole `LambdaForm` chain.
//
//   CRATONVM_MH_LINKER_BASIC_MEMBER=1       the member row per basic type
//   CRATONVM_MH_REAL_FIELD_BEFORE_LINKER=1  a field accessor served ahead of the linker
//
// This file is both the behaviour probe (every line must be the same in every
// arm and both modes) and the bench. Rows (`TIME <name> min <ns> med <ns> ns/op`, 20000 calls
// per rep; wave 103 moved the timings onto TIME lines: filter them before diffing):
//   getter-warm-exact        `(LBox;)I` site: both switches act
//   getter-warm-invoke       the same through `invoke`
//   setter-warm-exact        `(LBox;I)V`
//   ref-setter-warm-exact    `(LBox;Ljava/lang/String;)V`, a String value
//   static-getter-warm-exact `()I` site: the linker's pair row hits (only the
//                            field switch acts: the LambdaForm chain's cost alone)
//   method-warm-exact        findVirtual `(LBox;)I`: only the basic-member switch acts
//   bound-getter-warm-exact  a BoundMethodHandle at `()I`: control, neither acts
//
// Census (names the per-call cost): CRATONVM_DBG=mh-finder-census, then
//   grep '^\[mh-finder-census\] timed' -> `linker invokeExact miss-user-site`,
//   `linker invokeExact link-to`, `lane field-before-linker`, ...
//
// Run: javac -d . L4W95FieldHandleWarm.java
//      java -cp . L4W95FieldHandleWarm                       (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W95FieldHandleWarm [time] [reps]
//      cratonvm --compatible [--nojit] -cp . L4W95FieldHandleWarm [time] [reps]
// Wave 105: the TIME lines are printed only with the `time` argument (reps then
// default 5; without it 1), so a plain run prints exactly HotSpot's lines.
// A/B: pass `time`.
//
// Expected values from JDK 25's `DirectMethodHandle$Accessor` /
// `$StaticAccessor` semantics (a plain read or write; `checkBase`'s NPE; the
// static barrier runs `<clinit>` once, before the first read; `invokeExact` at
// another type is a `WrongMethodTypeException`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; derived from the sources)
// getter-exact 7
// getter-invoke 7
// getter-invoke-object 7
// setter-exact 9
// ref-setter-exact hello
// iface-setter-exact run
// volatile-getter-exact 12
// lazy-first clinit-ran 41
// lazy-second 41
// static-getter-exact 5
// method-exact 8
// bound-getter-exact 7
// null-receiver java.lang.NullPointerException
// wrong-type java.lang.invoke.WrongMethodTypeException
// sub-getter-exact 11
// checksum getter-warm-exact 140000
// checksum getter-warm-invoke 140000
// checksum setter-warm-exact 20000
// checksum ref-setter-warm-exact 1
// checksum static-getter-warm-exact 100000
// checksum method-warm-exact 160000
// checksum bound-getter-warm-exact 140000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.Locale;

public class L4W95FieldHandleWarm {
    public static class Box {
        public static int S = 5;
        public int v = 7;
        public String s = "";
        public Runnable r;
        public volatile int vol = 12;

        public int plusOne() {
            return v + 1;
        }
    }

    public static class Sub extends Box {
        public Sub() {
            v = 11;
        }
    }

    public static class Lazy {
        public static int X;

        static {
            System.out.print("clinit-ran ");
            X = 41;
        }
    }

    static final class Task implements Runnable {
        @Override
        public void run() {}

        @Override
        public String toString() {
            return "run";
        }
    }

    /** Wave 105: print TIME lines (the `time` argument). */
    static boolean timing;

    interface Body {
        long run(int n) throws Throwable;
    }

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
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle getter = lookup.findGetter(Box.class, "v", int.class);
        MethodHandle setter = lookup.findSetter(Box.class, "v", int.class);
        MethodHandle refSetter = lookup.findSetter(Box.class, "s", String.class);
        MethodHandle ifaceSetter = lookup.findSetter(Box.class, "r", Runnable.class);
        MethodHandle volGetter = lookup.findGetter(Box.class, "vol", int.class);
        MethodHandle sGetter = lookup.findStaticGetter(Box.class, "S", int.class);
        MethodHandle lazyGetter = lookup.findStaticGetter(Lazy.class, "X", int.class);
        MethodHandle method = lookup.findVirtual(Box.class, "plusOne", MethodType.methodType(int.class));
        Box box = new Box();
        MethodHandle bound = getter.bindTo(box);

        // Behaviour: each line is asked twice where a first call links and a
        // second is the warm call a switch serves.
        int a = (int) getter.invokeExact(box);
        a = (int) getter.invokeExact(box);
        System.out.println("getter-exact " + a);
        int b = (int) getter.invoke(box);
        b = (int) getter.invoke(box);
        System.out.println("getter-invoke " + b);
        Object o = getter.invoke(box);
        System.out.println("getter-invoke-object " + o);
        setter.invokeExact(box, 8);
        setter.invokeExact(box, 9);
        System.out.println("setter-exact " + box.v);
        refSetter.invokeExact(box, "hi");
        refSetter.invokeExact(box, "hello");
        System.out.println("ref-setter-exact " + box.s);
        Runnable task = new Task();
        ifaceSetter.invokeExact(box, task);
        ifaceSetter.invokeExact(box, task);
        System.out.println("iface-setter-exact " + box.r);
        int vol = (int) volGetter.invokeExact(box);
        vol = (int) volGetter.invokeExact(box);
        System.out.println("volatile-getter-exact " + vol);
        // The static barrier: `<clinit>` runs inside the first call, before
        // its read (the print lands before the value on the same line).
        System.out.print("lazy-first ");
        int lazy = (int) lazyGetter.invokeExact();
        System.out.println(lazy);
        System.out.println("lazy-second " + (int) lazyGetter.invokeExact());
        int s = (int) sGetter.invokeExact();
        s = (int) sGetter.invokeExact();
        System.out.println("static-getter-exact " + s);
        int m = (int) method.invokeExact(box);
        m = (int) method.invokeExact(box);
        box.v = 7;
        System.out.println("method-exact " + (int) method.invokeExact(box));
        int bb = (int) bound.invokeExact();
        bb = (int) bound.invokeExact();
        System.out.println("bound-getter-exact " + bb);
        try {
            int n = (int) getter.invokeExact((Box) null);
            System.out.println("null-receiver no-throw " + n);
        } catch (Throwable t) {
            System.out.println("null-receiver " + t.getClass().getName());
        }
        try {
            long w = (long) getter.invokeExact(box);
            System.out.println("wrong-type no-throw " + w);
        } catch (Throwable t) {
            System.out.println("wrong-type " + t.getClass().getName());
        }
        Box sub = new Sub();
        System.out.println("sub-getter-exact " + (int) getter.invokeExact(sub));

        // Bench rows (box.v is 7 here).
        row("getter-warm-exact", reps, 20000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) getter.invokeExact(box);
            }
            return acc;
        });
        row("getter-warm-invoke", reps, 20000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) getter.invoke(box);
            }
            return acc;
        });
        row("setter-warm-exact", reps, 20000, n -> {
            for (int i = 0; i < n; i++) {
                setter.invokeExact(box, i);
            }
            return box.v + 1;
        });
        String hello = "hello";
        row("ref-setter-warm-exact", reps, 20000, n -> {
            for (int i = 0; i < n; i++) {
                refSetter.invokeExact(box, hello);
            }
            return box.s == hello ? 1 : 0;
        });
        row("static-getter-warm-exact", reps, 20000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) sGetter.invokeExact();
            }
            return acc;
        });
        box.v = 7;
        row("method-warm-exact", reps, 20000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) method.invokeExact(box);
            }
            return acc;
        });
        row("bound-getter-warm-exact", reps, 20000, n -> {
            long acc = 0;
            for (int i = 0; i < n; i++) {
                acc += (int) bound.invokeExact();
            }
            return acc;
        });
    }
}
