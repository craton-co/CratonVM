// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.*;
import java.lang.reflect.*;

// Edge values through constant asType handles and Method.invoke, after warm-up.
public class AsTypeEdge {
    public interface Svc { int price(int item); }
    static final class Plain implements Svc { public int price(int item) { return item * 3; } }
    static long twiceJ(long v) { return v * 2; }
    static int sprice(int v) { return v + 7; }
    static String name(Plain p, String s) { return s + p.price(1); }
    static final MethodHandle VIRT, LONGH, STAT, REFS, RECV2, ARG2, RET2;
    static final Svc LAMBDA = x -> x * 5;
    static final Method M;
    static {
        try {
            var l = MethodHandles.lookup();
            VIRT = l.findVirtual(Plain.class, "price", MethodType.methodType(int.class, int.class))
                .asType(MethodType.methodType(Object.class, Object.class, Object.class));
            LONGH = l.findStatic(AsTypeEdge.class, "twiceJ", MethodType.methodType(long.class, long.class))
                .asType(MethodType.methodType(Object.class, Object.class));
            STAT = l.findStatic(AsTypeEdge.class, "sprice", MethodType.methodType(int.class, int.class))
                .asType(MethodType.methodType(Object.class, Object.class));
            REFS = l.findStatic(AsTypeEdge.class, "name", MethodType.methodType(String.class, Plain.class, String.class))
                .asType(MethodType.methodType(Object.class, Object.class, Object.class));
            M = Svc.class.getMethod("price", int.class);
            // Three adapters of ONE direct handle: only the last is its asTypeCache.
            MethodHandle base = l.findVirtual(Plain.class, "price", MethodType.methodType(int.class, int.class));
            RECV2 = base.asType(MethodType.methodType(int.class, Object.class, int.class));
            ARG2 = base.asType(MethodType.methodType(int.class, Plain.class, Object.class));
            RET2 = base.asType(MethodType.methodType(Object.class, Plain.class, int.class));
        } catch (Exception e) { throw new Error(e); }
    }
    static Object virt(Object r, Object a) throws Throwable { return VIRT.invokeExact(r, a); }
    static Object longh(Object a) throws Throwable { return LONGH.invokeExact(a); }
    static Object stat(Object a) throws Throwable { return STAT.invokeExact(a); }
    static Object refs(Object a, Object b) throws Throwable { return REFS.invokeExact(a, b); }
    static Object refl(Object r, Object a) throws Throwable { return M.invoke(r, a); }
    static int recv2(Object r, int a) throws Throwable { return (int) RECV2.invokeExact(r, a); }
    static int arg2(Plain r, Object a) throws Throwable { return (int) ARG2.invokeExact(r, a); }
    static Object ret2(Plain r, int a) throws Throwable { return RET2.invokeExact(r, a); }

    interface Call { Object run() throws Throwable; }
    static void show(String what, Call c) {
        try { Object v = c.run(); System.out.println(what + " = " + v + " (" + (v == null ? "null" : v.getClass().getName()) + ")"); }
        catch (Throwable t) { System.out.println(what + " threw " + t.getClass().getName() + ": " + t.getMessage()); }
    }

    public static void main(String[] args) throws Throwable {
        Plain p = new Plain();
        long s = 0;
        for (int i = 0; i < 30000; i++) {
            s += (Integer) virt(p, i) + (Long) longh((long) i) + (Integer) stat(i) + ((String) refs(p, "x")).length()
                + (Integer) refl(p, i) + (Integer) refl(LAMBDA, i)
                + recv2(p, i) + arg2(p, i) + (Integer) ret2(p, i);
        }
        System.out.println("warm " + s);
        for (int round = 0; round < 3; round++) {
            System.out.println("-- round " + round);
            show("virt ok", () -> virt(p, 4));
            show("virt cached box", () -> virt(p, 40) == virt(p, 40));
            show("virt big box", () -> virt(p, 400) == virt(p, 400));
            show("virt wrong recv", () -> virt("str", 4));
            show("virt null recv", () -> virt(null, 4));
            show("virt null arg", () -> virt(p, null));
            show("virt short arg", () -> virt(p, (short) 5));
            show("virt char arg", () -> virt(p, 'A'));
            show("virt long arg", () -> virt(p, 5L));
            show("virt string arg", () -> virt(p, "5"));
            show("long ok", () -> longh(21L));
            show("long int arg", () -> longh(21));
            show("long byte arg", () -> longh((byte) -3));
            show("long null", () -> longh(null));
            show("stat ok", () -> stat(1));
            show("stat double", () -> stat(1.0));
            show("refs ok", () -> refs(p, "y"));
            show("refs null s", () -> refs(p, null));
            show("refs null p", () -> refs(null, "z"));
            show("refs wrong s", () -> refs(p, 3));
            show("refl lambda", () -> refl(LAMBDA, 6));
            show("refl wrong recv", () -> refl("str", 6));
            show("refl null arg", () -> refl(p, null));
            show("refl long arg", () -> refl(p, 6L));
            show("recv2 ok", () -> recv2(p, 2));
            show("recv2 wrong", () -> recv2("s", 2));
            show("recv2 null", () -> recv2(null, 2));
            show("arg2 ok", () -> arg2(p, 2));
            show("arg2 byte", () -> arg2(p, (byte) 2));
            show("arg2 null", () -> arg2(p, null));
            show("arg2 string", () -> arg2(p, "2"));
            show("ret2 ok", () -> ret2(p, 2));
            show("ret2 identity", () -> ret2(p, 9) == ret2(p, 9));
        }
    }
}
