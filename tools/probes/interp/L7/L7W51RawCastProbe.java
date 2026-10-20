// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L7: the raw cast-site probe
// (`CRATONVM_CAST_HIT_RAW`, proposal i50-L7): `checkcast` / `instanceof`
// read the operand's slot raw and the receiver's header in one validated
// read, and `instanceof`'s hit writes its answer over the operand. Every
// shape the memos answer, cold (r0) and warm (r1..r3), one site per shape:
//
//   neg        instanceof refused twice at one site (negative memo), then
//              admitted by another class at the same site
//   under      instanceof with a long and an int live under the operand
//              (the in-place answer must not disturb them)
//   arrkeys    (Object[]) on a String[] then an Integer[] at one site
//              (array memo key change), instanceof Object[] on an int[]
//   arriface   Object[] instanceof Cloneable / Serializable / Runnable
//   poly       three receiver classes through one checkcast site
//   lambda     a lambda instanceof its interface and an unrelated one
//   cce        the ClassCastException message of a warm refusing site
//
// The positive control is `CRATONVM_DBG_FIELD_SITE=1`: its FINAL line's
// `cast-raw-hit:` is non-zero with the switch on, 0 with
// `CRATONVM_CAST_HIT_RAW=0`.
//
// Expected (HotSpot 25.0.3, `java L7W51RawCastProbe`, and `-Xint`):
//   r0 neg false false true
//   r0 under 42 3 1
//   r0 arrkeys 2 1 false
//   r0 arriface true true false
//   r0 poly 6
//   r0 lambda true false
//   r0 cce class L7W51RawCastProbe$Other cannot be cast to class L7W51RawCastProbe$Base (L7W51RawCastProbe$Other and L7W51RawCastProbe$Base are in unnamed module of loader 'app')
//   r1 neg false false true
//   r1 under 42 3 1
//   r1 arrkeys 2 1 false
//   r1 arriface true true false
//   r1 poly 6
//   r1 lambda true false
//   r1 cce class L7W51RawCastProbe$Other cannot be cast to class L7W51RawCastProbe$Base (L7W51RawCastProbe$Other and L7W51RawCastProbe$Base are in unnamed module of loader 'app')
//   r2 neg false false true
//   r2 under 42 3 1
//   r2 arrkeys 2 1 false
//   r2 arriface true true false
//   r2 poly 6
//   r2 lambda true false
//   r2 cce class L7W51RawCastProbe$Other cannot be cast to class L7W51RawCastProbe$Base (L7W51RawCastProbe$Other and L7W51RawCastProbe$Base are in unnamed module of loader 'app')
//   r3 neg false false true
//   r3 under 42 3 1
//   r3 arrkeys 2 1 false
//   r3 arriface true true false
//   r3 poly 6
//   r3 lambda true false
//   r3 cce class L7W51RawCastProbe$Other cannot be cast to class L7W51RawCastProbe$Base (L7W51RawCastProbe$Other and L7W51RawCastProbe$Base are in unnamed module of loader 'app')
public class L7W51RawCastProbe {
    static class Base {
        int v() {
            return 1;
        }
    }

    static final class SubA extends Base {}

    static final class SubB extends Base {
        int v() {
            return 2;
        }
    }

    static final class SubC extends Base {
        int v() {
            return 3;
        }
    }

    static final class Other {}

    static Object pick(Object o) {
        return o;
    }

    static boolean negTest(Object o) {
        return o instanceof Base;
    }

    static String underRow(long l, int i, Object o) {
        // `l` and `i` are on the operand stack under `o` when the
        // `instanceof` runs: `l + i + (o instanceof Base ? 1 : 0)` is
        // evaluated left to right with both still live.
        long sum = l + (i + (o instanceof Base ? 1 : 0));
        return sum + " " + i + " " + (o instanceof Base ? 1 : 0);
    }

    static int arrCast(Object o) {
        return ((Object[]) o).length;
    }

    static boolean arrTest(Object o) {
        return o instanceof Object[];
    }

    static String arrIfaceRow(Object o) {
        return (o instanceof Cloneable) + " " + (o instanceof java.io.Serializable)
                + " " + (o instanceof Runnable);
    }

    static int polyCast(Object o) {
        return ((Base) o).v();
    }

    static String lambdaRow(Object o) {
        return (o instanceof Runnable) + " " + (o instanceof java.util.function.Supplier);
    }

    static String cceRow(Object o) {
        try {
            return "cast " + ((Base) o).v();
        } catch (ClassCastException e) {
            return e.getMessage();
        }
    }

    public static void main(String[] args) {
        Runnable lambda = () -> {};
        for (int r = 0; r < 4; r++) {
            String p = "r" + r + " ";
            System.out.println(p + "neg " + negTest(pick(new Other())) + " "
                    + negTest(pick(new Other())) + " " + negTest(pick(new SubA())));
            System.out.println(p + "under " + underRow(38L, 3, pick(new SubA())));
            System.out.println(p + "arrkeys " + arrCast(pick(new String[] {"a", "b"})) + " "
                    + arrCast(pick(new Integer[] {1})) + " " + arrTest(pick(new int[2])));
            System.out.println(p + "arriface " + arrIfaceRow(pick(new String[0])));
            int sum = polyCast(pick(new SubA())) + polyCast(pick(new SubB()))
                    + polyCast(pick(new SubC()));
            System.out.println(p + "poly " + sum);
            System.out.println(p + "lambda " + lambdaRow(pick(lambda)));
            System.out.println(p + "cce " + cceRow(pick(new Other())));
        }
    }
}
