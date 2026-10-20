// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 130, lane L4 (page
// docs/known-issues/interpreter/i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call-20261212.md,
// "Wave 130"). Two changes, both default off:
//
// * CRATONVM_MH_INTERP_BASIC_FRAME_PUSH (--nojit): an interpreted `invokeExact` keeps the
//   door for a handle class a before-linker lane serves (field accessors, plain
//   DirectMethodHandles, bound accessors), and remembers the linker pair / the door decision
//   per (address epoch, site, caller class, handle class). Part A: lane handles and
//   combinators (which still push the linker frame) at shareable sites, warm, across a
//   System.gc(), a null first argument on a bound species, a target that throws.
// * CRATONVM_MH_IWA_LINK_MEMO (under CRATONVM_MH_IWA_JDK_BODY + _NATIVE_STEPS +
//   _SPREADER_MEMO): the native steps' spreader and linker rows from a per-thread line keyed
//   by (VM, address epoch, caller class, arity). Part B: invokeWithArguments from two caller
//   classes alternating, arities 0-3, across a System.gc(), a throwing target, and a
//   recursion through invokeWithArguments into StackOverflowError.
//
// Run: javac -d . L4W130ExactPushAndLinkMemo.java
//      java -cp . L4W130ExactPushAndLinkMemo            (HotSpot; -Xint prints the same)
//      for each of --jdk-only / --compatible, with --nojit (and once with the JIT):
//        cratonvm $mode -cp . L4W130ExactPushAndLinkMemo
//        CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1 cratonvm $mode -cp . L4W130ExactPushAndLinkMemo
//        CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_MH_IWA_NATIVE_STEPS=1 CRATONVM_MH_IWA_SPREADER_MEMO=1 \
//            CRATONVM_MH_IWA_LINK_MEMO=1 cratonvm $mode -cp . L4W130ExactPushAndLinkMemo
//        the last arm plus CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// A static-getter: 300 sum=1500
// A bound-getter: 300 sum=2100
// A static-direct: 300 sum=90300
// A jdk-site: 300 sum=1500
// A combinator: 300 sum=90300
// A combinator-after-gc: 300 sum=90300
// A lane-after-gc: 1500 2100
// A null-first: 5 5
// A wrong-type: java.lang.invoke.WrongMethodTypeException
// A thrown: java.lang.IllegalStateException: boom 3
// A thrown-trace: [L4W130ExactPushAndLinkMemo::boom, L4W130ExactPushAndLinkMemo::partA, L4W130ExactPushAndLinkMemo::main]
// B arity: zero 6 3+4 1/2/3
// B two-callers: 200 main=40200 other=40200
// B after-gc: 14 14 ab ab
// B list: ab
// B thrown: java.lang.IllegalStateException: boom 4
// B soe: java.lang.StackOverflowError
// B after-soe: 14 14
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L4W130ExactPushAndLinkMemo {
    public static class Box {
        public static int S = 5;
        public int v = 7;
    }

    static int twice(int x) {
        return x * 2;
    }

    static String cat(String a, String b) {
        return a + b;
    }

    static int boom(int x) {
        throw new IllegalStateException("boom " + x);
    }

    static String zero() {
        return "zero";
    }

    static String three(int a, int b, int c) {
        return a + "/" + b + "/" + c;
    }

    static String plus(int a, int b) {
        return a + "+" + b;
    }

    static MethodHandle REC;
    static int depth;

    static Object rec(Object x) throws Throwable {
        depth++;
        return REC.invokeWithArguments(x);
    }

    static List<String> probeFrames(Throwable t) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : t.getStackTrace()) {
            if (e.getClassName().startsWith("L4W130")) {
                out.add(e.getClassName() + "::" + e.getMethodName());
            }
        }
        return out;
    }

    static void partA() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle sGetter = l.findStaticGetter(Box.class, "S", int.class);
        MethodHandle getter = l.findGetter(Box.class, "v", int.class);
        MethodHandle bound = getter.bindTo(new Box());
        MethodHandle twice = l.findStatic(L4W130ExactPushAndLinkMemo.class, "twice",
                MethodType.methodType(int.class, int.class));
        MethodHandle length = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
        // A combinator at a shareable site `(String,int)int`: no lane serves its class.
        MethodHandle dropped = MethodHandles.dropArguments(twice, 0, String.class);

        long sum = 0;
        int n = 0;
        for (int i = 0; i < 300; i++, n++) {
            sum += (int) sGetter.invokeExact();
        }
        System.out.println("A static-getter: " + n + " sum=" + sum);
        sum = 0;
        n = 0;
        for (int i = 0; i < 300; i++, n++) {
            sum += (int) bound.invokeExact();
        }
        System.out.println("A bound-getter: " + n + " sum=" + sum);
        sum = 0;
        n = 0;
        for (int i = 0; i < 300; i++, n++) {
            sum += (int) twice.invokeExact(i + 1);
        }
        System.out.println("A static-direct: " + n + " sum=" + sum);
        sum = 0;
        n = 0;
        for (int i = 0; i < 300; i++, n++) {
            sum += (int) length.invokeExact("hello");
        }
        System.out.println("A jdk-site: " + n + " sum=" + sum);
        sum = 0;
        n = 0;
        for (int i = 0; i < 300; i++, n++) {
            sum += (int) dropped.invokeExact("x", i + 1);
        }
        System.out.println("A combinator: " + n + " sum=" + sum);
        System.gc();
        sum = 0;
        n = 0;
        for (int i = 0; i < 300; i++, n++) {
            sum += (int) dropped.invokeExact("x", i + 1);
        }
        System.out.println("A combinator-after-gc: " + n + " sum=" + sum);
        int s1 = 0;
        int s2 = 0;
        for (int i = 0; i < 300; i++) {
            s1 += (int) sGetter.invokeExact();
            s2 += (int) bound.invokeExact();
        }
        System.out.println("A lane-after-gc: " + s1 + " " + s2);

        MethodHandle five = MethodHandles.dropArguments(MethodHandles.constant(int.class, 5), 0,
                String.class);
        int a = (int) five.invokeExact((String) null);
        int b = (int) five.invokeExact((String) null);
        System.out.println("A null-first: " + a + " " + b);
        try {
            long w = (long) dropped.invokeExact("x", 1);
            System.out.println("A wrong-type: none " + w);
        } catch (Throwable t) {
            System.out.println("A wrong-type: " + t.getClass().getName());
        }
        MethodHandle boom = MethodHandles.dropArguments(
                l.findStatic(L4W130ExactPushAndLinkMemo.class, "boom",
                        MethodType.methodType(int.class, int.class)), 0, String.class);
        for (int i = 0; i < 200; i++) {
            try {
                int r = (int) boom.invokeExact("x", 3);
                System.out.println("A thrown: none " + r);
            } catch (IllegalStateException e) {
                if (i == 199) {
                    System.out.println("A thrown: " + e);
                    System.out.println("A thrown-trace: " + probeFrames(e));
                }
            }
        }
    }

    static final class Other {
        static int call(MethodHandle h, int x) throws Throwable {
            return (Integer) h.invokeWithArguments(x);
        }
    }

    static void partB() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W130ExactPushAndLinkMemo.class;
        MethodHandle twice = l.findStatic(me, "twice", MethodType.methodType(int.class, int.class));
        MethodHandle cat = l.findStatic(me, "cat",
                MethodType.methodType(String.class, String.class, String.class));
        MethodHandle zero = l.findStatic(me, "zero", MethodType.methodType(String.class));
        MethodHandle plus = l.findStatic(me, "plus",
                MethodType.methodType(String.class, int.class, int.class));
        MethodHandle three = l.findStatic(me, "three",
                MethodType.methodType(String.class, int.class, int.class, int.class));
        System.out.println("B arity: " + zero.invokeWithArguments() + " "
                + twice.invokeWithArguments(3) + " "
                + plus.invokeWithArguments(3, 4) + " " + three.invokeWithArguments(1, 2, 3));
        long mainSum = 0;
        long otherSum = 0;
        int n = 0;
        for (int i = 0; i < 200; i++, n++) {
            mainSum += (Integer) twice.invokeWithArguments(i + 1);
            otherSum += Other.call(twice, i + 1);
        }
        System.out.println("B two-callers: " + n + " main=" + mainSum + " other=" + otherSum);
        System.gc();
        System.out.println("B after-gc: " + twice.invokeWithArguments(7) + " " + Other.call(twice, 7)
                + " " + cat.invokeWithArguments("a", "b") + " " + cat.invokeWithArguments("a", "b"));
        List<Object> pair = new ArrayList<>(List.of("a", "b"));
        System.out.println("B list: " + cat.invokeWithArguments(pair));
        MethodHandle boom = l.findStatic(me, "boom", MethodType.methodType(int.class, int.class));
        for (int i = 0; i < 200; i++) {
            try {
                Object r = boom.invokeWithArguments(4);
                System.out.println("B thrown: none " + r);
            } catch (IllegalStateException e) {
                if (i == 199) {
                    System.out.println("B thrown: " + e);
                }
            }
        }
        REC = l.findStatic(me, "rec", MethodType.methodType(Object.class, Object.class));
        try {
            rec("x");
            System.out.println("B soe: none");
        } catch (Throwable t) {
            System.out.println("B soe: " + t.getClass().getName());
        }
        System.out.println("B after-soe: " + twice.invokeWithArguments(7) + " "
                + Other.call(twice, 7));
    }

    public static void main(String[] args) throws Throwable {
        partA();
        partB();
    }
}
