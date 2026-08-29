// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Wave 2, Task C probe: `java.lang.invoke.MethodHandles.Lookup` across the
// {primitive, object, varargs} x {user class, JDK class} matrix.
//
// Driven by `vm/tests/wave2_c_methodhandles.rs`, which requires at least 5 of
// the 7 lookup lines to match the HotSpot reference plus a trailing `OK`:
//
//   findStatic.prim=7
//   findStatic.obj=HELLO
//   findStatic.varargs=10
//   findVirtual.prim=30
//   findVirtual.obj=hi!
//   findStatic.jdk=42
//   findVirtual.jdk=5
//   OK
//
// Every value printed is the RESULT of invoking the handle that was just looked
// up -- no line is a literal. Each of the seven is looked up and invoked
// independently inside its own try/catch, so one broken lookup does not hide
// the state of the other six (that partial-credit shape is what the driving
// test's "5 of 7" rule is written against); a failed step prints
// `<label>=ERR:<throwable>`, which matches none of the expected strings. A VM
// whose `findStatic`/`findVirtual` is missing or mis-typed therefore fails the
// 5-of-7 assertion, and one that cannot bootstrap `MethodHandles.lookup()` at
// all never reaches `OK` and exits non-zero.
//
// Deliberately written without lambdas or method references: a
// `LambdaMetafactory` gap would otherwise take all seven lines down at once and
// be misread as a MethodHandles.Lookup defect. The only `invokedynamic` this
// class contains is whatever javac emits for string concatenation.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Locale;

public class MhProbe {

    // ---- user-class targets -------------------------------------------------

    static int addInts(int a, int b) {
        return a + b;
    }

    static String upper(String s) {
        return s.toUpperCase(Locale.ROOT);
    }

    static int sumAll(int... xs) {
        int t = 0;
        for (int x : xs) {
            t += x;
        }
        return t;
    }

    private final int factor;

    MhProbe(int factor) {
        this.factor = factor;
    }

    int scale(int x) {
        return x * factor;
    }

    String bang(String s) {
        return s + "!";
    }

    // ---- probe --------------------------------------------------------------

    private static String err(Throwable t) {
        return "ERR:" + t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] args) {
        final MethodHandles.Lookup lookup = MethodHandles.lookup();
        final MhProbe inst = new MhProbe(3);

        // 1. static, primitive signature, user class: 3 + 4
        String v1;
        try {
            MethodHandle mh = lookup.findStatic(MhProbe.class, "addInts",
                    MethodType.methodType(int.class, int.class, int.class));
            v1 = Integer.toString((int) mh.invokeExact(3, 4));
        } catch (Throwable t) {
            v1 = err(t);
        }
        System.out.println("findStatic.prim=" + v1);

        // 2. static, reference signature, user class: "hello" -> "HELLO"
        String v2;
        try {
            MethodHandle mh = lookup.findStatic(MhProbe.class, "upper",
                    MethodType.methodType(String.class, String.class));
            v2 = (String) mh.invokeExact("hello");
        } catch (Throwable t) {
            v2 = err(t);
        }
        System.out.println("findStatic.obj=" + v2);

        // 3. static varargs, user class: the handle must be a varargs collector
        //    and must box the loose arguments into the int[] itself.
        String v3;
        try {
            MethodHandle mh = lookup.findStatic(MhProbe.class, "sumAll",
                    MethodType.methodType(int.class, int[].class));
            if (!mh.isVarargsCollector()) {
                throw new AssertionError(
                        "findStatic on an ACC_VARARGS method returned a non-varargs handle: " + mh);
            }
            v3 = Integer.toString((int) mh.invoke(1, 2, 3, 4));
        } catch (Throwable t) {
            v3 = err(t);
        }
        System.out.println("findStatic.varargs=" + v3);

        // 4. virtual, primitive signature, user class: 10 * factor(3)
        String v4;
        try {
            MethodHandle mh = lookup.findVirtual(MhProbe.class, "scale",
                    MethodType.methodType(int.class, int.class));
            v4 = Integer.toString((int) mh.invokeExact(inst, 10));
        } catch (Throwable t) {
            v4 = err(t);
        }
        System.out.println("findVirtual.prim=" + v4);

        // 5. virtual, reference signature, user class: "hi" -> "hi!"
        String v5;
        try {
            MethodHandle mh = lookup.findVirtual(MhProbe.class, "bang",
                    MethodType.methodType(String.class, String.class));
            v5 = (String) mh.invokeExact(inst, "hi");
        } catch (Throwable t) {
            v5 = err(t);
        }
        System.out.println("findVirtual.obj=" + v5);

        // 6. static, JDK class: Integer.parseInt("42")
        String v6;
        try {
            MethodHandle mh = lookup.findStatic(Integer.class, "parseInt",
                    MethodType.methodType(int.class, String.class));
            v6 = Integer.toString((int) mh.invokeExact("42"));
        } catch (Throwable t) {
            v6 = err(t);
        }
        System.out.println("findStatic.jdk=" + v6);

        // 7. virtual, JDK class: "hello".length()
        String v7;
        try {
            MethodHandle mh = lookup.findVirtual(String.class, "length",
                    MethodType.methodType(int.class));
            v7 = Integer.toString((int) mh.invokeExact("hello"));
        } catch (Throwable t) {
            v7 = err(t);
        }
        System.out.println("findVirtual.jdk=" + v7);

        System.out.println("OK");
    }
}
