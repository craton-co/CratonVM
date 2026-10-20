// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 133, lane L4 (page
// docs/known-issues/interpreter/i133-L4-nojit-invoke-with-arguments-recursion-overflows-the-native-stack-20261231.md).
// A recursion closed through a native door re-enters the interpreter once per
// level; under --nojit it died with a native EXCEPTION_STACK_OVERFLOW (exit
// 127) instead of a catchable StackOverflowError when the door's own Rust
// frames outgrew the count-only re-entry bound. Each row recurses until
// StackOverflowError, catches it, then runs the same door once more to show
// the thread recovered. Rows: invokeWithArguments, invoke (generic),
// invokeExact, Method.invoke, Constructor.newInstance, and a recursion in a
// second thread (8 MiB carrier) through invokeWithArguments.
//
// Run: javac -d . L4W133NativeReentrySoe.java
//      java -cp . L4W133NativeReentrySoe                      (HotSpot; -Xint prints the same)
//      for each of --jdk-only / --compatible, with and without --nojit:
//        cratonvm $mode -cp . L4W133NativeReentrySoe
//        CRATONVM_EXEC_NATIVE_STACK_GUARD=0 cratonvm $mode --nojit -cp . L4W133NativeReentrySoe
//            (the escape: expected to crash again, exit 127, on the rows that crashed before)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// iwa soe: java.lang.StackOverflowError deep=true
// iwa after: 14
// invoke soe: java.lang.StackOverflowError deep=true
// invoke after: 14
// exact soe: java.lang.StackOverflowError deep=true
// exact after: 14
// reflect soe: java.lang.StackOverflowError deep=true
// reflect after: 14
// ctor soe: java.lang.StackOverflowError deep=true
// ctor after: 14
// thread soe: java.lang.StackOverflowError deep=true
// thread after: 14
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W133NativeReentrySoe {
    static MethodHandle IWA;
    static MethodHandle GEN;
    static MethodHandle EXACT;
    static Method REFL;
    static Constructor<Node> CTOR;
    static int depth;

    static int twice(int x) {
        return x * 2;
    }

    static Object iwa(Object x) throws Throwable {
        depth++;
        return IWA.invokeWithArguments(x);
    }

    static Object gen(Object x) throws Throwable {
        depth++;
        return GEN.invoke(x);
    }

    static Object exact(Object x) throws Throwable {
        depth++;
        return (Object) EXACT.invokeExact(x);
    }

    static Object refl(Object x) throws Throwable {
        depth++;
        try {
            return REFL.invoke(null, x);
        } catch (InvocationTargetException e) {
            throw e.getCause();
        }
    }

    public static class Node {
        public Node(Object x) throws Throwable {
            depth++;
            try {
                CTOR.newInstance(x);
            } catch (InvocationTargetException e) {
                throw e.getCause();
            }
        }
    }

    interface Body {
        void run() throws Throwable;
    }

    static void row(String name, Body body, MethodHandle after) throws Throwable {
        depth = 0;
        try {
            body.run();
            System.out.println(name + " soe: none");
        } catch (Throwable t) {
            System.out.println(name + " soe: " + t.getClass().getName() + " deep=" + (depth > 100));
        }
        System.out.println(name + " after: " + after.invokeWithArguments(7));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W133NativeReentrySoe.class;
        MethodType oo = MethodType.methodType(Object.class, Object.class);
        IWA = l.findStatic(me, "iwa", oo);
        GEN = l.findStatic(me, "gen", oo);
        EXACT = l.findStatic(me, "exact", oo);
        REFL = me.getDeclaredMethod("refl", Object.class);
        CTOR = Node.class.getConstructor(Object.class);
        MethodHandle twice = l.findStatic(me, "twice", MethodType.methodType(int.class, int.class));

        row("iwa", () -> iwa("x"), twice);
        row("invoke", () -> gen("x"), twice);
        row("exact", () -> exact("x"), twice);
        row("reflect", () -> refl("x"), twice);
        row("ctor", () -> new Node("x"), twice);

        Throwable[] failed = new Throwable[1];
        Thread t = new Thread(() -> {
            try {
                row("thread", () -> iwa("x"), twice);
            } catch (Throwable e) {
                failed[0] = e;
            }
        });
        t.start();
        t.join();
        if (failed[0] != null) {
            System.out.println("thread failed: " + failed[0]);
        }
    }
}
