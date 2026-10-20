// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L4 (`CRATONVM_MH_IWA_NATIVE_STEPS` and
// `CRATONVM_MH_IWA_SPREADER_MEMO`, default off, read only under
// `CRATONVM_MH_IWA_JDK_BODY=1`; page
// docs/known-issues/interpreter/i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call-20261212.md):
// `invokeWithArguments(Object...)` answers, frames, exceptions and nesting
// while the array body runs step by step from the intrinsic and its spread
// invoker is reused. Rows past 300 calls cross the linker's customization
// threshold (127) many times over on one reused spreader; every row must be
// HotSpot's.
//
// Run: javac -d . L4W116IwaNativeSteps.java
//      java -cp . L4W116IwaNativeSteps                                              (HotSpot)
//      for each of --jdk-only / --compatible, JIT and --nojit:
//        cratonvm $mode -cp . L4W116IwaNativeSteps
//        CRATONVM_MH_IWA_JDK_BODY=1 cratonvm $mode -cp . L4W116IwaNativeSteps
//        CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_MH_IWA_NATIVE_STEPS=1 cratonvm $mode -cp . L4W116IwaNativeSteps
//        CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_MH_IWA_NATIVE_STEPS=1 CRATONVM_MH_IWA_SPREADER_MEMO=1 \
//            cratonvm $mode -cp . L4W116IwaNativeSteps
//      (the `hidden` row is a known gap with CRATONVM_MH_IWA_JDK_BODY off, and may be one
//       under --compatible: see gap-hidden-walks-lack-hotspots-spun-lambda-form-frames)
//
// `hidden-after-warm`: after ~127 calls HotSpot CUSTOMIZES the TARGET handle (the generic
// invoker form the spreader runs calls `Invokers.checkCustomized` on it), so its holder frame
// becomes a spun `LambdaForm$DMH::invokeStatic` (same under -Xint). That is a property of the
// target, not of the spreader the memo reuses; a DIFF there in EVERY arm (switches off too) is a
// pre-existing customization gap, not these switches. Compare the arms with each other first.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// arity0: zero
// arity1: 7
// arity2: 3+4
// arity3: 1/2/3
// null-array: zero
// boxed-int: 21
// warm-500: 500 sum=125250
// nested: 6
// many-n: 0 1 2 3 4 5
// default: [L4W116IwaNativeSteps::walk, L4W116IwaNativeSteps::frames, java.lang.invoke.MethodHandle::invokeWithArguments]
// hidden: [L4W116IwaNativeSteps::walk, L4W116IwaNativeSteps::frames, java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.Invokers$Holder::invokeExact_MT, java.lang.invoke.MethodHandle::invokeWithArguments]
// hidden-after-warm: [L4W116IwaNativeSteps::walk, L4W116IwaNativeSteps::frames, java.lang.invoke.LambdaForm$DMH::invokeStatic, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.Invokers$Holder::invokeExact_MT, java.lang.invoke.MethodHandle::invokeWithArguments]
// thrown: java.lang.IllegalStateException: boom 9
// thrown-trace: [L4W116IwaNativeSteps::boom, java.lang.invoke.MethodHandle::invokeWithArguments]
// wrong-arity: java.lang.invoke.WrongMethodTypeException
// bad-cast: java.lang.ClassCastException
// HOTSPOT_EXPECTED_END

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L4W116IwaNativeSteps {
    static final StackWalker DEFAULT = StackWalker.getInstance();
    static final StackWalker HIDDEN = StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES);
    static List<String> lastDefault;
    static List<String> lastHidden;
    static MethodHandle self;

    static String zero() {
        return "zero";
    }

    static int one(int x) {
        return x;
    }

    static String two(Object a, Object b) {
        return a + "+" + b;
    }

    static String three(String a, String b, String c) {
        return a + "/" + b + "/" + c;
    }

    static int add(int a, int b) {
        return a + b;
    }

    static Object frames(Object x) {
        lastDefault = walk(DEFAULT);
        lastHidden = walk(HIDDEN);
        return x;
    }

    static Object boom(Object x) {
        throw new IllegalStateException("boom " + x);
    }

    // Calls invokeWithArguments again from inside the target (the same n):
    // a reused spreader is entered re-entrantly.
    static int nest(int depth) throws Throwable {
        if (depth == 0) {
            return 0;
        }
        return 1 + (Integer) self.invokeWithArguments(depth - 1);
    }

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("LambdaForm\\$(\\w+)/ADDR", "LambdaForm\\$$1");
    }

    static List<String> walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            for (StackFrame f : (Iterable<StackFrame>) s::iterator) {
                if (f.getClassName().equals(L4W116IwaNativeSteps.class.getName())
                        && f.getMethodName().equals("main")) {
                    break;
                }
                out.add(fold(f.getClassName() + "::" + f.getMethodName()));
            }
            return out;
        });
    }

    static List<String> traceOf(Throwable t) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : t.getStackTrace()) {
            if (e.getClassName().equals(L4W116IwaNativeSteps.class.getName())
                    && e.getMethodName().equals("main")) {
                break;
            }
            out.add(fold(e.getClassName() + "::" + e.getMethodName()));
        }
        return out;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W116IwaNativeSteps.class;
        MethodHandle zero = l.findStatic(me, "zero", MethodType.methodType(String.class));
        MethodHandle one = l.findStatic(me, "one", MethodType.methodType(int.class, int.class));
        MethodHandle two = l.findStatic(me, "two",
                MethodType.methodType(String.class, Object.class, Object.class));
        MethodHandle three = l.findStatic(me, "three",
                MethodType.methodType(String.class, String.class, String.class, String.class));
        MethodHandle add = l.findStatic(me, "add", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle frames = l.findStatic(me, "frames", MethodType.methodType(Object.class, Object.class));
        MethodHandle boom = l.findStatic(me, "boom", MethodType.methodType(Object.class, Object.class));
        self = l.findStatic(me, "nest", MethodType.methodType(int.class, int.class));

        System.out.println("arity0: " + zero.invokeWithArguments());
        System.out.println("arity1: " + one.invokeWithArguments(7));
        System.out.println("arity2: " + two.invokeWithArguments(3, 4));
        System.out.println("arity3: " + three.invokeWithArguments("1", "2", "3"));
        System.out.println("null-array: " + zero.invokeWithArguments((Object[]) null));
        System.out.println("boxed-int: " + add.invokeWithArguments(Integer.valueOf(10), Integer.valueOf(11)));

        int calls = 0;
        long sum = 0;
        for (int i = 1; i <= 500; i++) {
            sum += (Integer) add.invokeWithArguments(i, 0);
            calls++;
        }
        System.out.println("warm-500: " + calls + " sum=" + sum);
        System.out.println("nested: " + self.invokeWithArguments(6));

        StringBuilder many = new StringBuilder();
        Object[][] shapes = {{}, {1}, {1, 2}, {1, 2, 3}, {1, 2, 3, 4}, {1, 2, 3, 4, 5}};
        for (Object[] shape : shapes) {
            MethodHandle count = MethodHandles.dropArguments(
                    MethodHandles.constant(int.class, shape.length), 0,
                    java.util.Collections.nCopies(shape.length, Object.class));
            many.append(many.length() == 0 ? "" : " ").append(count.invokeWithArguments(shape));
        }
        System.out.println("many-n: " + many);

        frames.invokeWithArguments("x");
        System.out.println("default: " + lastDefault);
        System.out.println("hidden: " + lastHidden);
        for (int i = 0; i < 300; i++) {
            frames.invokeWithArguments("x");
        }
        System.out.println("hidden-after-warm: " + lastHidden);

        try {
            boom.invokeWithArguments(9);
            System.out.println("thrown: none");
        } catch (Throwable t) {
            System.out.println("thrown: " + t);
            System.out.println("thrown-trace: " + traceOf(t));
        }
        try {
            add.invokeWithArguments(1);
            System.out.println("wrong-arity: none");
        } catch (Throwable t) {
            System.out.println("wrong-arity: " + t.getClass().getName());
        }
        try {
            three.invokeWithArguments("a", 2, "c");
            System.out.println("bad-cast: none");
        } catch (Throwable t) {
            System.out.println("bad-cast: " + t.getClass().getName());
        }
    }
}
