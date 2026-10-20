// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.List;
import java.util.function.Supplier;
import java.util.stream.Collectors;

/**
 * SBR round 5, lane {@code walker}: StackWalker.walk and Class.getTypeName, diffed against HotSpot
 * (both modes). Prints one line per row; diff the output of {@code java} and {@code cratonvm}.
 */
public final class Sbr5WalkerBehavior {
    interface Iface { Object call(); }
    static class Inner { class Deep {} }
    enum E { A { void f() {} }; void f() {} }

    static void row(String name, Supplier<Object> s) {
        String r;
        try {
            r = String.valueOf(s.get());
        } catch (Throwable t) {
            r = "THROWS " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + " = " + r);
    }

    static String fmt(StackFrame f) {
        return f.getClassName() + "." + f.getMethodName() + ":" + f.getLineNumber() + " " + f.getFileName() + " native=" + f.isNativeMethod();
    }

    static List<String> top(StackWalker w, int n) {
        return w.walk(s -> s.limit(n).map(Sbr5WalkerBehavior::fmt).collect(Collectors.toList()));
    }

    static Object m1(StackWalker w) { return m2(w); }
    static Object m2(StackWalker w) { return m3(w); }
    static Object m3(StackWalker w) { return top(w, 4); }

    public static void main(String[] args) throws Throwable {
        StackWalker def = StackWalker.getInstance();
        StackWalker retain = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);
        StackWalker hidden = StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES);
        StackWalker refl = StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES);
        StackWalker drop = null;
        try { drop = StackWalker.getInstance(Option.valueOf("DROP_METHOD_INFO")); } catch (Throwable t) { System.out.println("drop-option " + t); }
        final StackWalker dropF = drop;

        row("chain", () -> m1(def));
        row("retain-chain", () -> m1(retain));
        row("count", () -> def.walk(s -> s.count()));
        row("depth-main", () -> def.walk(s -> s.map(StackFrame::getMethodName).collect(Collectors.toList())));
        row("toString", () -> def.walk(s -> s.limit(2).map(StackFrame::toString).collect(Collectors.toList())));
        row("toSTE", () -> def.walk(s -> s.limit(2).map(f -> f.toStackTraceElement().toString()).collect(Collectors.toList())));
        row("toSTE-jdk", () -> {
            List<String> l = new ArrayList<>();
            try {
                Object o = java.util.List.of(1).stream().map(x -> def.walk(s -> s.limit(6).map(f -> f.toStackTraceElement().toString()).collect(Collectors.toList()))).findFirst().get();
                l.add(String.valueOf(o));
            } catch (Throwable t) { l.add(t.toString()); }
            return l;
        });
        row("bci", () -> def.walk(s -> s.limit(1).map(f -> f.getByteCodeIndex() >= 0).findFirst().get()));
        row("declaringClass-noretain", () -> def.walk(s -> s.findFirst().get().getDeclaringClass()));
        row("declaringClass-retain", () -> retain.walk(s -> s.findFirst().get().getDeclaringClass()));
        row("declaringClass-after-walk", () -> retain.walk(s -> s.findFirst().get()).getDeclaringClass());
        row("methodType-noretain", () -> def.walk(s -> s.findFirst().get().getMethodType()));
        row("methodType-retain", () -> retain.walk(s -> s.findFirst().get().getMethodType()));
        row("descriptor-retain", () -> retain.walk(s -> s.findFirst().get().getDescriptor()));
        row("descriptor-noretain", () -> def.walk(s -> s.findFirst().get().getDescriptor()));
        row("lambda-hidden", () -> {
            Iface i = () -> hidden.walk(s -> s.limit(3).map(f -> f.getClassName().replaceAll("0x[0-9a-f]+", "0x") + "." + f.getMethodName()).collect(Collectors.toList()));
            return i.call();
        });
        row("lambda-default", () -> {
            Iface i = () -> def.walk(s -> s.limit(3).map(f -> f.getClassName() + "." + f.getMethodName()).collect(Collectors.toList()));
            return i.call();
        });
        row("reflect-frames", () -> {
            try {
                Method m = Sbr5WalkerBehavior.class.getDeclaredMethod("top", StackWalker.class, int.class);
                return ((List<?>) m.invoke(null, refl, 6)).stream().map(x -> x.toString().replaceAll("jdk.internal.reflect.\\S+", "R")).collect(Collectors.toList());
            } catch (Exception e) { return e; }
        });
        row("reflect-default", () -> {
            try {
                Method m = Sbr5WalkerBehavior.class.getDeclaredMethod("top", StackWalker.class, int.class);
                return m.invoke(null, def, 4);
            } catch (Exception e) { return e; }
        });
        row("mh-frames", () -> {
            try {
                MethodHandle mh = MethodHandles.lookup().findStatic(Sbr5WalkerBehavior.class, "top", java.lang.invoke.MethodType.methodType(List.class, StackWalker.class, int.class));
                return mh.invoke(hidden, 5);
            } catch (Throwable e) { return e; }
        });
        row("forEach", () -> { List<String> l = new ArrayList<>(); def.forEach(f -> l.add(f.getMethodName())); return l; });
        row("callerClass-noretain", () -> { try { return def.getCallerClass(); } catch (Throwable t) { return t; } });
        row("callerClass-retain", () -> retain.getCallerClass());
        row("callerClass-lambda", () -> { Iface i = () -> retain.getCallerClass(); return i.call(); });
        row("callerClass-from-m", () -> callerHelper(retain));
        row("depth-limit-skip", () -> def.walk(s -> s.skip(1).limit(1).map(StackFrame::getMethodName).collect(Collectors.toList())));
        row("walk-null", () -> def.walk(null));
        row("walk-stream-escape", () -> { try { return def.walk(s -> s).count(); } catch (Throwable t) { return t; } });
        row("nested-walk", () -> def.walk(s -> s.limit(1).map(f -> def.walk(t -> t.limit(1).map(StackFrame::getMethodName).findFirst().get())).findFirst().get()));
        row("walk-in-catch", () -> {
            try { throw new RuntimeException("x"); }
            catch (RuntimeException e) { return def.walk(s -> s.limit(2).map(StackFrame::getMethodName).collect(Collectors.toList())); }
        });
        row("walk-thread", () -> {
            List<Object> out = new ArrayList<>();
            Thread t = new Thread(() -> out.add(def.walk(s -> s.map(StackFrame::getMethodName).collect(Collectors.toList()))));
            t.start();
            try { t.join(); } catch (InterruptedException e) { }
            return out;
        });
        row("drop-method-info", () -> dropF == null ? "n/a" : dropF.walk(s -> s.limit(2).map(f -> {
            try { return f.getClassName() + "/" + f.getMethodName(); }
            catch (Throwable t) { return f.getClassName() + "/" + t.getClass().getName() + ":" + t.getMessage(); }
        }).collect(Collectors.toList())));
        row("drop-decl", () -> dropF == null ? "n/a" : dropF.walk(s -> s.findFirst().get().getDeclaringClass()));
        row("exception-in-fn", () -> { try { return def.walk(s -> { throw new IllegalStateException("boom"); }); } catch (Throwable t) { return t; } });
        row("sets", () -> StackWalker.getInstance(EnumSet.of(Option.RETAIN_CLASS_REFERENCE, Option.SHOW_HIDDEN_FRAMES)).walk(s -> s.limit(1).map(f -> f.getDeclaringClass().getName()).findFirst().get()));
        row("sets-depth", () -> StackWalker.getInstance(EnumSet.noneOf(Option.class), 2).walk(s -> s.count() >= 1));
        row("sets-depth-0", () -> StackWalker.getInstance(EnumSet.noneOf(Option.class), 0));

        Class<?>[] cs = { int.class, void.class, int[].class, int[][].class, String.class, String[].class, String[][][].class,
            Object[].class, Inner.class, Inner.Deep.class, Inner.Deep[].class, E.class, E.A.getClass(),
            Thread.State.class, Thread.State[].class, new Object() {}.getClass(), Iface.class, Iface[].class,
            Proxy.newProxyInstance(Sbr5WalkerBehavior.class.getClassLoader(), new Class<?>[]{ Iface.class }, (p, m, a) -> null).getClass(),
            ((Iface) () -> null).getClass(), java.util.Map.Entry.class, java.util.Map.Entry[][].class, Class.class, Class[].class };
        for (int i = 0; i < cs.length; i++) {
            final Class<?> c = cs[i];
            row("typeName-" + i, () -> c.getTypeName().replaceAll("0x[0-9a-f]+", "0x").replaceAll("\\$\\$Lambda/.*", "\\$\\$Lambda").replaceAll("\\$Proxy[0-9]+", "\\$ProxyN"));
        }
        System.out.println("typeName-identity-main = " + (String.class.getTypeName() == String.class.getName()) + " " + (int[].class.getTypeName() == int[].class.getTypeName()));
        row("lambda-array", () -> {
            Class<?> lc = ((Iface) () -> null).getClass();
            Object arr = java.lang.reflect.Array.newInstance(lc, 2);
            Class<?> ac = arr.getClass();
            return ac.getName().replaceAll("0x[0-9a-f]+", "0x") + " " + ac.getTypeName().replaceAll("0x[0-9a-f]+", "0x") + " " + ac.isArray() + " " + (ac == lc.arrayType()) + " " + (ac.getComponentType() == lc) + " " + (arr instanceof Object[]) + " " + java.lang.reflect.Array.getLength(arr);
        });
        row("lambda-array2", () -> {
            Class<?> lc = ((Iface) () -> null).getClass();
            Object arr = java.lang.reflect.Array.newInstance(lc, 1, 1);
            return arr.getClass().getTypeName().replaceAll("0x[0-9a-f]+", "0x") + " " + arr.getClass().getName().replaceAll("0x[0-9a-f]+", "0x");
        });
        row("lambda-array3", () -> {
            Class<?> lc = ((Iface) () -> null).getClass();
            Object arr = java.lang.reflect.Array.newInstance(lc, 2, 3);
            Class<?> ac = arr.getClass();
            return ac.getTypeName().replaceAll("0x[0-9a-f]+", "0x") + " " + (ac.getComponentType() == lc.arrayType()) + " " + (arr instanceof Object[][]) + " " + ((Object[]) arr).length + " " + ((Object[]) ((Object[]) arr)[1]).length + " " + (ac == lc.arrayType().arrayType());
        });
        row("hidden-array", () -> {
            try (java.io.InputStream in = Sbr5WalkerBehavior.class.getResourceAsStream("Sbr5WalkerBehavior$Inner.class")) {
                byte[] b = in.readAllBytes();
                Class<?> hc = MethodHandles.lookup().defineHiddenClass(b, false).lookupClass();
                Object a1 = java.lang.reflect.Array.newInstance(hc, 2);
                Object a2 = java.lang.reflect.Array.newInstance(hc, 2, 2);
                String n1 = a1.getClass().getName().replaceAll("0x[0-9a-f]+", "0x");
                String n2 = a2.getClass().getTypeName().replaceAll("0x[0-9a-f]+", "0x");
                return n1 + " " + n2 + " " + (a1.getClass().getComponentType() == hc) + " " + (a1.getClass() == hc.arrayType()) + " " + (a2.getClass().getComponentType() == hc.arrayType());
            } catch (Throwable t) { return t; }
        });
        row("bool-valueOf", () -> (Boolean.valueOf(true) == Boolean.TRUE) + " " + (Boolean.valueOf(false) == Boolean.FALSE) + " " + Boolean.valueOf(Boolean.parseBoolean("TRUE")));
        row("typeName-equals", () -> String[].class.getTypeName().equals("java.lang.String[]"));
        row("typeName-misc", () -> int[][].class.toString() + " " + int[][].class.getName() + " " + int[][].class.getSimpleName() + " " + int[][].class.getCanonicalName());
    }

    static Object callerHelper(StackWalker w) { return w.getCallerClass(); }
}
