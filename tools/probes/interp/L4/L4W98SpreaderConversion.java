// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L4 -- page
// `docs/internal/fixed-bugs/interpreter-i98-L4-as-spreader-element-conversion-not-judged-FIXED-20261130.md`.
//
// `MethodHandle.asSpreader(arrayType, n)` (JDK 25 `asSpreaderChecks`): after
// the array and length checks, every spread parameter must accept the array's
// element (`MethodType.canConvert(element, ptype)`); otherwise `asType(needType)`
// is called only to raise its `WrongMethodTypeException`. CratonVM's
// two-argument `asSpreader` native (a Bridge, kept by `--jdk-only`) built the
// spreader instead. The three-argument form already judged it; both are here,
// with the neighbouring texts for context.
//
// No setup.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the JDK sources)
// spread-int-into-strings: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String)String to (int,int)String
// spread-int-into-last-string: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String,String)String to (String,String,int)String
// spread-at-int-into-strings: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String,String,String)String to (int,int,String)String
// spread-int-into-longs: 5
// spread-int-into-longs-type: (int[])long
// spread-objects-into-strings: ab
// spread-wrong-length-call: java.lang.IllegalArgumentException: array is not of length 2
// spread-null-call: java.lang.NullPointerException: null array reference
// spread-not-array: java.lang.IllegalArgumentException: not an array type: class java.lang.String
// spread-bad-length: java.lang.IllegalArgumentException: bad spread array length
// spread-bad-position: java.lang.IllegalArgumentException: bad spread position
// spread-to-string: MethodHandle(Object[])String
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W98SpreaderConversion {
    static String two(String a, String b) {
        return a + b;
    }

    static String three(String a, String b, String c) {
        return a + b + c;
    }

    static long sum(long a, long b) {
        return a + b;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W98SpreaderConversion.class;
        MethodHandle two = l.findStatic(me, "two",
                MethodType.methodType(String.class, String.class, String.class));
        MethodHandle three = l.findStatic(me, "three",
                MethodType.methodType(String.class, String.class, String.class, String.class));
        MethodHandle sum = l.findStatic(me, "sum",
                MethodType.methodType(long.class, long.class, long.class));

        row("spread-int-into-strings", () -> two.asSpreader(int[].class, 2));
        row("spread-int-into-last-string", () -> three.asSpreader(int[].class, 1));
        row("spread-at-int-into-strings", () -> three.asSpreader(0, int[].class, 2));
        row("spread-int-into-longs", () -> (long) sum.asSpreader(int[].class, 2).invoke(new int[] {2, 3}));
        row("spread-int-into-longs-type", () -> sum.asSpreader(int[].class, 2).type());
        MethodHandle objects = two.asSpreader(Object[].class, 2);
        row("spread-objects-into-strings", () -> (String) objects.invokeExact(new Object[] {"a", "b"}));
        row("spread-wrong-length-call", () -> (String) objects.invokeExact(new Object[] {"a"}));
        row("spread-null-call", () -> (String) objects.invokeExact((Object[]) null));
        row("spread-not-array", () -> two.asSpreader(String.class, 2));
        row("spread-bad-length", () -> two.asSpreader(Object[].class, 3));
        row("spread-bad-position", () -> two.asSpreader(1, Object[].class, 2));
        row("spread-to-string", () -> objects.toString());
    }
}
