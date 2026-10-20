// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L7: JVMS 5.5 class-initialization
// triggers and order, javac output only (no hand assembly). Every row prints
// the <clinit> side effects the row's action caused, in order.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W98InitOrder
//
// HotSpot 25 prints (the orchestrator's HotSpot run is authoritative):
//   r1 inherited static field: S1
//   r2 inherited static method: S2
//   r3 interface field via class: I3
//   r4 no-default interface not initialized: C4
//   r5 default superinterfaces order: S5 I5a I5c C5
//   r6 interface init skips superinterfaces: I6b
//   r7 array/literal/forName(false): <none>
//   r7 forName(true): C7
//   r8 recursive init: B8:10 A8:11 = 11
//   r9 interface static method: I9b
//   r10 getDeclaredFields: <none>
//   r10 Field.getInt: C10
//   r11 findStatic: <none>
//   r11 invokeExact: C11 = 11
//   r12 first: java.lang.AssertionError: a12
//   r12 second: java.lang.NoClassDefFoundError: Could not initialize class L7W98InitOrder$C12 / java.lang.ExceptionInInitializerError: Exception java.lang.AssertionError: a12 [in thread "main"]
//   r13 sub first: java.lang.ExceptionInInitializerError: null / java.lang.RuntimeException: s13
//   r13 sub second: java.lang.NoClassDefFoundError: Could not initialize class L7W98InitOrder$C13 / java.lang.ExceptionInInitializerError: Exception java.lang.ExceptionInInitializerError [in thread "main"]
//   r13 super: java.lang.NoClassDefFoundError: Could not initialize class L7W98InitOrder$S13 / java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: s13 [in thread "main"]
//   r13 log: <none>
//   r14 first: java.lang.ExceptionInInitializerError: null / java.lang.RuntimeException: i14
//   r14 second: java.lang.NoClassDefFoundError: Could not initialize class L7W98InitOrder$C14 / java.lang.ExceptionInInitializerError: Exception java.lang.ExceptionInInitializerError [in thread "main"]
//   r15 waited: 15
//   r16 waited failure: java.lang.NoClassDefFoundError: Could not initialize class L7W98InitOrder$C16 / java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: c16 [in thread "t16"]
//   r16 initializer thread saw: java.lang.ExceptionInInitializerError: null / java.lang.RuntimeException: c16
//   r17 inlined constant: <none> 5
//   r17 reflective constant: C17 5
//   r18 null instanceof/checkcast/call: <none>
//   r19 putstatic: C19
//   r20 own static method: S20 C20
//   r21 instance creation: S21 I21 C21

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.concurrent.CountDownLatch;

public class L7W98InitOrder {
    static final StringBuilder LOG = new StringBuilder();

    static synchronized void log(String s) {
        LOG.append(s).append(' ');
    }

    static synchronized String take() {
        String s = LOG.toString().trim();
        LOG.setLength(0);
        return s.isEmpty() ? "<none>" : s;
    }

    static int side(String s) {
        log(s);
        return 1;
    }

    static int boom(String s) {
        throw new RuntimeException(s);
    }

    static int err(String s) {
        throw new AssertionError(s);
    }

    static String t(Throwable e) {
        String s = e.getClass().getName() + ": " + e.getMessage();
        if (e.getCause() != null) {
            s += " / " + e.getCause().getClass().getName() + ": " + e.getCause().getMessage();
        }
        return s;
    }

    static class S1 { static int x = side("S1"); }
    static class C1 extends S1 { static { log("C1"); } }

    static class S2 { static { log("S2"); } static int m() { return 2; } }
    static class C2 extends S2 { static { log("C2"); } }

    interface I3 { int F = side("I3"); }
    static class C3 implements I3 { static { log("C3"); } }

    interface I4 { int F = side("I4"); void m(); }
    static class C4 implements I4 { static { log("C4"); } public void m() { } }

    interface I5a { int F = side("I5a"); default void d() { } }
    interface I5b extends I5a { int G = side("I5b"); }
    interface I5c { int H = side("I5c"); default void e() { } }
    static class S5 { static { log("S5"); } }
    static class C5 extends S5 implements I5b, I5c { static { log("C5"); } }

    interface I6a { int F = side("I6a"); default void d() { } }
    interface I6b extends I6a { int G = side("I6b"); }

    static class C7 { static { log("C7"); } }

    static class A8 { static int a = B8.b + 1; static { log("A8:" + a); } }
    static class B8 { static int b = A8.a + 10; static { log("B8:" + b); } }

    interface I9a { int F = side("I9a"); default void d() { } }
    interface I9b extends I9a { int G = side("I9b"); static int s() { return 9; } }

    static class C10 { static int v = side("C10"); }

    static class C11 { static { log("C11"); } static int m() { return 11; } }

    static class C12 { static int v = err("a12"); }

    static class S13 { static int x = boom("s13"); }
    static class C13 extends S13 { static int v; static { log("C13"); } }

    interface I14 { int F = boom("i14"); default void d() { } }
    static class C14 implements I14 { static int v; static { log("C14"); } }

    static final CountDownLatch L15 = new CountDownLatch(1);
    static class C15 {
        static int v;
        static {
            L15.countDown();
            try { Thread.sleep(300); } catch (InterruptedException e) { }
            v = 15;
        }
    }

    static final CountDownLatch L16 = new CountDownLatch(1);
    static class C16 {
        static int v;
        static {
            L16.countDown();
            try { Thread.sleep(300); } catch (InterruptedException e) { }
            v = boom("c16");
        }
    }

    static class C17 { static final int K = 5; static { log("C17"); } }

    static class C18 { static { log("C18"); } int m() { return 18; } }

    static class C19 { static int x; static { log("C19"); } }

    static class S20 { static { log("S20"); } }
    static class C20 extends S20 { static { log("C20"); } static int own() { return 20; } }

    interface I21 { int F = side("I21"); default void d() { } }
    static class S21 { static { log("S21"); } }
    static class C21 extends S21 implements I21 { static { log("C21"); } }

    public static void main(String[] args) throws Throwable {
        int sink = C1.x;
        System.out.println("r1 inherited static field: " + take());
        sink += C2.m();
        System.out.println("r2 inherited static method: " + take());
        sink += C3.F;
        System.out.println("r3 interface field via class: " + take());
        new C4();
        System.out.println("r4 no-default interface not initialized: " + take());
        new C5();
        System.out.println("r5 default superinterfaces order: " + take());
        sink += I6b.G;
        System.out.println("r6 interface init skips superinterfaces: " + take());

        C7[] a7 = new C7[3];
        C7[][] b7 = new C7[2][2];
        Class<?> k7 = C7.class;
        Class<?> f7 = Class.forName("L7W98InitOrder$C7", false, L7W98InitOrder.class.getClassLoader());
        System.out.println("r7 array/literal/forName(false): " + take());
        Class.forName("L7W98InitOrder$C7");
        System.out.println("r7 forName(true): " + take());

        int a8 = A8.a;
        System.out.println("r8 recursive init: " + take() + " = " + a8);

        sink += I9b.s();
        System.out.println("r9 interface static method: " + take());

        int nf = C10.class.getDeclaredFields().length;
        System.out.println("r10 getDeclaredFields: " + take());
        sink += C10.class.getDeclaredField("v").getInt(null);
        System.out.println("r10 Field.getInt: " + take());

        MethodHandle mh = MethodHandles.lookup().findStatic(C11.class, "m", MethodType.methodType(int.class));
        System.out.println("r11 findStatic: " + take());
        int r11 = (int) mh.invokeExact();
        System.out.println("r11 invokeExact: " + take() + " = " + r11);

        try { sink += C12.v; System.out.println("r12 first: no throw"); }
        catch (Throwable e) { System.out.println("r12 first: " + t(e)); }
        try { sink += C12.v; System.out.println("r12 second: no throw"); }
        catch (Throwable e) { System.out.println("r12 second: " + t(e)); }

        try { sink += C13.v; System.out.println("r13 sub first: no throw"); }
        catch (Throwable e) { System.out.println("r13 sub first: " + t(e)); }
        try { sink += C13.v; System.out.println("r13 sub second: no throw"); }
        catch (Throwable e) { System.out.println("r13 sub second: " + t(e)); }
        try { sink += S13.x; System.out.println("r13 super: no throw"); }
        catch (Throwable e) { System.out.println("r13 super: " + t(e)); }
        System.out.println("r13 log: " + take());

        try { sink += C14.v; System.out.println("r14 first: no throw"); }
        catch (Throwable e) { System.out.println("r14 first: " + t(e)); }
        try { sink += C14.v; System.out.println("r14 second: no throw"); }
        catch (Throwable e) { System.out.println("r14 second: " + t(e)); }
        take();

        Thread t15 = new Thread(() -> { int v = C15.v; }, "t15");
        t15.start();
        L15.await();
        System.out.println("r15 waited: " + C15.v);
        t15.join();

        final String[] seen16 = new String[1];
        Thread t16 = new Thread(() -> {
            try { int v = C16.v; seen16[0] = "no throw"; }
            catch (Throwable e) { seen16[0] = t(e); }
        }, "t16");
        t16.start();
        L16.await();
        try { sink += C16.v; System.out.println("r16 waited failure: no throw"); }
        catch (Throwable e) { System.out.println("r16 waited failure: " + t(e)); }
        t16.join();
        System.out.println("r16 initializer thread saw: " + seen16[0]);

        int k17 = C17.K;
        System.out.println("r17 inlined constant: " + take() + " " + k17);
        int r17 = C17.class.getDeclaredField("K").getInt(null);
        System.out.println("r17 reflective constant: " + take() + " " + r17);

        Object o18 = null;
        boolean is18 = o18 instanceof C18;
        C18 c18 = (C18) o18;
        try { sink += c18.m(); } catch (NullPointerException e) { }
        C18[] arr18 = (C18[]) null;
        System.out.println("r18 null instanceof/checkcast/call: " + take());

        C19.x = 5;
        System.out.println("r19 putstatic: " + take());

        sink += C20.own();
        System.out.println("r20 own static method: " + take());

        new C21();
        System.out.println("r21 instance creation: " + take());
        if (sink == Integer.MIN_VALUE + nf) System.out.println("unreachable");
    }
}
