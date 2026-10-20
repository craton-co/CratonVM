// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 87, lane L4 (page
// `docs/internal/fixed-bugs/interpreter-i86-L4-lambda-and-switch-residuals-FIXED-20261120.md`,
// residual 2): primitive type patterns
// in `switch` (JEP 507, preview in JDK 25), which javac compiles to
// `SwitchBootstraps.typeSwitch` sites with primitive `Class` labels. JDK 25's
// generated switch decides each label by the SELECTOR type:
//   Object selector   `instanceof` the label's own box (`case int i` matches
//                     an Integer only, never a Byte, Short or Character);
//   int / long /      the value's exact convertibility
//   double selector   (`ExactConversionsSupport`: 16777217 is not an exact
//                     float, 2^60+1 is not an exact float, -0.0 is not an
//                     exact int, NaN is an exact float).
// (javac 25 refuses `case byte b` over an `Integer` selector, so the wrapper
// selector arm is not reachable from source.)
// CratonVM matched any int-family box for any int-family label and ignored
// exactness.
//
// Run: javac --enable-preview --release 25 -d . L4W87PrimitivePatternLabels.java
//      java --enable-preview -cp . L4W87PrimitivePatternLabels
//      cratonvm --enable-preview --jdk-only [--nojit] -cp . L4W87PrimitivePatternLabels
//      cratonvm --enable-preview --compatible [--nojit] -cp . L4W87PrimitivePatternLabels
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, --enable-preview)
// obj: int:5|byte:5|char:x|other:3|long:7|other:true
// lng: byte:5|int:100000|float:4.2949673E9|long:1152921504606846977|byte:-1
// prim: byte:5|char:200|int:16777217|float:1.6777216E7|byte:-3
// dbl: int:3|float:-0.0|double:0.1|float:NaN|float:1.0E10
// HOTSPOT_EXPECTED_END

public class L4W87PrimitivePatternLabels {
    static String obj(Object o) {
        return switch (o) {
            case byte b -> "byte:" + b;
            case char c -> "char:" + c;
            case int i -> "int:" + i;
            case long l -> "long:" + l;
            default -> "other:" + o;
        };
    }

    static String lng(long v) {
        return switch (v) {
            case byte b -> "byte:" + b;
            case int i -> "int:" + i;
            case float f -> "float:" + f;
            case long x -> "long:" + x;
        };
    }

    static String prim(int v) {
        return switch (v) {
            case byte b -> "byte:" + b;
            case char c -> "char:" + (int) c;
            case float f -> "float:" + f;
            case int x -> "int:" + x;
        };
    }

    static String dbl(double d) {
        return switch (d) {
            case int i -> "int:" + i;
            case float f -> "float:" + f;
            case double x -> "double:" + x;
        };
    }

    static String join(String[] parts) {
        return String.join("|", parts);
    }

    public static void main(String[] args) {
        Object[] objs = {5, (byte) 5, 'x', (short) 3, 7L, true};
        String[] out = new String[objs.length];
        for (int i = 0; i < objs.length; i++) {
            out[i] = obj(objs[i]);
        }
        System.out.println("obj: " + join(out));

        long[] ls = {5L, 100000L, 4294967296L, 1152921504606846977L, -1L};
        out = new String[ls.length];
        for (int i = 0; i < ls.length; i++) {
            out[i] = lng(ls[i]);
        }
        System.out.println("lng: " + join(out));

        int[] ps = {5, 200, 16777217, 16777216, -3};
        out = new String[ps.length];
        for (int i = 0; i < ps.length; i++) {
            out[i] = prim(ps[i]);
        }
        System.out.println("prim: " + join(out));

        double[] ds = {3.0, -0.0, 0.1, Double.NaN, 1e10};
        out = new String[ds.length];
        for (int i = 0; i < ds.length; i++) {
            out[i] = dbl(ds[i]);
        }
        System.out.println("dbl: " + join(out));
    }
}
