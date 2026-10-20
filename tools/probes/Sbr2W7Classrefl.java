// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.lang.reflect.RecordComponent;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.function.Supplier;

/**
 * SBR2 wave 7, lane classrefl: Class.newInstance / getTypeName, Method/Constructor.getRoot,
 * RecordComponent.getGenericType, InvocationHandler.invokeDefault, Proxy. First-run value, then
 * the same rows after a hot loop ("hot ..."). Diff the whole output against HotSpot 25.
 */
public class Sbr2W7Classrefl {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    interface X { Object get() throws Throwable; }

    static void row(String l, X x) {
        LABELS.add(l);
        ROWS.add(() -> {
            try { return String.valueOf(x.get()); }
            catch (Throwable t) {
                Throwable c = t;
                return "!" + c.getClass().getName() + ": " + c.getMessage()
                    + (c.getCause() != null ? " <- " + c.getCause().getClass().getName() + ": " + c.getCause().getMessage() : "");
            }
        });
    }

    public static class Pub { public Pub() {} }
    public static abstract class Abs { public Abs() {} }
    public interface Iface { default String hello(String n) { return "hi " + n; } String plain(); default int add(int a, int b) { return a + b; } }
    public interface Sub extends Iface { default String hello(String n) { return "sub " + Iface.super.hello(n); } }
    public interface Thr { default int t() throws java.io.IOException { throw new java.io.IOException("dflt"); } }
    public static class NoDef { public NoDef(int i) {} }
    public static class Priv { private Priv() {} }
    public static class Thrower { public Thrower() throws java.io.IOException { throw new java.io.IOException("boom"); } }
    public static class UThrower { public UThrower() { throw new IllegalStateException("ubo"); } }
    public class Inner { }
    public enum Color { R, G }
    public record Rec<T extends Number>(List<String> names, Map<String, List<Integer>> m, T t, int i, T[] arr, String plain) {}
    public record Simple(int a, String b) {}

    static String rc(Class<?> c) {
        StringBuilder sb = new StringBuilder();
        for (RecordComponent r : c.getRecordComponents())
            sb.append(r.getName()).append(':').append(r.getGenericType().getTypeName()).append('/').append(r.getType().getName()).append(';');
        return sb.toString();
    }

    static Object proxy(Class<?> i, InvocationHandler h) {
        return Proxy.newProxyInstance(Sbr2W7Classrefl.class.getClassLoader(), new Class<?>[] { i }, h);
    }

    public static void main(String[] a) throws Throwable {
        row("newInstance Pub", () -> Pub.class.newInstance().getClass().getSimpleName());
        row("newInstance twice", () -> Pub.class.newInstance() != Pub.class.newInstance());
        row("newInstance abstract", () -> Abs.class.newInstance());
        row("newInstance iface", () -> Iface.class.newInstance());
        row("newInstance nodef", () -> NoDef.class.newInstance());
        row("newInstance private", () -> Priv.class.newInstance().getClass().getSimpleName());
        row("newInstance thrower", () -> Thrower.class.newInstance());
        row("newInstance uthrower", () -> UThrower.class.newInstance());
        row("newInstance Class", () -> Class.class.newInstance());
        row("newInstance int", () -> int.class.newInstance());
        row("newInstance int[]", () -> int[].class.newInstance());
        row("newInstance enum", () -> Color.class.newInstance());
        row("newInstance inner", () -> Inner.class.newInstance());
        row("newInstance Object", () -> Object.class.newInstance().getClass());
        row("newInstance ArrayList", () -> ArrayList.class.newInstance());
        row("newInstance String", () -> "[" + String.class.newInstance() + "]");
        row("newInstance rec", () -> Simple.class.newInstance());
        row("newInstance void", () -> void.class.newInstance());
        Class<?>[] tn = { int.class, String.class, int[].class, String[][].class, Map.Entry.class, Pub.class, Pub[].class, Pub[][][].class,
            void.class, long[][].class, Object.class, Class.class, Color.class, Rec.class, Rec[].class, Inner[].class };
        for (Class<?> c : tn) { final Class<?> cc = c; row("getTypeName " + c.getName(), () -> cc.getTypeName() + "|" + cc.getName()); }
        row("getTypeName lambda", () -> { Runnable r = () -> {}; return r.getClass().getTypeName().replaceAll("[0-9]+", "N"); });
        row("getTypeName anon", () -> new Object(){}.getClass().getTypeName());
        row("getTypeName anon array", () -> new Object(){}.getClass().arrayType().getTypeName());
        row("getRoot method same", () -> { Method m = Pub.class.getMethod("hashCode"); return m.equals(Pub.class.getMethod("hashCode")) + " " + (m == Pub.class.getMethod("hashCode")); });
        row("getRoot method invoke", () -> Iface.class.getMethod("plain").getName());
        row("getRoot ctor same", () -> { Constructor<?> c = Pub.class.getConstructor(); return c.equals(Pub.class.getConstructor()) + " " + (c == Pub.class.getConstructor()); });
        row("getRoot ctor newInstance", () -> Pub.class.getConstructor().newInstance().getClass().getSimpleName());
        row("getRoot copy setAccessible", () -> { Constructor<?> c = Priv.class.getDeclaredConstructor(); c.setAccessible(true); return c.newInstance().getClass().getSimpleName() + " " + Priv.class.getDeclaredConstructor().canAccess(null); });
        row("getRoot method accessible", () -> { Method m = Pub.class.getMethod("hashCode"); m.setAccessible(true); return m.invoke(new Pub()) instanceof Integer; });
        row("record generics Rec", () -> rc(Rec.class));
        row("record generics Simple", () -> rc(Simple.class));
        row("record component toString", () -> { StringBuilder sb = new StringBuilder(); for (RecordComponent r : Rec.class.getRecordComponents()) sb.append(r).append(';').append(r.getGenericSignature()).append(';'); return sb; });
        row("record accessor", () -> Rec.class.getRecordComponents()[0].getAccessor().getName());
        final InvocationHandler h = (p, m, args) -> {
            if (m.isDefault()) return InvocationHandler.invokeDefault(p, m, args);
            return "plain:" + m.getName();
        };
        row("invokeDefault hello", () -> ((Iface) proxy(Iface.class, h)).hello("x"));
        row("invokeDefault sub", () -> ((Sub) proxy(Sub.class, h)).hello("y"));
        row("invokeDefault add", () -> ((Iface) proxy(Iface.class, h)).add(2, 3));
        row("invokeDefault plain", () -> ((Iface) proxy(Iface.class, h)).plain());
        row("invokeDefault abstract", () -> InvocationHandler.invokeDefault(proxy(Iface.class, h), Iface.class.getMethod("plain")));
        row("invokeDefault nonproxy", () -> InvocationHandler.invokeDefault(new Object(), Iface.class.getMethod("hello", String.class), "q"));
        row("invokeDefault null", () -> InvocationHandler.invokeDefault(null, Iface.class.getMethod("hello", String.class), "q"));
        row("invokeDefault wrong arg", () -> InvocationHandler.invokeDefault(proxy(Iface.class, h), Iface.class.getMethod("hello", String.class), 5));
        row("invokeDefault no args", () -> InvocationHandler.invokeDefault(proxy(Iface.class, h), Iface.class.getMethod("hello", String.class)));
        row("invokeDefault objectmethod", () -> InvocationHandler.invokeDefault(proxy(Iface.class, h), Object.class.getMethod("toString")));
        row("invokeDefault thrower", () -> {
            InvocationHandler hh = (p, m, ar) -> InvocationHandler.invokeDefault(p, m, ar);
            return ((Thr) proxy(Thr.class, hh)).t();
        });
        row("proxy basics", () -> { Object p = proxy(Iface.class, h); return Proxy.isProxyClass(p.getClass()) + " " + (Proxy.getInvocationHandler(p) == h) + " " + p.getClass().getInterfaces().length; });
        row("proxy same class", () -> proxy(Iface.class, h).getClass() == proxy(Iface.class, h).getClass());
        row("proxy typename", () -> { Object p = proxy(Iface.class, h); return p.getClass().getTypeName().replaceAll("[0-9]+", "N") + " " + p.getClass().arrayType().getTypeName().replaceAll("[0-9]+", "N"); });
        row("mt changeReturnType", () -> MethodHandles.lookup().findVirtual(Iface.class, "add", java.lang.invoke.MethodType.methodType(int.class, int.class, int.class)).type().changeReturnType(long.class).toString());
        for (int i = 0; i < LABELS.size(); i++) System.out.println(LABELS.get(i) + " = " + ROWS.get(i).get());
        for (int r = 0; r < 60; r++) for (int i = 0; i < LABELS.size(); i++) ROWS.get(i).get();
        System.out.println("rows=" + LABELS.size());
        for (int i = 0; i < LABELS.size(); i++) System.out.println("hot " + LABELS.get(i) + " = " + ROWS.get(i).get());
    }
}
