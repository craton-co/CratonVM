// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The reference-copy store checks, on the shape that breaks them: an element
// that is ITSELF AN ARRAY.
//
// H2's `SortOrder.sort` does `rows.toArray(new Value[0][])` on a list of
// `Value[]`. Under `--jdk-only` the `ArrayList.toArray([Ljava/lang/Object;)`
// synthetic stub is refused, the real `ArrayList` bytecode runs, and it calls
// `Arrays.copyOf(elementData, size, a.getClass())` -- whose native compared the
// element's class id against the destination component's. On a REFERENCE ARRAY
// the header's class id holds the COMPONENT class, so a `String[]` element
// answers `java/lang/String` while a `String[][]`'s component is
// `[Ljava/lang/String;`: never equal, so every `ORDER BY` died with a FALSE
// ArrayStoreException naming `java.lang.String`.
//
// The sibling check in `System.arraycopy` was wrong the other way: any array
// element was accepted into any array-of-array destination, so `String[]` into
// `Integer[][]` copied where HotSpot throws.
//
// So the sweep asks BOTH polarities on every rung. A probe that only checks
// "the legal store succeeds" cannot see the second defect, and one that only
// checks "the illegal store throws" cannot see the first.
//
//   javac -d out DodArrayStoreSweep.java
//   cratonvm --java-home "$JDK" --jdk-only -cp out DodArrayStoreSweep
//   diff <(java -cp out DodArrayStoreSweep) <(cratonvm ... DodArrayStoreSweep)
//
// Deterministic by construction: every value printed is a length, a class name
// or an exception message the program produced.

import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public final class DodArrayStoreSweep {

    private static int rows = 0;

    public static void main(String[] args) {
        types();
        copyOfArrayOfArray();
        arraycopy();
        toArray();
        mustThrow();
        System.out.println("DOD ROWS " + rows);
        System.out.println("DOD RESULT OK");
    }

    // The two rungs `Arrays.copyOf`'s real bytecode stands on. If either of
    // these is wrong the copy cannot be right, and the diff should say so here
    // rather than three sections later.
    private static void types() {
        for (Class<?> t : new Class<?>[] {
                String[].class, String[][].class, String[][][].class,
                Object[].class, Object[][].class,
                int[].class, int[][].class, Integer[][].class }) {
            Class<?> c = t.getComponentType();
            row("componentType " + t.getName(), () -> c == null ? "null" : c.getName());
        }
        for (Class<?> c : new Class<?>[] {
                String.class, String[].class, String[][].class,
                Object.class, Object[].class,
                int.class, int[].class, Integer[].class }) {
            row("newInstance " + c.getName(), () -> {
                Object a = Array.newInstance(c, 2);
                return a.getClass().getName() + " len=" + Array.getLength(a);
            });
        }
        row("newInstance(String,2,3)", () -> Array.newInstance(String.class, 2, 3)
                .getClass().getName());
        row("opcode new String[2][]", () -> new String[2][].getClass().getName());
        row("opcode new String[2][3]", () -> new String[2][3].getClass().getName());
        row("newInstance(String[].class,2) is String[][]", () ->
                String.valueOf(Array.newInstance(String[].class, 2).getClass()
                        == String[][].class));
    }

    // The failing family: a destination whose component type is itself an array.
    private static void copyOfArrayOfArray() {
        Object[] src = new Object[] { new String[] { "x" }, new String[] { "y" } };
        row("copyOf(Object[]{String[]},2,String[][])", () -> {
            String[][] out = Arrays.copyOf(src, 2, String[][].class);
            return out.length + "/" + out[0][0] + out[1][0];
        });
        row("copyOf(String[][],2,Object[][])", () -> {
            String[][] s = { { "p" }, { "q" } };
            Object[][] out = Arrays.copyOf(s, 2, Object[][].class);
            return out.length + "/" + out[1][0];
        });
        row("copyOf(Object[]{String[][]},1,String[][][])", () -> {
            Object[] s = { new String[][] { { "d3" } } };
            String[][][] out = Arrays.copyOf(s, 1, String[][][].class);
            return out[0][0][0];
        });
        row("copyOf(Object[]{int[]},1,int[][])", () -> {
            Object[] s = { new int[] { 7 } };
            int[][] out = Arrays.copyOf(s, 1, int[][].class);
            return String.valueOf(out[0][0]);
        });
        // Ordinary covariance, as the control: these never depended on the bug.
        row("copyOf(Object[]{Integer},1,Number[])", () ->
                Arrays.copyOf(new Object[] { 3 }, 1, Number[].class)[0].toString());
        row("copyOf(Object[]{String},1,Comparable[])", () ->
                Arrays.copyOf(new Object[] { "s" }, 1, Comparable[].class)[0].toString());
        row("copyOf(String[],2,Object[]) widen", () ->
                Arrays.copyOf(new String[] { "s", "t" }, 2, Object[].class).length + "");
        row("copyOf(Object[]{null,String},2,String[]) null element", () ->
                String.valueOf(Arrays.copyOf(new Object[] { null, "s" }, 2, String[].class)[1]));
        row("copyOf(Object[]{String},4,String[]) grow", () ->
                String.valueOf(Arrays.copyOf(new Object[] { "s" }, 4, String[].class)[3]));
        row("copyOf(Object[]{String},0,String[]) empty", () ->
                Arrays.copyOf(new Object[] { "s" }, 0, String[].class).length + "");
    }

    private static void arraycopy() {
        row("arraycopy Object[]{String[]} -> String[][]", () -> {
            String[][] d = new String[1][];
            System.arraycopy(new Object[] { new String[] { "s" } }, 0, d, 0, 1);
            return d[0][0];
        });
        row("arraycopy Object[]{String[][]} -> String[][][]", () -> {
            String[][][] d = new String[1][][];
            System.arraycopy(new Object[] { new String[][] { { "s" } } }, 0, d, 0, 1);
            return d[0][0][0];
        });
        row("arraycopy Object[]{int[]} -> int[][]", () -> {
            int[][] d = new int[1][];
            System.arraycopy(new Object[] { new int[] { 7 } }, 0, d, 0, 1);
            return String.valueOf(d[0][0]);
        });
        row("arraycopy int[][] -> int[][]", () -> {
            int[][] d = new int[1][];
            System.arraycopy(new int[][] { { 7 } }, 0, d, 0, 1);
            return String.valueOf(d[0][0]);
        });
    }

    private static void toArray() {
        List<String[]> l = new ArrayList<>();
        l.add(new String[] { "a", "b" });
        l.add(new String[] { "c", "d" });
        // The zero-length form routes through Arrays.copyOf; the pre-sized form
        // routes through System.arraycopy. They are different bugs and this is
        // the pair that tells them apart.
        row("toArray(new String[0][]) copyOf route", () -> {
            String[][] out = l.toArray(new String[0][]);
            return out.length + "/" + out[0][1];
        });
        row("toArray(new String[2][]) arraycopy route", () -> {
            String[][] out = l.toArray(new String[2][]);
            return out.length + "/" + out[1][0];
        });
        row("toArray() Object[] route", () -> l.toArray().getClass().getName());
        row("toArray(new int[0][])", () -> {
            List<int[]> il = new ArrayList<>();
            il.add(new int[] { 7 });
            return String.valueOf(il.toArray(new int[0][])[0][0]);
        });
        List<Object> mixed = new ArrayList<>();
        mixed.add(3);
        mixed.add(4L);
        row("toArray(new Number[0]) covariant", () -> {
            Number[] out = mixed.toArray(new Number[0]);
            return out[0] + "/" + out[1];
        });
    }

    /**
     * The other polarity. A sweep with no rejection rows reports a VM that
     * accepts everything as perfect.
     */
    private static void mustThrow() {
        row("aastore Integer into String[]", () -> {
            Object[] as = new String[1];
            as[0] = 1;
            return "NO-THROW";
        });
        row("aastore String[] into Integer[][]", () -> {
            Object[] as = new Integer[1][];
            as[0] = new String[] { "q" };
            return "NO-THROW";
        });
        row("copyOf(Object[]{String,Integer},2,String[])", () ->
                Arrays.copyOf(new Object[] { "s", 1 }, 2, String[].class).length + "");
        row("copyOf(Object[]{String[]},1,Integer[][])", () ->
                Arrays.copyOf(new Object[] { new String[] { "s" } }, 1,
                        Integer[][].class).length + "");
        row("copyOf(Object[]{Integer},1,String[][])", () ->
                Arrays.copyOf(new Object[] { 1 }, 1, String[][].class).length + "");
        row("copyOf(Object[]{Object},1,Comparable[])", () ->
                Arrays.copyOf(new Object[] { new Object() }, 1, Comparable[].class).length + "");
        row("copyOf(Object[]{long[]},1,int[][])", () ->
                Arrays.copyOf(new Object[] { new long[] { 7L } }, 1, int[][].class).length + "");
        row("arraycopy Object[]{Integer} -> String[]", () -> {
            System.arraycopy(new Object[] { 1 }, 0, new String[1], 0, 1);
            return "NO-THROW";
        });
        row("arraycopy Object[]{String[]} -> Integer[][]", () -> {
            System.arraycopy(new Object[] { new String[] { "s" } }, 0, new Integer[1][], 0, 1);
            return "NO-THROW";
        });
        row("arraycopy Object[]{Object} -> Comparable[]", () -> {
            System.arraycopy(new Object[] { new Object() }, 0, new Comparable<?>[1], 0, 1);
            return "NO-THROW";
        });
        row("arraycopy Object[]{long[]} -> int[][]", () -> {
            System.arraycopy(new Object[] { new long[] { 7L } }, 0, new int[1][], 0, 1);
            return "NO-THROW";
        });
        // The two toArray(T[]) routes throw from DIFFERENT places and so print
        // DIFFERENT messages, which a single case could not have shown:
        // ArrayList's goes through Arrays.copyOf / System.arraycopy and gets
        // the arraycopy sentence; AbstractCollection's stores through `aastore`
        // in its own loop and gets the bare class name.
        row("ArrayList.toArray(new String[0]) copyOf route", () -> {
            List<Object> l = new ArrayList<>();
            l.add(1);
            return l.toArray(new String[0]).length + "";
        });
        row("ArrayList.toArray(new String[2]) arraycopy route", () -> {
            List<Object> l = new ArrayList<>();
            l.add(1);
            return l.toArray(new String[2]).length + "";
        });
        row("HashSet.toArray(new String[0]) aastore route", () -> {
            java.util.Set<Object> hs = new java.util.HashSet<>();
            hs.add(1);
            return hs.toArray(new String[0]).length + "";
        });
        row("LinkedList.toArray(new String[0]) aastore route", () -> {
            List<Object> ll = new java.util.LinkedList<>();
            ll.add(1);
            return ll.toArray(new String[0]).length + "";
        });
        // The RESULT TYPE, which is a separate claim from the store check and
        // the one a `List<Object>` receiver hides: `toArray(T[])` is statically
        // `Object[]` there, so javac emits no `checkcast` and a wrong runtime
        // type never surfaces. Printing the class asserts it directly.
        row("ArrayList.toArray(new String[0]) legal + type", () -> {
            List<Object> l = new ArrayList<>();
            l.add("s");
            Object[] r = l.toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
        row("HashSet.toArray(new String[0]) legal + type", () -> {
            java.util.Set<Object> hs = new java.util.HashSet<>();
            hs.add("s");
            Object[] r = hs.toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
        row("LinkedList.toArray(new String[0]) legal + type", () -> {
            List<Object> ll = new java.util.LinkedList<>();
            ll.add("s");
            Object[] r = ll.toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
        row("TreeSet.toArray(new String[0]) legal + type", () -> {
            java.util.Set<String> ts = new java.util.TreeSet<>();
            ts.add("s");
            Object[] r = ts.toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
        row("ArrayDeque.toArray(new String[0]) legal + type", () -> {
            java.util.Deque<String> dq = new java.util.ArrayDeque<>();
            dq.add("s");
            Object[] r = dq.toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
        row("List.of(..).toArray(new String[0]) legal + type", () -> {
            Object[] r = List.of("s").toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
        row("HashMap.keySet().toArray(new String[0]) legal + type", () -> {
            java.util.Map<String, String> m = new java.util.HashMap<>();
            m.put("s", "v");
            Object[] r = m.keySet().toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
        row("HashMap.values().toArray(new String[0]) legal + type", () -> {
            java.util.Map<String, String> m = new java.util.HashMap<>();
            m.put("s", "v");
            Object[] r = m.values().toArray(new String[0]);
            return r.getClass().getName() + "/" + r[0];
        });
    }

    private interface Case {
        String run() throws Throwable;
    }

    /** The exception TEXT is part of the answer: HotSpot's copyOf reaches its
     *  ArrayStoreException through System.arraycopy, so both sites must print
     *  the same sentence. A right/wrong verdict alone would hide that. */
    private static void row(String label, Case c) {
        rows++;
        String got;
        try {
            got = c.run();
        } catch (Throwable t) {
            got = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println("DOD CASE " + label + " => " + got);
    }
}
