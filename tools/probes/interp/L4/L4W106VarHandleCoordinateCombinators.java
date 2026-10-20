// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L4 (hunt): the `VarHandle` coordinate
// combinators (`MethodHandles.insertCoordinates`, `dropCoordinates`,
// `permuteCoordinates`, `filterCoordinates`, `collectCoordinates`,
// `filterValue`) over a field, a static field, an array element and a byte
// view handle, their access through the combined handle, and their refusals.
// No probe exercised these factories (only `filterValue` in
// `L4W57VarHandleReturnCombinators`); in `--compatible` the targets are this
// VM's synthetic handles, which the JDK's `IndirectVarHandle` must accept.
// Suspected, not confirmed: the battery compares each line with HotSpot 25
// live. Every line is deterministic (no hashes, no timings).
//
// Run: javac -d out L4W106VarHandleCoordinateCombinators.java
//      java -cp out L4W106VarHandleCoordinateCombinators            (HotSpot)
//      cratonvm --jdk-only -cp out L4W106VarHandleCoordinateCombinators
//      cratonvm --compatible -cp out L4W106VarHandleCoordinateCombinators
//      (each also with --nojit)
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.List;

public class L4W106VarHandleCoordinateCombinators {
    static final class Box {
        int v;
        String s = "s0";
    }

    static int sv = 5;

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof VarHandle h ? shape(h) : String.valueOf(v);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static String shape(VarHandle h) {
        StringBuilder b = new StringBuilder("vh ").append(h.varType().getSimpleName()).append(" (");
        for (int i = 0; i < h.coordinateTypes().size(); i++) {
            if (i > 0) {
                b.append(',');
            }
            b.append(h.coordinateTypes().get(i).getSimpleName());
        }
        return b.append(')').toString();
    }

    static Box pick(Box[] boxes, int i) {
        return boxes[i];
    }

    static int twice(int x) {
        return x * 2;
    }

    static int half(int x) {
        return x / 2;
    }

    static int sum(int a, int b) {
        return a + b;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle field = lk.findVarHandle(Box.class, "v", int.class);
        VarHandle ref = lk.findVarHandle(Box.class, "s", String.class);
        VarHandle stat = lk.findStaticVarHandle(L4W106VarHandleCoordinateCombinators.class, "sv", int.class);
        VarHandle arr = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle view = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        MethodHandle pick = lk.findStatic(L4W106VarHandleCoordinateCombinators.class, "pick",
                MethodType.methodType(Box.class, Box[].class, int.class));
        MethodHandle twice = lk.findStatic(L4W106VarHandleCoordinateCombinators.class, "twice",
                MethodType.methodType(int.class, int.class));
        MethodHandle half = lk.findStatic(L4W106VarHandleCoordinateCombinators.class, "half",
                MethodType.methodType(int.class, int.class));
        MethodHandle sum = lk.findStatic(L4W106VarHandleCoordinateCombinators.class, "sum",
                MethodType.methodType(int.class, int.class, int.class));

        // insertCoordinates
        Box box = new Box();
        row("insert-field", () -> MethodHandles.insertCoordinates(field, 0, box));
        row("insert-field-set-get", () -> {
            VarHandle h = MethodHandles.insertCoordinates(field, 0, box);
            h.set(41);
            return (int) h.getAndAdd(1) + " " + box.v;
        });
        row("insert-array-index", () -> {
            int[] a = {1, 2, 3};
            VarHandle h = MethodHandles.insertCoordinates(arr, 1, 2);
            h.set(a, 30);
            return shape(h) + " " + (int) h.get(a) + " " + Arrays.toString(a);
        });
        row("insert-bad-pos", () -> MethodHandles.insertCoordinates(field, 1, box));
        row("insert-negative-pos", () -> MethodHandles.insertCoordinates(field, -1, box));
        row("insert-wrong-type", () -> MethodHandles.insertCoordinates(field, 0, "notabox"));
        row("insert-null-value-primitive", () -> MethodHandles.insertCoordinates(arr, 1, (Object) null));
        row("insert-static", () -> MethodHandles.insertCoordinates(stat, 0, 1));

        // dropCoordinates
        row("drop-field", () -> MethodHandles.dropCoordinates(field, 0, String.class, long.class));
        row("drop-field-get", () -> {
            VarHandle h = MethodHandles.dropCoordinates(field, 1, String.class);
            box.v = 9;
            return shape(h) + " " + (int) h.get(box, "ignored");
        });
        row("drop-static", () -> {
            VarHandle h = MethodHandles.dropCoordinates(stat, 0, int.class);
            return shape(h) + " " + (int) h.get(123);
        });
        row("drop-bad-pos", () -> MethodHandles.dropCoordinates(field, 2, int.class));
        row("drop-void", () -> MethodHandles.dropCoordinates(field, 0, void.class));

        // permuteCoordinates
        row("permute-array", () -> {
            VarHandle h = MethodHandles.permuteCoordinates(arr, List.<Class<?>>of(int.class, int[].class), 1, 0);
            int[] a = {7, 8};
            return shape(h) + " " + (int) h.get(1, a);
        });
        row("permute-dup", () -> {
            VarHandle h = MethodHandles.permuteCoordinates(view,
                    List.<Class<?>>of(byte[].class, int.class), 0, 1);
            byte[] b = {0, 0, 1, 2, 0, 0};
            return shape(h) + " " + (int) h.get(b, 2);
        });
        row("permute-bad-count", () -> MethodHandles.permuteCoordinates(arr,
                List.<Class<?>>of(int.class, int[].class), 1));
        row("permute-bad-index", () -> MethodHandles.permuteCoordinates(arr,
                List.<Class<?>>of(int.class, int[].class), 1, 2));
        row("permute-bad-type", () -> MethodHandles.permuteCoordinates(arr,
                List.<Class<?>>of(int.class, int[].class), 0, 1));

        // filterCoordinates
        row("filter-array-index", () -> {
            VarHandle h = MethodHandles.filterCoordinates(arr, 1, twice);
            int[] a = {0, 10, 20, 30, 40};
            return shape(h) + " " + (int) h.get(a, 2);
        });
        row("filter-bad-type", () -> MethodHandles.filterCoordinates(arr, 0, twice));
        row("filter-too-many", () -> MethodHandles.filterCoordinates(arr, 1, twice, twice));

        // collectCoordinates
        row("collect-pick", () -> {
            VarHandle h = MethodHandles.collectCoordinates(field, 0, pick);
            Box[] boxes = {new Box(), new Box()};
            boxes[1].v = 77;
            return shape(h) + " " + (int) h.get(boxes, 1);
        });
        row("collect-sum", () -> {
            VarHandle h = MethodHandles.collectCoordinates(arr, 1, sum);
            int[] a = {0, 1, 2, 3, 4, 5};
            return shape(h) + " " + (int) h.get(a, 2, 3);
        });
        row("collect-bad-return", () -> MethodHandles.collectCoordinates(field, 0, twice));

        // filterValue
        row("filterValue-field", () -> {
            VarHandle h = MethodHandles.filterValue(field, half, twice);
            h.set(box, 10);
            return shape(h) + " " + box.v + " " + (int) h.get(box);
        });
        row("filterValue-bad", () -> MethodHandles.filterValue(field, twice, pick));
        row("filterValue-ref-cas", () -> {
            VarHandle h = MethodHandles.insertCoordinates(ref, 0, box);
            boolean ok = h.compareAndSet("s0", "s1");
            return ok + " " + box.s + " " + h.accessModeType(VarHandle.AccessMode.COMPARE_AND_SET);
        });

        // access modes and toMethodHandle through a combined handle
        row("combined-modes", () -> {
            VarHandle h = MethodHandles.insertCoordinates(field, 0, box);
            return h.isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD) + " "
                    + h.accessModeType(VarHandle.AccessMode.GET_AND_ADD) + " "
                    + h.toMethodHandle(VarHandle.AccessMode.GET).type();
        });
        row("combined-hasInvokeExact", () -> {
            VarHandle h = MethodHandles.insertCoordinates(field, 0, box).withInvokeExactBehavior();
            return h.hasInvokeExactBehavior() + " " + shape(h);
        });
        row("combined-exact-mismatch", () -> {
            VarHandle h = MethodHandles.insertCoordinates(field, 0, box).withInvokeExactBehavior();
            return (Object) (long) h.get();
        });
        row("null-target", () -> MethodHandles.insertCoordinates(null, 0, 1));
    }
}
