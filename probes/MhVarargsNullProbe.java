import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.List;

/**
 * A bare `null` supplied as the SOLE varargs argument through a
 * `MethodHandle`'s generic (`invokeWithArguments`) entry point must be
 * COLLECTED into a one-element array, not passed through as the array
 * reference itself.
 *
 * The JDK decides this in `MethodHandleImpl$AsVarargsCollector.asType`: the
 * "pass it straight through" shortcut is taken only when the caller's
 * trailing parameter type is assignable to the collector's array type.
 * `invokeWithArguments` always adapts to `genericMethodType(n)`, whose
 * trailing parameter is `Object` -- never assignable to `String[]` -- so the
 * collector always wraps. `invokeExact((String[]) null)` names the array type
 * statically and must NOT wrap.
 *
 * CratonVM applies varargs semantics at DISPATCH from the runtime values, and
 * a null carries no static type; it treated a null in the array slot as
 * "already packed" on every entry point. Spring's SpEL
 * `VariableAndFunctionTests.functionViaMethodHandleForStaticMethodThatAccepts
 * OnlyVarargs` -- `#varargsFunctionHandle(null)` -- is the reported victim:
 * expected `[null]`, got `null`.
 */
public class MhVarargsNullProbe {

    public static String varargsFunction(String... strings) {
        return Arrays.toString(strings);
    }

    public static String mixedVarargs(String a, String... strings) {
        return a + "|" + Arrays.toString(strings);
    }

    public static String objVarargs(Object... o) {
        return Arrays.toString(o);
    }

    public static String intVarargs(int... v) {
        return Arrays.toString(v);
    }

    public static String fixedArray(String[] a) {
        return "fixed:" + Arrays.toString(a);
    }

    public String ivarargs(String... strings) {
        return "i:" + Arrays.toString(strings);
    }

    static final class Holder {
        final String v;
        Holder(String... parts) { this.v = Arrays.toString(parts); }
        @Override public String toString() { return "H" + v; }
    }

    interface Thrower { Object get() throws Throwable; }

    static void row(String name, Thrower t) {
        String v;
        try {
            Object o = t.get();
            v = String.valueOf(o);
        }
        catch (Throwable ex) {
            v = "THREW " + ex.getClass().getName() + ": " + ex.getMessage();
        }
        System.out.println(name + " = " + v);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup L = MethodHandles.lookup();

        MethodHandle vf = L.findStatic(MhVarargsNullProbe.class, "varargsFunction",
                MethodType.methodType(String.class, String[].class));
        MethodHandle mx = L.findStatic(MhVarargsNullProbe.class, "mixedVarargs",
                MethodType.methodType(String.class, String.class, String[].class));
        MethodHandle ov = L.findStatic(MhVarargsNullProbe.class, "objVarargs",
                MethodType.methodType(String.class, Object[].class));
        MethodHandle iv = L.findStatic(MhVarargsNullProbe.class, "intVarargs",
                MethodType.methodType(String.class, int[].class));
        MethodHandle fx = L.findStatic(MhVarargsNullProbe.class, "fixedArray",
                MethodType.methodType(String.class, String[].class));
        MethodHandle inst = L.findVirtual(MhVarargsNullProbe.class, "ivarargs",
                MethodType.methodType(String.class, String[].class));
        MethodHandle ctor = L.findConstructor(Holder.class,
                MethodType.methodType(void.class, String[].class));

        // ---- collector marking, for orientation -------------------------
        row("A01 vf.isVarargsCollector", () -> vf.isVarargsCollector());
        row("A02 mx.isVarargsCollector", () -> mx.isVarargsCollector());
        row("A03 fx.isVarargsCollector", () -> fx.isVarargsCollector());
        row("A04 inst.isVarargsCollector", () -> inst.isVarargsCollector());
        row("A05 ctor.isVarargsCollector", () -> ctor.isVarargsCollector());
        row("A06 vf.type", () -> vf.type());

        // ---- invokeWithArguments: the generic (Object-typed) entry -------
        // Every row here adapts to genericMethodType(n), so a null in the
        // trailing slot is COLLECTED on every JDK.
        row("B01 vf.iwa(null)", () -> vf.invokeWithArguments(new Object[] {null}));
        row("B02 vf.iwa()", () -> vf.invokeWithArguments());
        row("B03 vf.iwa(a)", () -> vf.invokeWithArguments("a"));
        row("B04 vf.iwa(a,null,b)", () -> vf.invokeWithArguments("a", null, "b"));
        row("B05 vf.iwa(null,null)", () -> vf.invokeWithArguments(null, null));
        row("B06 vf.iwa(new String[0])", () -> vf.invokeWithArguments(new Object[] {new String[0]}));
        row("B07 vf.iwaList([null])", () -> vf.invokeWithArguments(Arrays.asList((Object) null)));
        row("B08 mx.iwa(a,null)", () -> mx.invokeWithArguments("a", null));
        row("B09 mx.iwa(a)", () -> mx.invokeWithArguments("a"));
        row("B10 mx.iwa(null,null)", () -> mx.invokeWithArguments(null, null));
        row("B11 ov.iwa(null)", () -> ov.invokeWithArguments(new Object[] {null}));
        row("B12 ov.iwa(null,null)", () -> ov.invokeWithArguments(null, null));
        row("B13 iv.iwa(1,2)", () -> iv.invokeWithArguments(1, 2));
        row("B14 iv.iwa()", () -> iv.invokeWithArguments());
        row("B15 fx.iwa(null) [non-varargs param]", () -> fx.invokeWithArguments(new Object[] {null}));
        row("B16 inst.iwa(recv,null)", () -> inst.invokeWithArguments(new MhVarargsNullProbe(), null));
        row("B17 ctor.iwa(null)", () -> ctor.invokeWithArguments(new Object[] {null}));
        row("B18 ctor.iwa(a,b)", () -> ctor.invokeWithArguments("a", "b"));

        // ---- invokeExact / invoke: the STATICALLY TYPED entries ----------
        // `(String[]) null` names the array type, so it must pass straight
        // through and `Arrays.toString` sees a null array.
        row("C01 vf.invokeExact((String[])null)", () -> (String) vf.invokeExact((String[]) null));
        row("C02 vf.invoke((String[])null)", () -> (String) vf.invoke((String[]) null));
        row("C03 vf.invoke((Object)null)", () -> (String) vf.invoke((Object) null));
        row("C04 vf.invoke(a,b)", () -> (String) vf.invoke("a", "b"));
        row("C05 vf.invoke()", () -> (String) vf.invoke());
        row("C06 fx.invokeExact((String[])null)", () -> (String) fx.invokeExact((String[]) null));
        row("C07 mx.invoke(a,(Object)null)", () -> (String) mx.invoke("a", (Object) null));
        row("C08 mx.invokeExact(a,(String[])null)", () -> (String) mx.invokeExact("a", (String[]) null));
        row("C09 ov.invoke((Object)null)", () -> (String) ov.invoke((Object) null));
        row("C10 ov.invokeExact((Object[])null)", () -> (String) ov.invokeExact((Object[]) null));

        // ---- asType, the mechanism the JDK actually uses -----------------
        row("D01 vf.asType((Object)Object).iwa(null)", () -> vf
                .asType(MethodType.methodType(Object.class, Object.class))
                .invokeWithArguments(new Object[] {null}));
        row("D02 vf.asType((String[])String).iwa(null)", () -> vf
                .asType(MethodType.methodType(String.class, String[].class))
                .invokeWithArguments(new Object[] {null}));
        row("D03 vf.asFixedArity().iwa(null)", () -> vf.asFixedArity()
                .invokeWithArguments(new Object[] {null}));
        row("D04 fx.asVarargsCollector(String[]).iwa(null)", () -> fx
                .asVarargsCollector(String[].class)
                .invokeWithArguments(new Object[] {null}));
        row("D05 vf.asCollector(String[],1).iwa(null)", () -> vf.asFixedArity()
                .asCollector(String[].class, 1)
                .invokeWithArguments(new Object[] {null}));

        // ---- the exact shape Spring's FunctionReference builds -----------
        // (see spring-expression FunctionReference.executeFunctionViaMethodHandle)
        row("E01 spel(#f(null))", () -> spel(vf, new Object[] {null}));
        row("E02 spel(#f())", () -> spel(vf, new Object[] {}));
        row("E03 spel(#f(a))", () -> spel(vf, new Object[] {"a"}));
        row("E04 spel(#f(new String[0]))", () -> spel(vf, new Object[] {new String[0]}));
        row("E05 spel(#f(a,null,b))", () -> spel(vf, new Object[] {"a", null, "b"}));
        row("E06 spel(#f(a,b,c))", () -> spel(vf, new Object[] {"a", "b", "c"}));
        row("E07 spel-mixed(#m(a,null))", () -> spel(mx, new Object[] {"a", null}));
        row("E08 spel-mixed(#m(a))", () -> spel(mx, new Object[] {"a"}));

        // ---- reflection twin: Method.invoke must be UNAFFECTED -----------
        java.lang.reflect.Method rm =
                MhVarargsNullProbe.class.getMethod("varargsFunction", String[].class);
        row("F01 Method.invoke(null-array)", () -> rm.invoke(null, new Object[] {null}));
        row("F02 Method.invoke(new String[]{null})",
                () -> rm.invoke(null, new Object[] {new String[] {null}}));
        row("F03 plain java varargsFunction((String[])null)",
                () -> varargsFunction((String[]) null));
        row("F04 plain java varargsFunction(null-as-element)",
                () -> varargsFunction(new String[] {null}));

        // ---- the inexact `invoke` door, by CALL-SITE parameter type -------
        // `invoke` compiles to `asType(callSiteType)`, so what the caller
        // WROTE at the call site decides whether the collector wraps. These
        // rows are the ones a VM that cannot see its call-site descriptor
        // gets wrong.
        row("G01 vf.invoke(new String[0]) [exact array type]",
                () -> (String) vf.invoke(new String[0]));
        row("G02 vf.invoke((Object) new String[0])",
                () -> (String) vf.invoke((Object) new String[0]));
        row("G03 ov.invoke((String[]) null) [Object[] param, String[] site]",
                () -> (String) ov.invoke((String[]) null));
        row("G04 ov.invoke(new String[] {\"a\"}) [subtype array]",
                () -> (String) ov.invoke(new String[] {"a"}));
        row("G05 mx.invoke(\"a\", (Object) new String[] {\"b\"})",
                () -> (String) mx.invoke("a", (Object) new String[] {"b"}));
        row("G06 mx.invoke(\"a\", new String[] {\"b\"}) [exact array type]",
                () -> (String) mx.invoke("a", new String[] {"b"}));
        row("G07 vf.invoke((Object) \"a\")", () -> (String) vf.invoke((Object) "a"));
        row("G08 iv.invoke((Object) null)", () -> (String) iv.invoke((Object) null));
        row("G09 iv.invoke((int[]) null)", () -> (String) iv.invoke((int[]) null));
        // A VIRTUAL collector: the call site names the receiver, `MH_DESC` does
        // not, so the two have to be aligned before the trailing types can be
        // compared at all.
        row("G10 inst.invoke(recv,(Object) null)",
                () -> (String) inst.invoke(new MhVarargsNullProbe(), (Object) null));
        row("G11 inst.invoke(recv,(String[]) null)",
                () -> (String) inst.invoke(new MhVarargsNullProbe(), (String[]) null));
        row("G12 inst.invoke(recv,new String[] {a})",
                () -> (String) inst.invoke(new MhVarargsNullProbe(), new String[] {"a"}));
        row("G13 inst.invoke(recv,a,b)",
                () -> (String) inst.invoke(new MhVarargsNullProbe(), "a", "b"));

        // ---- a FIXED-ARITY handle refuses; it does not gather -------------
        MethodHandle fa = vf.asFixedArity();
        row("H01 fa.isVarargsCollector", () -> fa.isVarargsCollector());
        row("H02 fa.iwa(x,y)", () -> fa.invokeWithArguments("x", "y"));
        row("H03 fa.iwa(x)", () -> fa.invokeWithArguments("x"));
        row("H04 fa.iwa(new String[]{x})",
                () -> fa.invokeWithArguments(new Object[] {new String[] {"x"}}));
        row("H05 fa.iwa()", () -> fa.invokeWithArguments());
        row("H06 fa.invoke(x,y)", () -> (String) fa.invoke("x", "y"));
        row("H07 fa.invokeExact(new String[]{x})",
                () -> (String) fa.invokeExact(new String[] {"x"}));
        MethodHandle mfa = mx.asFixedArity();
        row("H08 mfa.iwa(a,b,c)", () -> mfa.invokeWithArguments("a", "b", "c"));
        row("H09 mfa.iwa(a,new String[]{b})",
                () -> mfa.invokeWithArguments("a", new String[] {"b"}));

        // ---- a NON-varargs target reached with too many arguments ---------
        // The JRuby shape `collect_trailing_varargs`' `arity_excess` trigger
        // exists for: a plain (non-ACC_VARARGS) array parameter, more flat
        // values than params. `fx` is exactly that method.
        row("H10 fx.iwa(a,b) [plain String[] param, 2 flat args]",
                () -> fx.invokeWithArguments("a", "b"));
        row("H11 fx.iwa(new String[]{a,b})",
                () -> fx.invokeWithArguments(new Object[] {new String[] {"a", "b"}}));
    }

    /** Mirrors FunctionReference.executeFunctionViaMethodHandle's varargs repackaging. */
    static Object spel(MethodHandle mh, Object[] functionArgs) throws Throwable {
        MethodType declaredParams = mh.type();
        int spelParamCount = functionArgs.length;
        int declaredParamCount = declaredParams.parameterCount();
        boolean isSuspectedVarargs = declaredParams.lastParameterType().isArray();
        if (isSuspectedVarargs) {
            if (declaredParamCount == 1 && !mh.isVarargsCollector()) {
                if (spelParamCount != 1 || !(functionArgs[0] instanceof String[])) {
                    String[] packed = new String[spelParamCount];
                    for (int i = 0; i < spelParamCount; i++) {
                        packed[i] = (String) functionArgs[i];
                    }
                    functionArgs = new Object[] {packed};
                }
            }
            else if (spelParamCount == declaredParamCount) {
                int actualVarargsIndex = functionArgs.length - 1;
                if (actualVarargsIndex >= 0 && functionArgs[actualVarargsIndex] instanceof Object[] argsToUnpack) {
                    Object[] newArgs = new Object[actualVarargsIndex + argsToUnpack.length];
                    System.arraycopy(functionArgs, 0, newArgs, 0, actualVarargsIndex);
                    System.arraycopy(argsToUnpack, 0, newArgs, actualVarargsIndex, argsToUnpack.length);
                    functionArgs = newArgs;
                }
            }
        }
        List<Object> shown = Arrays.asList(functionArgs);
        return mh.invokeWithArguments(functionArgs) + "   <- iwa" + shown;
    }
}
