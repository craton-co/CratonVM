import java.lang.invoke.*;
import java.util.*;

/**
 * Two properties every `MethodHandles` combinator has to hold, measured
 * separately so one cannot mask the other:
 *
 *   ARITY   — the returned handle's `type()` is the ADAPTED one. invokebinder's
 *             `Binder.invoke(target)` walks its transforms calling each one's
 *             `up()` (a combinator) and then hands the result to
 *             `MethodHandles.explicitCastArguments(handle, startType)`, which
 *             throws `WrongMethodTypeException` on an arity mismatch alone. So
 *             a combinator whose type does not move is the difference between
 *             JRuby's `InvokeSite.<init>` linking and throwing.
 *
 *   PURITY  — the ORIGINAL handle is left alone. A combinator that adapts its
 *             target in place corrupts every other holder of that handle, and
 *             the corruption only shows up in whatever they do next.
 *
 * `SmartBinder.collect(name, pattern, collectorHandle)` — the shape JRuby's
 * `InvokeSite.prepareBinder` uses to fold `arg0, arg1, ...` into the
 * `IRubyObject[] args` parameter — comes out as `collectArguments`, so that row
 * is the one that decides `JRubyScriptTemplateTests`.
 */
public class MhCombinatorProbe {
    static int fails = 0;

    public static String take2(String a, String[] rest) { return a + Arrays.toString(rest); }
    public static String[] mkArray(String a, String b) { return new String[]{a, b}; }
    public static String fixed3(String a, String b, String c) { return a + b + c; }
    public static String pre(String s) { return "<" + s + ">"; }

    static void check(String name, String got, String want) {
        boolean ok = got.equals(want);
        if (!ok) fails++;
        System.out.println((ok ? "OK   " : "FAIL ") + name + " => " + got + (ok ? "" : "  (want " + want + ")"));
    }

    interface Thrower { String get() throws Throwable; }

    static void run(String name, Thrower c, String want) {
        try { check(name, c.get(), want); }
        catch (Throwable t) { fails++; System.out.println("FAIL " + name + " threw " + t); }
    }

    /** Rendered from the MethodType's own components, so a `toString` bug cannot hide a type bug. */
    static String shape(MethodHandle h) { return shape(h.type()); }

    static String shape(MethodType t) {
        StringBuilder sb = new StringBuilder("(");
        for (int i = 0; i < t.parameterCount(); i++) {
            if (i > 0) sb.append(",");
            sb.append(t.parameterType(i).getSimpleName());
        }
        return sb.append(")").append(t.returnType().getSimpleName()).toString();
    }

    static MethodHandles.Lookup L = MethodHandles.lookup();

    static MethodHandle take2() throws Throwable {
        return L.findStatic(MhCombinatorProbe.class, "take2",
                MethodType.methodType(String.class, String.class, String[].class));
    }

    static MethodHandle fixed3() throws Throwable {
        return L.findStatic(MhCombinatorProbe.class, "fixed3",
                MethodType.methodType(String.class, String.class, String.class, String.class));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle mkArray = L.findStatic(MhCombinatorProbe.class, "mkArray",
                MethodType.methodType(String[].class, String.class, String.class));
        MethodHandle pre = L.findStatic(MhCombinatorProbe.class, "pre",
                MethodType.methodType(String.class, String.class));

        System.out.println("== collectArguments (the JRuby SmartBinder.collect shape)");
        run("collectArguments arity", () -> shape(MethodHandles.collectArguments(take2(), 1, mkArray)),
                "(String,String,String)String");
        run("collectArguments invoke",
                () -> (String) MethodHandles.collectArguments(take2(), 1, mkArray).invoke("a", "b", "c"),
                "a[b, c]");
        run("collectArguments at 0 (same arity)", () -> shape(MethodHandles.collectArguments(fixed3(), 0, pre)),
                "(String,String,String)String");

        System.out.println("== the full Binder shape: collect then explicitCastArguments to the start type");
        run("collect-then-explicitCast", () -> {
            MethodHandle collected = MethodHandles.collectArguments(take2(), 1, mkArray);
            MethodHandle cast = MethodHandles.explicitCastArguments(collected,
                    MethodType.methodType(String.class, String.class, String.class, String.class));
            return shape(cast);
        }, "(String,String,String)String");
        run("collect-then-explicitCast invoke", () -> {
            MethodHandle collected = MethodHandles.collectArguments(take2(), 1, mkArray);
            MethodHandle cast = MethodHandles.explicitCastArguments(collected,
                    MethodType.methodType(String.class, String.class, String.class, String.class));
            return (String) cast.invoke("a", "b", "c");
        }, "a[b, c]");

        System.out.println("== purity: the ORIGINAL handle must be untouched");
        run("asType leaves target alone", () -> {
            MethodHandle h = fixed3();
            h.asType(MethodType.methodType(Object.class, Object.class, Object.class, Object.class));
            return shape(h);
        }, "(String,String,String)String");
        run("asType returns a different handle", () -> {
            MethodHandle h = fixed3();
            MethodHandle a = h.asType(MethodType.methodType(Object.class, Object.class, Object.class, Object.class));
            return String.valueOf(a != h);
        }, "true");
        run("explicitCastArguments leaves target alone", () -> {
            MethodHandle h = fixed3();
            MethodHandles.explicitCastArguments(h,
                    MethodType.methodType(Object.class, Object.class, Object.class, Object.class));
            return shape(h);
        }, "(String,String,String)String");
        run("collectArguments leaves target alone", () -> {
            MethodHandle h = take2();
            MethodHandles.collectArguments(h, 1, mkArray);
            return shape(h);
        }, "(String,String[])String");
        run("asCollector leaves target alone", () -> {
            MethodHandle h = take2();
            h.asCollector(String[].class, 2);
            return shape(h);
        }, "(String,String[])String");
        run("asSpreader leaves target alone", () -> {
            MethodHandle h = fixed3();
            h.asSpreader(String[].class, 2);
            return shape(h);
        }, "(String,String,String)String");
        run("bindTo leaves target alone", () -> {
            MethodHandle h = fixed3();
            h.bindTo("a");
            return shape(h);
        }, "(String,String,String)String");
        run("insertArguments leaves target alone", () -> {
            MethodHandle h = fixed3();
            MethodHandles.insertArguments(h, 0, "a");
            return shape(h);
        }, "(String,String,String)String");
        run("dropArguments leaves target alone", () -> {
            MethodHandle h = fixed3();
            MethodHandles.dropArguments(h, 0, int.class);
            return shape(h);
        }, "(String,String,String)String");

        System.out.println("== the same handle adapted twice, from the ORIGINAL each time");
        run("two asTypes off one handle", () -> {
            MethodHandle h = fixed3();
            MethodHandle a = h.asType(MethodType.methodType(Object.class, String.class, String.class, String.class));
            MethodHandle b = h.asType(MethodType.methodType(String.class, Object.class, String.class, String.class));
            return shape(a) + " " + shape(b);
        }, "(String,String,String)Object (Object,String,String)String");

        System.out.println("TOTALFAILS=" + fails);
    }
}
