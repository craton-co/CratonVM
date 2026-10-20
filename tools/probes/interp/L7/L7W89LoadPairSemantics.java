// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 89, lane L7: what CRATONVM_INTERP_FUSED_LOAD_PAIR
// and CRATONVM_INTERP_FUSED_ICONST_GOTO must leave unchanged. Each shape runs
// 2000 times so the field sites are warm before the interesting call:
//   * `h.v = v` with h null in a static method (aload_0; iload_1; putfield):
//     the NPE is the putfield's, helpful message and all;
//   * a store into a final field from its own constructor, and two reference
//     stores (aload_0; aload_1; putfield) whose values are read back;
//   * a long and a double stored through aload_0; lload_1 (not fused) next to
//     an int one, to show the neighbouring shapes still store;
//   * an instance call aload_0; aload_1; invokevirtual whose argument is null;
//   * boolean materialisation in both directions.
//
// Compile with plain `javac` (no -g): without a LocalVariableTable the
// helpful NPE names the static method's first parameter `<parameter1>`.
// Run (all three must print the block below, with and without --nojit):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W89LoadPairSemantics
//   CRATONVM_INTERP_FUSED_LOAD_PAIR=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W89LoadPairSemantics
//   CRATONVM_INTERP_FUSED_ICONST_GOTO=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W89LoadPairSemantics
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// npe java.lang.NullPointerException: Cannot assign field "v" because "<parameter1>" is null warm=2000
// pair 1999 b x
// wide 7 9000000000 2.5
// call null 2000
// bool 999 1001
// HOTSPOT_EXPECTED_END
public class L7W89LoadPairSemantics {
    static final class Holder {
        int v;
    }

    static final class Pair {
        final int n;
        Object left;
        Object right;

        Pair(int n, Object l, Object r) {
            this.n = n;
            this.left = l;
            this.right = r;
        }
    }

    static final class Wide {
        int i;
        long l;
        double d;

        void set(int i, long l, double d) {
            this.i = i;
            this.l = l;
            this.d = d;
        }
    }

    static void store(Holder h, int v) {
        h.v = v;
    }

    String name(Object o) {
        return String.valueOf(o);
    }

    String call(Object o) {
        return this.name(o);
    }

    static boolean positive(int x) {
        return x > 0;
    }

    static boolean notPositive(int x) {
        return !(x > 0);
    }

    public static void main(String[] args) {
        Holder h = new Holder();
        int warm = 0;
        for (int k = 0; k < 2000; k++) {
            store(h, k);
            warm++;
        }
        try {
            store(null, 1);
            System.out.println("npe none");
        } catch (NullPointerException e) {
            System.out.println("npe " + e + " warm=" + warm);
        }

        Pair last = null;
        for (int k = 0; k < 2000; k++) {
            last = new Pair(k, "b", "x");
        }
        System.out.println("pair " + last.n + " " + last.left + " " + last.right);

        Wide w = new Wide();
        for (int k = 0; k < 2000; k++) {
            w.set(7, 9_000_000_000L, 2.5);
        }
        System.out.println("wide " + w.i + " " + w.l + " " + w.d);

        L7W89LoadPairSemantics self = new L7W89LoadPairSemantics();
        String got = null;
        int calls = 0;
        for (int k = 0; k < 2000; k++) {
            got = self.call(null);
            calls++;
        }
        System.out.println("call " + got + " " + calls);

        int yes = 0;
        int no = 0;
        for (int k = -1000; k < 1000; k++) {
            if (positive(k)) {
                yes++;
            }
            if (notPositive(k)) {
                no++;
            }
        }
        System.out.println("bool " + yes + " " + no);
    }
}
