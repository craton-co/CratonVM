// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Proxy;
import java.lang.reflect.RecordComponent;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;
import java.util.stream.Collectors;

/**
 * SBR2 wave 8, lane ctorplan: Constructor.newInstance / getDeclaredConstructor / Class.newInstance
 * rows plus the held rows this cluster unblocks (StackWalker, getTypeName, RecordComponent,
 * invokeDefault). Output is diffed against HotSpot in both modes. Each row is evaluated twice
 * (cold then hot) so a per-Constructor plan cache cannot hide a first-call difference.
 */
public class Sbr2W8CtorplanRows {
    public static class Pub { public Pub() {} public String toString() { return "Pub"; } }
    public static class Args { final int a; final String s; public Args(int a, String s) { this.a = a; this.s = s; }
        public String toString() { return "Args" + a + s; } }
    public static class Boxed { final long l; public Boxed(long l) { this.l = l; } public String toString() { return "Boxed" + l; } }
    public static class Varargs { final int n; public Varargs(String... xs) { n = xs.length; } public String toString() { return "Varargs" + n; } }
    static class Priv { private Priv() {} private Priv(int x) {} public String toString() { return "Priv"; } }
    public static class Throws { public Throws() { throw new IllegalStateException("boom"); } }
    public static class Init { static { System.out.println("Init.clinit"); } public Init() {} }
    public static class BadInit { static { if (true) throw new RuntimeException("clinit"); } public BadInit() {} }
    public static abstract class Abs { public Abs() {} }
    public interface Iface {}
    public enum E { A }
    public class Inner { public Inner() {} }
    public record Rec(List<String> names, int i) {}
    public interface Dflt { default int d(int a) { return a + 1; } }

    static final List<String> out = new ArrayList<>();

    static void row(String name, Supplier<Object> s) {
        for (int pass = 0; pass < 3; pass++) {
            String r;
            try { r = String.valueOf(s.get()).replaceAll("@[0-9a-f]+", "@H"); }
            catch (Throwable t) {
                Throwable c = t.getCause();
                r = "EXC " + t.getClass().getName() + ": " + t.getMessage() + (c == null ? "" : " / cause " + c.getClass().getName() + ": " + c.getMessage());
            }
            System.out.println(name + (pass == 0 ? " cold " : " hot" + pass + " ") + r);
        }
    }

    interface Thrower { Object run() throws Throwable; }
    static Supplier<Object> w(Thrower t) {
        return () -> { try { return t.run(); } catch (Throwable e) { if (e instanceof RuntimeException re) throw re; throw new RuntimeException(e.getClass().getName() + ": " + e.getMessage(), e.getCause()); } };
    }

    public static void main(String[] a) throws Exception {
        row("ctor pub", w(() -> Pub.class.getConstructor().newInstance()));
        row("ctor args", w(() -> Args.class.getConstructor(int.class, String.class).newInstance(3, "x")));
        row("ctor args wrong count", w(() -> Args.class.getConstructor(int.class, String.class).newInstance(3)));
        row("ctor args wrong type", w(() -> Args.class.getConstructor(int.class, String.class).newInstance("s", "x")));
        row("ctor args null prim", w(() -> Args.class.getConstructor(int.class, String.class).newInstance(null, "x")));
        row("ctor widen", w(() -> Boxed.class.getConstructor(long.class).newInstance(5)));
        row("ctor narrow", w(() -> Boxed.class.getConstructor(long.class).newInstance(5.0)));
        row("ctor null args array", w(() -> Pub.class.getConstructor().newInstance((Object[]) null)));
        row("ctor varargs", w(() -> Varargs.class.getConstructor(String[].class).newInstance((Object) new String[] { "a", "b" })));
        row("ctor private noaccess", w(() -> Priv.class.getDeclaredConstructor().newInstance()));
        row("ctor private access", w(() -> { Constructor<Priv> c = Priv.class.getDeclaredConstructor(); c.setAccessible(true); return c.newInstance(); }));
        row("ctor throws", w(() -> Throws.class.getConstructor().newInstance()));
        row("ctor abstract", w(() -> Abs.class.getConstructor().newInstance()));
        row("ctor iface getConstructor", w(() -> Iface.class.getConstructor()));
        row("ctor enum", w(() -> E.class.getDeclaredConstructors().length));
        row("ctor inner", w(() -> Inner.class.getConstructor(Sbr2W8CtorplanRows.class).newInstance(new Sbr2W8CtorplanRows())));
        row("ctor inner null outer", w(() -> Inner.class.getConstructor(Sbr2W8CtorplanRows.class).newInstance((Object) null)));
        row("ctor init", w(() -> Init.class.getConstructor().newInstance()));
        row("ctor badinit", w(() -> BadInit.class.getConstructor().newInstance()));
        row("ctor jdk String", w(() -> String.class.getConstructor(String.class).newInstance("hi")));
        row("ctor jdk ArrayList", w(() -> ArrayList.class.getConstructor(int.class).newInstance(4).size()));
        row("ctor jdk private", w(() -> Object.class.getDeclaredConstructor().newInstance().getClass().getName()));
        row("ctor jdk nonpublic cls", w(() -> Class.forName("java.util.Collections$EmptyList").getDeclaredConstructor().newInstance()));
        row("ctor class", w(() -> Class.class.getDeclaredConstructors().length));
        row("gdc found", w(() -> Args.class.getDeclaredConstructor(int.class, String.class)));
        row("gdc nosuch", w(() -> Args.class.getDeclaredConstructor(String.class)));
        row("gdc nosuch msg", w(() -> Pub.class.getDeclaredConstructor(int.class, long[].class)));
        row("gdc same identity", w(() -> Pub.class.getDeclaredConstructor() == Pub.class.getDeclaredConstructor()));
        row("gdc equals", w(() -> Pub.class.getDeclaredConstructor().equals(Pub.class.getDeclaredConstructor())));
        row("gdc array", w(() -> int[].class.getDeclaredConstructor()));
        row("gdc prim", w(() -> int.class.getDeclaredConstructor()));
        row("ci pub", w(() -> Pub.class.newInstance()));
        row("ci priv", w(() -> Priv.class.newInstance()));
        row("ci abstract", w(() -> Abs.class.newInstance()));
        row("ci iface", w(() -> Iface.class.newInstance()));
        row("ci noarg missing", w(() -> Args.class.newInstance()));
        row("ci throws", w(() -> Throws.class.newInstance()));
        row("ci class", w(() -> Class.class.newInstance()));
        row("ci prim", w(() -> int.class.newInstance()));
        row("ci array", w(() -> int[].class.newInstance()));
        row("ci badinit", w(() -> BadInit.class.newInstance()));
        // StackWalker rows
        StackWalker sw = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
        row("sw caller", w(() -> helperCaller(sw)));
        row("sw walk", w(() -> sw.walk(s -> s.limit(3).map(f -> f.getClassName() + "." + f.getMethodName()).collect(Collectors.toList()))));
        row("sw walk info", w(() -> sw.walk(s -> s.limit(2).map(f -> f.getDeclaringClass().getSimpleName() + ":" + f.getByteCodeIndex() + ":" + f.getMethodType()).collect(Collectors.toList()))));
        row("sw foreach", w(() -> { List<String> l = new ArrayList<>(); StackWalker.getInstance().forEach(f -> l.add(f.getMethodName())); return l.subList(0, 2); }));
        row("sw depth", w(() -> deep(sw, 12)));
        row("sw toStackTraceElement", w(() -> StackWalker.getInstance().walk(s -> s.findFirst().get().toStackTraceElement().toString())));
        // getTypeName rows
        row("typename array", w(() -> String[][].class.getTypeName()));
        row("typename prim array", w(() -> int[].class.getTypeName()));
        row("typename plain", w(() -> Pub.class.getTypeName()));
        row("typename inner array", w(() -> Inner[].class.getTypeName()));
        row("typename anon", w(() -> new Object() {}.getClass().getTypeName()));
        // RecordComponent
        row("rc generic", w(() -> Rec.class.getRecordComponents()[0].getGenericType()));
        row("rc plain", w(() -> Rec.class.getRecordComponents()[1].getGenericType()));
        row("rc tostring", w(() -> { RecordComponent rc = Rec.class.getRecordComponents()[0]; return rc + " | " + rc.getGenericSignature(); }));
        // invokeDefault
        InvocationHandler h = (p, m, ar) -> m.isDefault() ? InvocationHandler.invokeDefault(p, m, ar) : (Object) 7;
        Dflt p = (Dflt) Proxy.newProxyInstance(Sbr2W8CtorplanRows.class.getClassLoader(), new Class<?>[] { Dflt.class }, h);
        row("invokeDefault ok", w(() -> p.d(4)));
        InvocationHandler bad = (px, m, ar) -> InvocationHandler.invokeDefault(px, m, "hi");
        Dflt pb = (Dflt) Proxy.newProxyInstance(Sbr2W8CtorplanRows.class.getClassLoader(), new Class<?>[] { Dflt.class }, bad);
        row("invokeDefault wrong arg", w(() -> pb.d(5)));
        InvocationHandler cnt = (px, m, ar) -> InvocationHandler.invokeDefault(px, m, 1, 2);
        Dflt pc = (Dflt) Proxy.newProxyInstance(Sbr2W8CtorplanRows.class.getClassLoader(), new Class<?>[] { Dflt.class }, cnt);
        row("invokeDefault wrong count", w(() -> pc.d(5)));
        InvocationHandler nn = (px, m, ar) -> InvocationHandler.invokeDefault(px, m, new Object[] { null });
        Dflt pn = (Dflt) Proxy.newProxyInstance(Sbr2W8CtorplanRows.class.getClassLoader(), new Class<?>[] { Dflt.class }, nn);
        row("invokeDefault null prim", w(() -> pn.d(5)));
    }

    static Object deep(StackWalker sw, int n) {
        if (n == 0) return sw.walk(s -> s.map(f -> f.getMethodName()).collect(Collectors.joining(",")));
        return deep(sw, n - 1);
    }

    static Class<?> helperCaller(StackWalker sw) { return sw.getCallerClass(); }
}
