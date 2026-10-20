// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L4 --
// docs/internal/fixed-bugs/interpreter-L4-a-handle-door-s-class-init-failure-lists-no-jdk-frames-FIXED-20261023.md:
// a method handle or VarHandle on a member of a class whose static
// initializer throws. HotSpot initializes the class in the handle's own
// barrier, under `Unsafe.ensureClassInitialized0` (or, for a constructor,
// `Unsafe.allocateInstance`). Each row prints the throwable, its cause and the
// JDK frames above the probe's first frame. One failing class per row; the
// first row is repeated last (NoClassDefFoundError then).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical)
// mh-static-exact: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: a [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit]
// mh-static-invoke-boxed: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: b [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit]
// mh-static-with-arguments: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: c [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit java.lang.invoke.MethodHandle.invokeWithArguments java.lang.invoke.MethodHandle.invokeWithArguments]
// mh-static-getter: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: d [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized]
// mh-static-setter: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: e [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized]
// mh-constructor: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: f [jdk.internal.misc.Unsafe.allocateInstance java.lang.invoke.DirectMethodHandle.allocateInstance]
// vh-static-get: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: g [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.LazyInitializingVarHandle.initialize java.lang.invoke.LazyInitializingVarHandle.ensureInitialized java.lang.invoke.LazyInitializingVarHandle.target java.lang.invoke.VarHandleInts$FieldStaticReadOnly.get]
// vh-static-set: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: h [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.LazyInitializingVarHandle.initialize java.lang.invoke.LazyInitializingVarHandle.ensureInitialized java.lang.invoke.LazyInitializingVarHandle.target java.lang.invoke.VarHandleInts$FieldStaticReadWrite.set]
// mh-bound-static: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: i [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit]
// mh-static-void: java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: j [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit]
// mh-ok: 5
// vh-static-get-again: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: g [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.LazyInitializingVarHandle.initialize java.lang.invoke.LazyInitializingVarHandle.ensureInitialized java.lang.invoke.LazyInitializingVarHandle.target java.lang.invoke.VarHandleInts$FieldStaticReadOnly.get]
// mh-static-exact-again: java.lang.NoClassDefFoundError cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: a [in thread "main"] [jdk.internal.misc.Unsafe.ensureClassInitialized0 jdk.internal.misc.Unsafe.ensureClassInitialized java.lang.invoke.DirectMethodHandle.checkInitialized java.lang.invoke.DirectMethodHandle.ensureInitialized java.lang.invoke.DirectMethodHandle.internalMemberNameEnsureInit]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W59HandleClassInitFrames`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.List;

public class L4W59HandleClassInitFrames {
    static class A { static { if (true) throw new IllegalStateException("a"); } static int m() { return 1; } }
    static class B { static { if (true) throw new IllegalStateException("b"); } static int m(int x) { return x; } }
    static class C { static { if (true) throw new IllegalStateException("c"); } static Object m(Object x) { return x; } }
    static class D { static int v; static { if (true) throw new IllegalStateException("d"); } }
    static class E { static int v; static { if (true) throw new IllegalStateException("e"); } }
    static class F { static { if (true) throw new IllegalStateException("f"); } F() { } }
    static class G { static int v; static { if (true) throw new IllegalStateException("g"); } }
    static class H { static int v; static { if (true) throw new IllegalStateException("h"); } }
    static class I { static { if (true) throw new IllegalStateException("i"); } static int m(int x, int y) { return x + y; } }
    static class J { static { if (true) throw new IllegalStateException("j"); } static void m() { } }
    static class Ok { static int e() { return 5; } }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L4W59")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            System.out.println(name + ": " + t.getClass().getName() + " cause=" + t.getCause() + " [" + jdk + "]");
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle a = lk.findStatic(A.class, "m", MethodType.methodType(int.class));
        MethodHandle b = lk.findStatic(B.class, "m", MethodType.methodType(int.class, int.class));
        MethodHandle c = lk.findStatic(C.class, "m", MethodType.methodType(Object.class, Object.class));
        MethodHandle dGet = lk.findStaticGetter(D.class, "v", int.class);
        MethodHandle eSet = lk.findStaticSetter(E.class, "v", int.class);
        MethodHandle fNew = lk.findConstructor(F.class, MethodType.methodType(void.class));
        VarHandle g = lk.findStaticVarHandle(G.class, "v", int.class);
        VarHandle h = lk.findStaticVarHandle(H.class, "v", int.class);
        MethodHandle i = MethodHandles.insertArguments(
                lk.findStatic(I.class, "m", MethodType.methodType(int.class, int.class, int.class)), 0, 3);
        MethodHandle j = lk.findStatic(J.class, "m", MethodType.methodType(void.class));
        MethodHandle ok = lk.findStatic(Ok.class, "e", MethodType.methodType(int.class));
        row("mh-static-exact", () -> (int) a.invokeExact());
        row("mh-static-invoke-boxed", () -> (Object) b.invoke((Object) 4));
        row("mh-static-with-arguments", () -> c.invokeWithArguments(List.of("x")));
        row("mh-static-getter", () -> (int) dGet.invokeExact());
        row("mh-static-setter", () -> { eSet.invokeExact(3); return "set"; });
        row("mh-constructor", () -> (Object) fNew.invoke());
        row("vh-static-get", () -> (int) g.get());
        row("vh-static-set", () -> { h.set(2); return "set"; });
        row("mh-bound-static", () -> (int) i.invokeExact(4));
        row("mh-static-void", () -> { j.invokeExact(); return "void"; });
        row("mh-ok", () -> (int) ok.invokeExact());
        row("vh-static-get-again", () -> (int) g.get());
        row("mh-static-exact-again", () -> (int) a.invokeExact());
    }
}
