// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- the exception type and text of
// every VM-built runtime exception a bytecode or a VM native raises, cold and
// after warm-up (the compiled site), in both modes:
//
//   negative-size   newarray / anewarray / multianewarray (every dimension is
//                   checked, a zero outer dimension included), the reflective
//                   Array.newInstance forms (void, no dimensions, 256 dims)
//   divide          idiv / irem / ldiv / lrem by zero, and the non-trapping
//                   MIN_VALUE / -1 forms
//   bounds          each array kind, negative and too-large indexes
//   arraycopy       every System.arraycopy refusal (type, index, length)
//   aastore         ArrayStoreException naming the stored element's class
//   cast            ClassCastException module/loader clauses: app, boot,
//                   platform, arrays of each, interfaces, joint vs split
//   clone           CloneNotSupportedException names the class
//   monitor         IllegalMonitorStateException for wait/notify unowned
//   vm-limit        OutOfMemoryError "Requested array size exceeds VM limit"
//
// Each row prints "<label>: <exception class>: <message>" (or "<label>: ok").
// A warm text that differs adds "<label> WARM: ..." (HotSpot prints none).
//
//   $J/javac -d out tools/probes/interp/L9/L9W98VmErrorTexts.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W98VmErrorTexts
//   $J/java -cp out L9W98VmErrorTexts
//
// Expected HotSpot 25 lines (a selection; the HotSpot run is the reference):
//   multi-zero-outer: java.lang.NegativeArraySizeException: -3
//   reflect-void: java.lang.IllegalArgumentException: null
//   reflect-multi-neg: java.lang.NegativeArraySizeException: -4
//   idiv: java.lang.ArithmeticException: / by zero
//   iaload-neg: java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
//   copy-int-to-object: java.lang.ArrayStoreException: arraycopy: type mismatch: can not copy int[] into object array[]
//   copy-last-src: java.lang.ArrayIndexOutOfBoundsException: arraycopy: last source index 7 out of bounds for int[5]
//   copy-null-src: java.lang.NullPointerException: null
//   aastore-integer: java.lang.ArrayStoreException: java.lang.Integer
//   cast-app: java.lang.ClassCastException: class L9W98VmErrorTexts$A cannot be cast to class L9W98VmErrorTexts$B (L9W98VmErrorTexts$A and L9W98VmErrorTexts$B are in unnamed module of loader 'app')
//   clone: java.lang.CloneNotSupportedException: L9W98VmErrorTexts$NoClone
//   wait-unowned: java.lang.IllegalMonitorStateException: current thread is not owner
//   vm-limit-int: java.lang.OutOfMemoryError: Requested array size exceeds VM limit
import java.lang.reflect.Array;
import java.util.Objects;

public class L9W98VmErrorTexts {
    static class A {}

    static class B extends A {}

    interface I {}

    static class NoClone {
        Object copy() throws CloneNotSupportedException {
            return clone();
        }
    }

    interface Shape {
        Object run(boolean fail) throws Throwable;
    }

    static String text(Shape s) {
        try {
            Object r = s.run(true);
            return "ok";
        } catch (Throwable t) {
            // A lambda proxy's name carries an address-like id: normalised.
            return (t.getClass().getName() + ": " + t.getMessage())
                    .replaceAll("\\$\\$Lambda/0x[0-9a-f]+", "\\$\\$Lambda/0xN");
        }
    }

    static void row(String label, Shape s) {
        String cold = text(s);
        for (int i = 0; i < 12000; i++) {
            try {
                s.run(false);
            } catch (Throwable t) {
                System.out.println(label + " WARMUP-THREW " + t);
                return;
            }
        }
        String warm = text(s);
        System.out.println(label + ": " + cold);
        // A message-less warm exception is HotSpot's OmitStackTraceInFastThrow
        // (a preallocated exception of a hot compiled site): not compared.
        boolean fastThrow = warm.endsWith(": null") && !cold.endsWith(": null");
        if (!fastThrow && !Objects.equals(cold, warm)) {
            System.out.println(label + " WARM: " + warm);
        }
    }

    static void once(String label, Shape s) {
        System.out.println(label + ": " + text(s));
    }

    static int n(boolean fail, int bad, int good) {
        return fail ? bad : good;
    }

    static Object pick(boolean fail, Object bad, Object good) {
        return fail ? bad : good;
    }

    static final int[] I5 = new int[5];
    static final Object[] O5 = new Object[5];
    static final String[] S5 = new String[5];

    public static void main(String[] args) throws Throwable {
        // ---- negative sizes ----
        row("newarray", f -> new int[n(f, -1, 1)]);
        row("anewarray", f -> new String[n(f, -2, 1)]);
        row("multi-first", f -> new int[n(f, -1, 1)][n(f, -2, 1)]);
        row("multi-second", f -> new int[2][n(f, -5, 1)]);
        row("multi-zero-outer", f -> new int[0][n(f, -3, 1)]);
        row("multi-zero-outer-3", f -> new int[0][1][n(f, -7, 1)]);
        row("multi-ref", f -> new String[1][n(f, -9, 1)]);
        once("reflect-neg", f -> Array.newInstance(int.class, -1));
        once("reflect-ref-neg", f -> Array.newInstance(String.class, -6));
        once("reflect-multi-neg", f -> Array.newInstance(int.class, 2, -4));
        once("reflect-multi-zero-neg", f -> Array.newInstance(int.class, 0, -8));
        once("reflect-void", f -> Array.newInstance(void.class, 1));
        once("reflect-void-multi", f -> Array.newInstance(void.class, 1, 1));
        once("reflect-no-dims", f -> Array.newInstance(int.class, new int[0]));
        once("reflect-256-dims", f -> Array.newInstance(int.class, new int[256]));
        once("reflect-255-dims", f -> Array.newInstance(int.class, new int[255]).getClass().getName().length());
        once("reflect-null-type", f -> Array.newInstance(null, 1));
        once("reflect-deep-component", f -> Array.newInstance(int[][].class, new int[254]));

        // ---- division ----
        row("idiv", f -> 7 / n(f, 0, 1));
        row("irem", f -> 7 % n(f, 0, 1));
        row("ldiv", f -> 7L / (long) n(f, 0, 1));
        row("lrem", f -> 7L % (long) n(f, 0, 1));
        row("idiv-min", f -> Integer.MIN_VALUE / n(f, -1, 1));
        row("lrem-min", f -> Long.MIN_VALUE % (long) n(f, -1, 1));
        row("floordiv", f -> Math.floorDiv(7, n(f, 0, 1)));
        row("floormod-long", f -> Math.floorMod(7L, (long) n(f, 0, 1)));

        // ---- array bounds ----
        int[] i3 = new int[3];
        long[] l2 = new long[2];
        byte[] b1 = new byte[1];
        char[] c4 = new char[4];
        row("iaload-neg", f -> i3[n(f, -1, 0)]);
        row("iaload-high", f -> i3[n(f, 3, 0)]);
        row("laload", f -> l2[n(f, 2, 0)]);
        row("aaload", f -> O5[n(f, 5, 0)]);
        row("bastore", f -> { b1[n(f, 1, 0)] = 1; return null; });
        row("castore-neg", f -> { c4[n(f, Integer.MIN_VALUE, 0)] = 'x'; return null; });
        row("aastore-high", f -> { S5[n(f, 99, 0)] = "s"; return null; });

        // ---- System.arraycopy ----
        row("copy-null-src", f -> { System.arraycopy(pick(f, null, I5), 0, I5, 0, 1); return null; });
        row("copy-null-dst", f -> { System.arraycopy(I5, 0, pick(f, null, I5), 0, 1); return null; });
        row("copy-src-not-array", f -> { System.arraycopy(pick(f, "s", I5), 0, I5, 0, 1); return null; });
        row("copy-dst-not-array", f -> { System.arraycopy(I5, 0, pick(f, "s", I5), 0, 1); return null; });
        row("copy-obj-dst-not-array", f -> { System.arraycopy(O5, 0, pick(f, "s", O5), 0, 1); return null; });
        row("copy-int-to-object", f -> { System.arraycopy(I5, 0, pick(f, O5, I5), 0, 1); return null; });
        row("copy-object-to-int", f -> { System.arraycopy(O5, 0, pick(f, I5, O5), 0, 1); return null; });
        row("copy-int-to-long", f -> { System.arraycopy(I5, 0, pick(f, new long[5], I5), 0, 1); return null; });
        row("copy-src-neg", f -> { System.arraycopy(I5, n(f, -1, 0), I5, 0, 1); return null; });
        row("copy-dst-neg", f -> { System.arraycopy(I5, 0, I5, n(f, -2, 0), 1); return null; });
        row("copy-len-neg", f -> { System.arraycopy(I5, 0, I5, 0, n(f, -3, 1)); return null; });
        row("copy-last-src", f -> { System.arraycopy(I5, n(f, 2, 0), new int[9], 0, 5); return null; });
        row("copy-last-dst", f -> { System.arraycopy(new int[9], 0, I5, n(f, 3, 0), 5); return null; });
        row("copy-obj-src-neg", f -> { System.arraycopy(O5, n(f, -1, 0), O5, 0, 1); return null; });
        row("copy-obj-last-dst", f -> { System.arraycopy(O5, 0, O5, n(f, 4, 0), 2); return null; });
        row("copy-overflow", f -> { System.arraycopy(I5, n(f, Integer.MAX_VALUE, 0), I5, 0, 2); return null; });
        Object[] mixed = {"a", Integer.valueOf(1)};
        row("copy-element-mismatch", f -> {
            System.arraycopy(pick(f, mixed, new Object[] {"a", "b"}), 0, new String[2], 0, 2);
            return null;
        });
        row("copy-type-mismatch-ref", f -> {
            System.arraycopy(pick(f, new Integer[] {7}, new String[1]), 0, new String[1], 0, 1);
            return null;
        });
        row("copy-two-dim-mismatch", f -> {
            System.arraycopy(pick(f, new int[1][1], new String[1][1]), 0, new String[1][], 0, 1);
            return null;
        });

        // ---- aastore ----
        Object[] strings = new String[1];
        row("aastore-integer", f -> { strings[0] = pick(f, Integer.valueOf(3), "s"); return null; });
        row("aastore-array", f -> { strings[0] = pick(f, new int[1], "s"); return null; });
        row("aastore-ref-array", f -> { strings[0] = pick(f, new Integer[1], "s"); return null; });
        Object[] nested = new String[1][];
        row("aastore-nested", f -> { nested[0] = pick(f, new Object[1], new String[1]); return null; });
        Object[] ifaces = new Runnable[1];
        row("aastore-interface", f -> { ifaces[0] = pick(f, "s", (Runnable) () -> {}); return null; });

        // ---- checkcast ----
        row("cast-app", f -> (B) pick(f, new A(), new B()));
        row("cast-app-to-boot", f -> (Integer) pick(f, new A(), 1));
        row("cast-boot-to-app", f -> (A) pick(f, "s", new B()));
        row("cast-boot", f -> (Integer) pick(f, "s", 1));
        row("cast-interface", f -> (I) pick(f, new A(), new I() {}));
        row("cast-boot-interface", f -> (Runnable) pick(f, "s", (Runnable) () -> {}));
        row("cast-platform", f -> (String) pick(f, new java.sql.Date(0), "s"));
        row("cast-array-boot", f -> (String[]) pick(f, new Object[1], new String[1]));
        row("cast-prim-array", f -> (String[]) pick(f, new int[1], new String[1]));
        row("cast-app-array", f -> (B[]) pick(f, new A[1], new B[1]));
        row("cast-array-to-class", f -> (A) pick(f, new A[1], new A()));
        row("cast-2d-prim", f -> (long[][]) pick(f, new int[1][1], new long[1][1]));
        row("cast-app-array-to-boot-array", f -> (Integer[]) pick(f, new A[1], new Integer[1]));
        row("cast-lambda", f -> (String) pick(f, (Runnable) () -> {}, "s"));
        row("class-cast-method", f -> String.class.cast(pick(f, 1, "s")));

        // ---- clone / monitors ----
        once("clone", f -> new NoClone().copy());
        Object lock = new Object();
        once("wait-unowned", f -> { lock.wait(); return null; });
        once("wait-timed-unowned", f -> { lock.wait(5); return null; });
        once("notify-unowned", f -> { lock.notify(); return null; });
        once("notifyall-unowned", f -> { lock.notifyAll(); return null; });
        once("wait-negative", f -> { synchronized (lock) { lock.wait(-1); } return null; });
        once("wait-nanos-range", f -> { synchronized (lock) { lock.wait(1, 1000000); } return null; });
        once("sleep-negative", f -> { Thread.sleep(-1); return null; });

        // ---- VM limits ----
        once("vm-limit-int", f -> new int[Integer.MAX_VALUE]);
        once("vm-limit-object", f -> new Object[Integer.MAX_VALUE - 1]);
        once("vm-limit-multi", f -> new byte[2][Integer.MAX_VALUE]);
        once("vm-limit-reflect", f -> Array.newInstance(long.class, Integer.MAX_VALUE));
        once("vm-limit-reflect-multi", f -> Array.newInstance(int.class, 1, Integer.MAX_VALUE));
    }
}
