import java.lang.invoke.*;

/**
 * A `MethodHandle` for a variable-arity method must come back as a VARARGS
 * COLLECTOR, so `asType` to a longer, fixed-arity type collects the trailing
 * arguments into the array instead of failing on the arity.
 *
 * JRuby's `InvokeSite.<init>` does exactly that through invokebinder's
 * `Binder.invokeVirtualQuiet`, and a non-collector handle surfaces as
 * `WrongMethodTypeException: cannot explicitly cast MethodHandle(..., X[])R to
 * (..., X, X)R` — which is what `JRubyScriptTemplateTests` dies on.
 */
public class VarargsCollectorProbe {
    static int fails = 0;

    public static String var2(String a, String... rest) {
        StringBuilder sb = new StringBuilder(a);
        for (String r : rest) sb.append("|").append(r);
        return sb.toString();
    }

    public String ivar2(String a, String... rest) { return "i:" + var2(a, rest); }

    static Object fixedStatic(String a, String b) { return a + "/" + b; }

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

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup L = MethodHandles.lookup();

        MethodHandle st = L.findStatic(VarargsCollectorProbe.class, "var2",
                MethodType.methodType(String.class, String.class, String[].class));
        run("findStatic.isVarargsCollector", () -> String.valueOf(st.isVarargsCollector()), "true");
        run("findStatic.type", () -> st.type().toString(), "(String,String[])String");
        run("static asType(+2)", () -> {
            MethodHandle a = st.asType(MethodType.methodType(String.class, String.class, String.class, String.class));
            return (String) a.invokeExact("a", "b", "c");
        }, "a|b|c");
        run("static asType(+0)", () -> {
            MethodHandle a = st.asType(MethodType.methodType(String.class, String.class));
            return (String) a.invokeExact("a");
        }, "a");
        run("static invoke (inexact, collects)", () -> (String) st.invoke("a", "b", "c"), "a|b|c");

        MethodHandle vi = L.findVirtual(VarargsCollectorProbe.class, "ivar2",
                MethodType.methodType(String.class, String.class, String[].class));
        run("findVirtual.isVarargsCollector", () -> String.valueOf(vi.isVarargsCollector()), "true");
        run("virtual asType(+2)", () -> {
            MethodHandle a = vi.asType(MethodType.methodType(String.class, VarargsCollectorProbe.class,
                    String.class, String.class, String.class));
            return (String) a.invokeExact(new VarargsCollectorProbe(), "a", "b", "c");
        }, "i:a|b|c");
        run("virtual invoke (inexact, collects)", () -> (String) vi.invoke(new VarargsCollectorProbe(), "a", "b"), "i:a|b");

        MethodHandle unreflected = L.unreflect(VarargsCollectorProbe.class.getMethod("var2", String.class, String[].class));
        run("unreflect.isVarargsCollector", () -> String.valueOf(unreflected.isVarargsCollector()), "true");
        run("unreflect asType(+2)", () -> {
            MethodHandle a = unreflected.asType(MethodType.methodType(String.class, String.class, String.class, String.class));
            return (String) a.invokeExact("a", "b", "c");
        }, "a|b|c");

        // asVarargsCollector applied by hand, on a handle that was not variadic.
        MethodHandle raw = L.findStatic(VarargsCollectorProbe.class, "var2",
                MethodType.methodType(String.class, String.class, String[].class)).asFixedArity();
        run("asFixedArity.isVarargsCollector", () -> String.valueOf(raw.isVarargsCollector()), "false");
        run("hand-made collector.isVarargsCollector",
                () -> String.valueOf(raw.asVarargsCollector(String[].class).isVarargsCollector()), "true");
        run("hand-made collector asType(+2)", () -> {
            MethodHandle a = raw.asVarargsCollector(String[].class)
                    .asType(MethodType.methodType(String.class, String.class, String.class, String.class));
            return (String) a.invokeExact("a", "b", "c");
        }, "a|b|c");

        // A non-variadic handle must NOT become a collector.
        MethodHandle fixed = L.findStatic(VarargsCollectorProbe.class, "fixedStatic",
                MethodType.methodType(Object.class, String.class, String.class));
        run("fixed.isVarargsCollector", () -> String.valueOf(fixed.isVarargsCollector()), "false");

        // The Constructor / MethodHandles.Lookup.findConstructor variant.
        run("varargs ctor collector", () -> String.valueOf(
                L.findConstructor(java.lang.ProcessBuilder.class,
                        MethodType.methodType(void.class, String[].class)).isVarargsCollector()), "true");

        System.out.println("TOTALFAILS=" + fails);
    }
}
