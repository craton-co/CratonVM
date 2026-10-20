// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 80, lane L4: a converting `invoke` on a warm REAL
// field getter (`DirectMethodHandle$Accessor` / `$StaticAccessor`, what the
// `Lookup` field finders answer under `--jdk-only`), with and without
//
//   CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE=1  the getter at a site that
//       differs from its type only by `asType`'s boxing / widening / receiver
//       cast is served from the handle's record once the handle's
//       `asTypeCache` holds that site's adapter (proposal
//       docs/internal/fixed-bugs/interpreter-i73-L4-proposal-converting-invoke-of-a-real-field-accessor-is-served-from-its-record-DONE-20261112.md).
//
// Rows (one handle per row, so each row's `asTypeCache` holds its own site's
// adapter; every answer is summed, so the checksums are the loop sums on
// HotSpot and here):
//   getter-warm-exact      (int) get.invokeExact(box)             control (exact, not served)
//   getter-warm-object     (Integer)(Object) get.invoke(box)      box at (Box)Object
//   getter-warm-long       (long) get.invoke(box)                 widen at (Box)long
//   getter-warm-double     (double) get.invoke(box)               widen at (Box)double
//   getter-warm-number     ((Number) get.invoke(box)).intValue()  box at (Box)Number
//   getter-object-receiver (int) get.invoke((Object) box)         receiver cast at (Object)int
//   static-warm-object     (Integer)(Object) sget.invoke()        box at ()Object
//
// Then deterministic behaviour rows (each warms its handle at the site first):
//   wrong-receiver   (Object)int site, a String receiver: Class.cast's CCE
//   null-receiver    (Object)int site, a null receiver: checkBase's NPE
//   box-identity     small value boxes to the cached Integer, a large one not
//   boolean-box      a boolean field at an Object site is Boolean.TRUE itself
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"   every loop, min/median over reps
//   "checksum <name> <value>"               deterministic (rep 0)
//   "<behaviour row>: <text>"               deterministic
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). The row-set finishes well under 60 s.
//
// A/B, `--jdk-only --nojit` (the rows the switch moves), `--jdk-only` with the
// JIT, and `--compatible --nojit` (control: carrier handles, nothing served):
//   A: (none)        B: CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE=1
// Expected under B, `--jdk-only`: the five converting getter rows and
// `static-warm-object` toward `getter-warm-exact`; `getter-warm-exact` does
// not move.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum getter-warm-exact 120000
// checksum getter-warm-object 120000
// checksum getter-warm-long 120000
// checksum getter-warm-double 120000
// checksum getter-warm-number 120000
// checksum getter-object-receiver 120000
// checksum static-warm-object 80000
// wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.String to L4W80FieldConvertingBench$Box
// null-receiver: java.lang.NullPointerException
// box-identity: true false
// boolean-box: true
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L4W80FieldConvertingBench {
    public static class Box {
        public static int S = 2;
        public int v = 3;
        public boolean z = true;
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static double median(double[] xs) {
        double[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2;
    }

    static double min(double[] xs) {
        double best = Double.MAX_VALUE;
        for (double x : xs) best = Math.min(best, x);
        return best;
    }

    static void single(String name, Body b, int reps, int n) throws Throwable {
        b.run(Math.max(2, n / 10));
        double[] xs = new double[reps];
        long first = 0;
        for (int rep = 0; rep < reps; rep++) {
            long t0 = System.nanoTime();
            long s = b.run(n);
            long t1 = System.nanoTime();
            xs[rep] = (t1 - t0) / (double) n;
            if (rep == 0) first = s;
        }
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
        System.out.println("checksum " + name + " " + first);
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out);
    }

    static MethodHandle getter(MethodHandles.Lookup lk) throws ReflectiveOperationException {
        return lk.findGetter(Box.class, "v", int.class);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Box box = new Box();
        int n = 40000;

        MethodHandle exact = getter(lk);
        single("getter-warm-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) exact.invokeExact(box);
            return s;
        }, reps, n);
        MethodHandle gObject = getter(lk);
        single("getter-warm-object", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) gObject.invoke(box);
            return s;
        }, reps, n);
        MethodHandle gLong = getter(lk);
        single("getter-warm-long", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (long) gLong.invoke(box);
            return s;
        }, reps, n);
        MethodHandle gDouble = getter(lk);
        single("getter-warm-double", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (long) (double) gDouble.invoke(box);
            return s;
        }, reps, n);
        MethodHandle gNumber = getter(lk);
        single("getter-warm-number", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += ((Number) gNumber.invoke(box)).intValue();
            return s;
        }, reps, n);
        MethodHandle gRecv = getter(lk);
        Object boxObject = box;
        single("getter-object-receiver", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) gRecv.invoke(boxObject);
            return s;
        }, reps, n);
        MethodHandle sget = lk.findStaticGetter(Box.class, "S", int.class);
        single("static-warm-object", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) sget.invoke();
            return s;
        }, reps, n);

        MethodHandle wrong = getter(lk);
        for (int i = 0; i < 4; i++) {
            int unused = (int) wrong.invoke((Object) box);
        }
        row("wrong-receiver", () -> (int) wrong.invoke((Object) "x"));
        row("null-receiver", () -> (int) wrong.invoke((Object) null));

        MethodHandle ident = getter(lk);
        row("box-identity", () -> {
            Box b = new Box();
            for (int i = 0; i < 4; i++) {
                Object unused = (Object) ident.invoke(b);
            }
            boolean small = (Object) ident.invoke(b) == (Object) ident.invoke(b);
            b.v = 100000;
            boolean large = (Object) ident.invoke(b) == (Object) ident.invoke(b);
            return small + " " + large;
        });
        MethodHandle zget = lk.findGetter(Box.class, "z", boolean.class);
        row("boolean-box", () -> {
            for (int i = 0; i < 4; i++) {
                Object unused = (Object) zget.invoke(box);
            }
            return (Object) zget.invoke(box) == Boolean.TRUE;
        });
    }
}
