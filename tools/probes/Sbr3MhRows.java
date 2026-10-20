// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.io.Serializable;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.function.IntUnaryOperator;
import java.util.function.Supplier;

/**
 * SBR round 2 wave 7 lane MH: one probe over every native-won row of the
 * {@code java.lang.invoke} MethodHandle surface (combinators, {@code Lookup.find*}
 * / {@code unreflect}, {@code MethodHandle.as*}, {@code VarHandle} helpers,
 * lambda linkage), value AND exception class+message rows, then a 200-iteration
 * drift re-check. Labels start with the retired row's name so a bisection run
 * ({@code CRATONVM_UNRETIRE_NATIVE_SHADOW}) can be attributed per row.
 * Output is one line per row, diffable against HotSpot.
 */
public class Sbr3MhRows {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    interface X {
        Object get() throws Throwable;
    }

    static String show(Object o) {
        if (o instanceof int[]) return Arrays.toString((int[]) o);
        if (o instanceof Object[]) return Arrays.deepToString((Object[]) o);
        if (o instanceof byte[]) return Arrays.toString((byte[]) o);
        return String.valueOf(o);
    }

    static void rowx(String label, X x) {
        LABELS.add(label);
        ROWS.add(() -> {
            try {
                return show(x.get()).replaceAll("@[0-9a-f]+", "@N").replaceAll("/0x[0-9a-f]+", "/N");
            } catch (Throwable t) {
                String m = t.getMessage();
                // fast-throw makes HotSpot drop NPE/AIOOBE messages after warm-up
                if (t instanceof NullPointerException || t instanceof ArrayIndexOutOfBoundsException) m = "";
                // lambda/hidden class names carry a per-run suffix
                if (m != null) m = m.replaceAll("0x[0-9a-f]+", "0xN").replaceAll("/0x?[0-9a-f]+", "/N");
                if (m != null) m = m.replaceAll("@[0-9a-f]+", "@N");
                return "!" + t.getClass().getName() + ": " + m;
            }
        });
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!!" + t.getClass().getName();
        }
    }

    static int inc(int x) { return x + 1; }
    static int add(int a, int b) { return a + b; }
    static int add3(int a, int b, int c) { return a * 100 + b * 10 + c; }
    static String cat(String a, String b) { return a + b; }
    static String cat3(String a, String b, String c) { return a + b + c; }
    static boolean pos(int x) { return x > 0; }
    static int neg(int x) { return -x; }
    static void nothing() { }
    static int thrower(int x) { throw new IllegalStateException("t" + x); }
    static int okay(int x) { return x * 2; }
    static int handler(IllegalStateException e, int x) { return -1 - x; }
    static String join(Object... xs) { return Arrays.toString(xs); }
    static String sfmt(String f, Object... xs) { return f + Arrays.toString(xs); }
    static int sum(int... xs) { int s = 0; for (int x : xs) s += x; return s; }
    static boolean lt4(int x) { return x < 4; }
    static int sumIter(int acc, Object o) { return acc + (Integer) o; }
    static int cleanup(Throwable t, int r, int x) { return r + 100 + (t == null ? 0 : 1000); }
    static int body(int v, int i) { return v + i; }
    static int sfield = 11;
    static int tfield = 11;
    static final int SFINAL = 3;
    public String hello(String s) { return "hi " + s; }
    public static String shello(String s) { return "shi " + s; }
    private int priv(int x) { return x * 3; }
    public int f = 5;
    public long lf = 6L;
    public String sf = "s";
    public final int ff = 8;
    public Sbr3MhRows() { }
    public Sbr3MhRows(int f) { this.f = f; }
    public static class Nested { public int v = 1; public Nested() { } }
    public static class Hid { public static String tag() { return "hid"; } }
    public static class DefMe { }
    public static class DefMe2 { }
    static int cmpLen(String a, String b) { return a.length() - b.length(); }

    static byte[] classBytes(Class<?> c) throws Exception {
        try (InputStream in = c.getResourceAsStream(c.getSimpleName() + ".class")) {
            return in.readAllBytes();
        }
    }

    public static void main(String[] args) throws Exception {
        final Lookup L = MethodHandles.lookup();
        final Class<?> C = Sbr3MhRows.class;
        final MethodType ii = MethodType.methodType(int.class, int.class);
        final MethodType iii = MethodType.methodType(int.class, int.class, int.class);
        final MethodType sss = MethodType.methodType(String.class, String.class, String.class);
        final MethodHandle INC = L.findStatic(C, "inc", ii);
        final MethodHandle ADD = L.findStatic(C, "add", iii);
        final MethodHandle ADD3 = L.findStatic(C, "add3", MethodType.methodType(int.class, int.class, int.class, int.class));
        final MethodHandle CAT = L.findStatic(C, "cat", sss);
        final MethodHandle CAT3 = L.findStatic(C, "cat3", MethodType.methodType(String.class, String.class, String.class, String.class));
        final MethodHandle POS = L.findStatic(C, "pos", MethodType.methodType(boolean.class, int.class));
        final MethodHandle NEG = L.findStatic(C, "neg", ii);
        final MethodHandle NOTHING = L.findStatic(C, "nothing", MethodType.methodType(void.class));
        final MethodHandle JOIN = L.findStatic(C, "join", MethodType.methodType(String.class, Object[].class));
        final MethodHandle SUM = L.findStatic(C, "sum", MethodType.methodType(int.class, int[].class));

        // ---- MethodHandles.identity / constant
        rowx("identity.int", () -> MethodHandles.identity(int.class).invoke(7));
        rowx("identity.String", () -> MethodHandles.identity(String.class).invoke("s"));
        rowx("identity.type", () -> MethodHandles.identity(long.class).type());
        rowx("identity.void", () -> MethodHandles.identity(void.class));
        rowx("identity.null", () -> MethodHandles.identity(null));
        rowx("identity.exact", () -> (int) MethodHandles.identity(int.class).invokeExact(9));
        rowx("constant.String", () -> MethodHandles.constant(String.class, "k").invoke());
        rowx("constant.int", () -> MethodHandles.constant(int.class, 5).invoke());
        rowx("constant.type", () -> MethodHandles.constant(Object.class, "x").type());
        rowx("constant.void", () -> MethodHandles.constant(void.class, null));
        rowx("constant.bad", () -> MethodHandles.constant(int.class, "x"));
        rowx("constant.null", () -> MethodHandles.constant(String.class, null).invoke());
        rowx("constant.nullprim", () -> MethodHandles.constant(int.class, null));
        // ---- insertArguments
        rowx("insertArguments.0", () -> MethodHandles.insertArguments(ADD, 0, 5).invoke(3));
        rowx("insertArguments.1", () -> MethodHandles.insertArguments(ADD, 1, 5).invoke(3));
        rowx("insertArguments.two", () -> MethodHandles.insertArguments(ADD3, 0, 1, 2).invoke(3));
        rowx("insertArguments.type", () -> MethodHandles.insertArguments(ADD3, 1, 1).type());
        rowx("insertArguments.none", () -> MethodHandles.insertArguments(ADD, 0).invoke(1, 2));
        rowx("insertArguments.tooMany", () -> MethodHandles.insertArguments(INC, 0, 1, 2));
        rowx("insertArguments.badpos", () -> MethodHandles.insertArguments(INC, 5, 1));
        rowx("insertArguments.badtype", () -> MethodHandles.insertArguments(INC, 0, "x"));
        // ---- dropArgumentsTrusted (via dropArguments)
        rowx("dropArgumentsTrusted.front", () -> MethodHandles.dropArguments(INC, 0, String.class).invoke("x", 4));
        rowx("dropArgumentsTrusted.back", () -> MethodHandles.dropArguments(INC, 1, String.class, long.class).invoke(4, "x", 2L));
        rowx("dropArgumentsTrusted.type", () -> MethodHandles.dropArguments(INC, 1, int.class).type());
        rowx("dropArgumentsTrusted.zero", () -> MethodHandles.dropArguments(INC, 0, new Class<?>[0]) == INC);
        rowx("dropArgumentsTrusted.badpos", () -> MethodHandles.dropArguments(INC, 3, String.class));
        rowx("dropArgumentsTrusted.list", () -> MethodHandles.dropArguments(INC, 0, List.of(String.class)).type());
        rowx("dropArgumentsTrusted.void", () -> MethodHandles.dropArguments(NOTHING, 0, int.class).invoke(3));
        // ---- filterArguments / filterReturnValue
        rowx("filterArguments.0", () -> MethodHandles.filterArguments(ADD, 0, NEG).invoke(3, 10));
        rowx("filterArguments.both", () -> MethodHandles.filterArguments(ADD, 0, NEG, NEG).invoke(3, 10));
        rowx("filterArguments.nullfilter", () -> MethodHandles.filterArguments(ADD, 0, (MethodHandle) null, NEG).invoke(3, 10));
        rowx("filterArguments.type", () -> MethodHandles.filterArguments(CAT, 0,
                L.findStatic(String.class, "valueOf", MethodType.methodType(String.class, int.class))).type());
        rowx("filterArguments.bad", () -> MethodHandles.filterArguments(ADD, 0, CAT));
        rowx("filterReturnValue.basic", () -> MethodHandles.filterReturnValue(INC, NEG).invoke(3));
        rowx("filterReturnValue.void", () -> MethodHandles.filterReturnValue(NOTHING, L.findStatic(C, "nothing", MethodType.methodType(void.class))).invoke());
        rowx("filterReturnValue.type", () -> MethodHandles.filterReturnValue(INC, POS).type());
        rowx("filterReturnValue.bad", () -> MethodHandles.filterReturnValue(INC, CAT));
        // ---- foldArguments / collectArguments
        rowx("foldArguments.basic", () -> MethodHandles.foldArguments(ADD, INC).invoke(3));
        rowx("foldArguments.void", () -> MethodHandles.foldArguments(INC, NOTHING).invoke(3));
        rowx("foldArguments.pos", () -> MethodHandles.foldArguments(ADD3, 1, ADD).invoke(1, 2, 3));
        rowx("foldArguments.type", () -> MethodHandles.foldArguments(ADD, INC).type());
        rowx("foldArguments.bad", () -> MethodHandles.foldArguments(INC, CAT));
        rowx("collectArguments.basic", () -> MethodHandles.collectArguments(ADD, 0, NEG).invoke(3, 4));
        rowx("collectArguments.void", () -> MethodHandles.collectArguments(INC, 0, NOTHING).invoke(3));
        rowx("collectArguments.type", () -> MethodHandles.collectArguments(ADD, 1, ADD).type());
        rowx("collectArguments.bad", () -> MethodHandles.collectArguments(ADD, 5, NEG));
        // ---- permuteArguments / guardWithTest / catchException
        rowx("permuteArguments.swap", () -> MethodHandles.permuteArguments(CAT, sss, 1, 0).invoke("a", "b"));
        rowx("permuteArguments.dup", () -> MethodHandles.permuteArguments(CAT, MethodType.methodType(String.class, String.class), 0, 0).invoke("a"));
        rowx("permuteArguments.drop", () -> MethodHandles.permuteArguments(INC, MethodType.methodType(int.class, String.class, int.class), 1).invoke("x", 4));
        rowx("permuteArguments.type", () -> MethodHandles.permuteArguments(CAT3, MethodType.methodType(String.class, String.class, String.class), 0, 1, 0).type());
        rowx("permuteArguments.bad", () -> MethodHandles.permuteArguments(CAT, sss, 0));
        rowx("permuteArguments.badidx", () -> MethodHandles.permuteArguments(CAT, sss, 0, 5));
        rowx("guardWithTest.true", () -> MethodHandles.guardWithTest(POS, INC, NEG).invoke(5));
        rowx("guardWithTest.false", () -> MethodHandles.guardWithTest(POS, INC, NEG).invoke(-5));
        rowx("guardWithTest.type", () -> MethodHandles.guardWithTest(POS, INC, NEG).type());
        rowx("guardWithTest.bad", () -> MethodHandles.guardWithTest(INC, INC, NEG));
        rowx("guardWithTest.badtype", () -> MethodHandles.guardWithTest(POS, INC, CAT));
        rowx("catchException.caught", () -> MethodHandles.catchException(
                L.findStatic(C, "thrower", ii), IllegalStateException.class,
                L.findStatic(C, "handler", MethodType.methodType(int.class, IllegalStateException.class, int.class))).invoke(1));
        rowx("catchException.notthrown", () -> MethodHandles.catchException(
                L.findStatic(C, "okay", ii), IllegalStateException.class,
                L.findStatic(C, "handler", MethodType.methodType(int.class, IllegalStateException.class, int.class))).invoke(4));
        rowx("catchException.other", () -> MethodHandles.catchException(
                L.findStatic(C, "thrower", ii), IllegalArgumentException.class,
                MethodHandles.dropArguments(INC, 0, IllegalArgumentException.class)).invoke(1));
        rowx("catchException.bad", () -> MethodHandles.catchException(INC, IllegalStateException.class, INC));
        // ---- arrayElementGetter
        rowx("arrayElementGetter.int", () -> MethodHandles.arrayElementGetter(int[].class).invoke(new int[] {4, 5}, 1));
        rowx("arrayElementGetter.obj", () -> MethodHandles.arrayElementGetter(String[].class).invoke(new String[] {"a", "b"}, 1));
        rowx("arrayElementGetter.long", () -> MethodHandles.arrayElementGetter(long[].class).invoke(new long[] {4, 5}, 0));
        rowx("arrayElementGetter.type", () -> MethodHandles.arrayElementGetter(byte[].class).type());
        rowx("arrayElementGetter.oob", () -> MethodHandles.arrayElementGetter(int[].class).invoke(new int[] {4}, 3));
        rowx("arrayElementGetter.npe", () -> MethodHandles.arrayElementGetter(int[].class).invoke((int[]) null, 0));
        rowx("arrayElementGetter.notarray", () -> MethodHandles.arrayElementGetter(String.class));
        rowx("arrayElementGetter.exact", () -> (int) MethodHandles.arrayElementGetter(int[].class).invokeExact(new int[] {4, 5}, 1));
        rowx("arrayElementSetter.int", () -> { int[] a = new int[2]; MethodHandles.arrayElementSetter(int[].class).invoke(a, 1, 9); return a; });
        // ---- byteArrayViewVarHandle
        rowx("byteArrayViewVarHandle.int", () -> {
            VarHandle vh = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
            byte[] b = new byte[8];
            vh.set(b, 0, 0x01020304);
            return b[0] + " " + b[3] + " " + (int) vh.get(b, 0);
        });
        rowx("byteArrayViewVarHandle.long", () -> {
            VarHandle vh = MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN);
            byte[] b = new byte[8];
            vh.set(b, 0, 0x0102030405060708L);
            return b[0] + " " + b[7] + " " + (long) vh.get(b, 0);
        });
        rowx("byteArrayViewVarHandle.type", () -> MethodHandles.byteArrayViewVarHandle(short[].class, ByteOrder.BIG_ENDIAN).varType());
        rowx("byteArrayViewVarHandle.coords", () -> MethodHandles.byteArrayViewVarHandle(char[].class, ByteOrder.BIG_ENDIAN).coordinateTypes());
        rowx("byteArrayViewVarHandle.bad", () -> MethodHandles.byteArrayViewVarHandle(byte[].class, ByteOrder.BIG_ENDIAN));
        rowx("byteArrayViewVarHandle.oob", () -> MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN).get(new byte[2], 0));
        rowx("byteArrayViewVarHandle.modes", () -> MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN).isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD));

        // ---- MethodHandle.as*
        rowx("asType.box", () -> INC.asType(MethodType.methodType(Object.class, Integer.class)).invoke((Object) 4));
        rowx("asType.widen", () -> INC.asType(MethodType.methodType(long.class, short.class)).invoke((short) 4));
        rowx("asType.bad", () -> INC.asType(MethodType.methodType(String.class, int.class)));
        rowx("asType.same", () -> INC.asType(ii) == INC);
        rowx("asType.type", () -> INC.asType(MethodType.methodType(Object.class, Object.class)).type());
        rowx("asType.cce", () -> INC.asType(MethodType.methodType(Object.class, Object.class)).invoke("x"));
        rowx("asType.arity", () -> INC.asType(MethodType.methodType(int.class, int.class, int.class)));
        rowx("asType.void", () -> INC.asType(MethodType.methodType(void.class, int.class)).invoke(1));
        rowx("asSpreader.int", () -> ADD.asSpreader(int[].class, 2).invoke(new int[] {3, 9}));
        rowx("asSpreader.obj", () -> CAT.asSpreader(Object[].class, 2).invoke(new Object[] {"p", "q"}));
        rowx("asSpreader.type", () -> ADD3.asSpreader(int[].class, 2).type());
        rowx("asSpreader.bad", () -> ADD.asSpreader(int[].class, 3));
        rowx("asSpreader.len", () -> ADD.asSpreader(int[].class, 2).invoke(new int[] {3}));
        rowx("asSpreader.null", () -> ADD.asSpreader(int[].class, 2).invoke((int[]) null));
        rowx("asCollector.obj", () -> JOIN.asCollector(Object[].class, 2).invoke("a", "b"));
        rowx("asCollector.zero", () -> JOIN.asCollector(Object[].class, 0).invoke());
        rowx("asCollector.int", () -> SUM.asCollector(int[].class, 3).invoke(1, 2, 3));
        rowx("asCollector.type", () -> SUM.asCollector(int[].class, 2).type());
        rowx("asCollector.bad", () -> INC.asCollector(int[].class, 1));
        rowx("asVarargsCollector.basic", () -> JOIN.asVarargsCollector(Object[].class).invoke("a", "b", "c"));
        rowx("asVarargsCollector.none", () -> JOIN.asVarargsCollector(Object[].class).invoke());
        rowx("asVarargsCollector.array", () -> JOIN.asVarargsCollector(Object[].class).invoke((Object) new Object[] {"z"}));
        rowx("asVarargsCollector.isVarargs", () -> JOIN.asVarargsCollector(Object[].class).isVarargsCollector());
        rowx("asVarargsCollector.type", () -> JOIN.asVarargsCollector(Object[].class).type());
        rowx("asVarargsCollector.bad", () -> INC.asVarargsCollector(int[].class));
        rowx("asFixedArity.basic", () -> JOIN.asVarargsCollector(Object[].class).asFixedArity().isVarargsCollector());
        rowx("asFixedArity.same", () -> INC.asFixedArity() == INC);
        rowx("asFixedArity.call", () -> JOIN.asVarargsCollector(Object[].class).asFixedArity().invoke((Object) new Object[] {"a"}));
        rowx("isVarargsCollector.no", () -> INC.isVarargsCollector());
        rowx("isVarargsCollector.jdk", () -> L.findStatic(C, "join", MethodType.methodType(String.class, Object[].class)).isVarargsCollector());
        rowx("isVarargsCollector.unreflect", () -> L.unreflect(C.getDeclaredMethod("join", Object[].class)).isVarargsCollector());
        rowx("isVarargsCollector.jdkvarargs", () -> L.findStatic(String.class, "format", MethodType.methodType(String.class, String.class, Object[].class)).isVarargsCollector());
        rowx("bindTo.basic", () -> L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class)).bindTo(new Sbr3MhRows()).invoke("z"));
        rowx("bindTo.type", () -> L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class)).bindTo(new Sbr3MhRows()).type());
        rowx("bindTo.bad", () -> L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class)).bindTo("x"));
        rowx("bindTo.prim", () -> ADD.bindTo(1));
        rowx("bindTo.static", () -> L.findStatic(C, "shello", MethodType.methodType(String.class, String.class)).bindTo("q").invoke());
        rowx("invokeWithArguments.basic", () -> ADD.invokeWithArguments(3, 4));
        rowx("invokeWithArguments.list", () -> ADD.invokeWithArguments(List.of(3, 4)));
        rowx("invokeWithArguments.arity", () -> ADD.invokeWithArguments(3));
        rowx("invokeWithArguments.cce", () -> ADD.invokeWithArguments("a", 4));
        rowx("invokeWithArguments.varargs", () -> JOIN.asVarargsCollector(Object[].class).invokeWithArguments("a", "b"));
        rowx("invokeWithArguments.void", () -> NOTHING.invokeWithArguments());
        rowx("invokeWithArguments.throws", () -> L.findStatic(C, "thrower", ii).invokeWithArguments(4));
        rowx("invokeExact.basic", () -> (int) INC.invokeExact(3));
        rowx("invokeExact.wrong", () -> (long) INC.invokeExact(3));
        rowx("invoke.adapt", () -> (long) INC.invoke((short) 3));
        rowx("type.basic", () -> INC.type());

        // ---- Lookup.find* / unreflect
        rowx("findStatic.basic", () -> L.findStatic(C, "inc", ii).invoke(1));
        rowx("findStatic.type", () -> L.findStatic(C, "inc", ii).type());
        rowx("findStatic.nosuch", () -> L.findStatic(C, "nope", ii));
        rowx("findStatic.nonstatic", () -> L.findStatic(C, "hello", MethodType.methodType(String.class, String.class)));
        rowx("findStatic.private", () -> MethodHandles.publicLookup().findStatic(C, "inc", ii));
        rowx("findStatic.jdk", () -> L.findStatic(Math.class, "abs", MethodType.methodType(int.class, int.class)).invoke(-4));
        rowx("findStatic.jdkpriv", () -> L.findStatic(String.class, "checkIndex", MethodType.methodType(void.class, int.class, int.class)));
        rowx("findStatic.varargs", () -> L.findStatic(String.class, "format", MethodType.methodType(String.class, String.class, Object[].class)).invoke("%s-%s", "a", "b"));
        rowx("findStatic.null", () -> L.findStatic(null, "inc", ii));
        rowx("findStatic.void", () -> L.findStatic(C, "nothing", MethodType.methodType(void.class)).invoke());
        rowx("findStatic.throws", () -> L.findStatic(C, "thrower", ii).invoke(2));
        rowx("findStatic.exact", () -> (int) L.findStatic(C, "add", iii).invokeExact(1, 2));
        rowx("findStatic.reflect", () -> L.revealDirect(L.findStatic(C, "inc", ii)).getName());
        rowx("findVirtual.basic", () -> L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class)).invoke(new Sbr3MhRows(), "v"));
        rowx("findVirtual.type", () -> L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class)).type());
        rowx("findVirtual.string", () -> L.findVirtual(String.class, "length", MethodType.methodType(int.class)).invoke("abcd"));
        rowx("findVirtual.iface", () -> L.findVirtual(List.class, "size", MethodType.methodType(int.class)).invoke(List.of(1, 2)));
        rowx("findVirtual.object", () -> L.findVirtual(Object.class, "toString", MethodType.methodType(String.class)).invoke(List.of(1)));
        rowx("findVirtual.nosuch", () -> L.findVirtual(C, "nope", ii));
        rowx("findVirtual.static", () -> L.findVirtual(C, "inc", ii));
        rowx("findVirtual.private", () -> L.findVirtual(C, "priv", ii).invoke(new Sbr3MhRows(), 3));
        rowx("findVirtual.npe", () -> L.findVirtual(String.class, "length", MethodType.methodType(int.class)).invoke((String) null));
        rowx("findVirtual.cce", () -> L.findVirtual(String.class, "length", MethodType.methodType(int.class)).invoke((Object) 5));
        rowx("findVirtual.invokeExact", () -> (int) L.findVirtual(String.class, "length", MethodType.methodType(int.class)).invokeExact("ab"));
        rowx("findVirtual.publicOnJdk", () -> MethodHandles.publicLookup().findVirtual(String.class, "length", MethodType.methodType(int.class)).invoke("xyz"));
        rowx("findConstructor.basic", () -> ((Sbr3MhRows) L.findConstructor(C, MethodType.methodType(void.class)).invoke()).f);
        rowx("findConstructor.arg", () -> ((Sbr3MhRows) L.findConstructor(C, MethodType.methodType(void.class, int.class)).invoke(41)).f);
        rowx("findConstructor.type", () -> L.findConstructor(C, MethodType.methodType(void.class, int.class)).type());
        rowx("findConstructor.string", () -> L.findConstructor(String.class, MethodType.methodType(void.class, char[].class)).invoke(new char[] {'o', 'k'}));
        rowx("findConstructor.abstract", () -> L.findConstructor(java.util.AbstractList.class, MethodType.methodType(void.class)));
        rowx("findConstructor.nosuch", () -> L.findConstructor(C, MethodType.methodType(void.class, String.class)));
        rowx("findConstructor.badret", () -> L.findConstructor(C, MethodType.methodType(int.class)));
        rowx("findConstructor.iface", () -> L.findConstructor(Runnable.class, MethodType.methodType(void.class)));
        rowx("findConstructor.array", () -> L.findConstructor(int[].class, MethodType.methodType(void.class)));
        rowx("findConstructor.nested", () -> ((Nested) L.findConstructor(Nested.class, MethodType.methodType(void.class)).invoke()).v);
        rowx("findGetter.int", () -> L.findGetter(C, "f", int.class).invoke(new Sbr3MhRows()));
        rowx("findGetter.long", () -> L.findGetter(C, "lf", long.class).invoke(new Sbr3MhRows()));
        rowx("findGetter.string", () -> L.findGetter(C, "sf", String.class).invoke(new Sbr3MhRows()));
        rowx("findGetter.type", () -> L.findGetter(C, "f", int.class).type());
        rowx("findGetter.nosuch", () -> L.findGetter(C, "nope", int.class));
        rowx("findGetter.wrongtype", () -> L.findGetter(C, "f", long.class));
        rowx("findGetter.static", () -> L.findGetter(C, "sfield", int.class));
        rowx("findGetter.npe", () -> L.findGetter(C, "f", int.class).invoke((Sbr3MhRows) null));
        rowx("findSetter.int", () -> { Sbr3MhRows r = new Sbr3MhRows(); L.findSetter(C, "f", int.class).invoke(r, 77); return r.f; });
        rowx("findSetter.string", () -> { Sbr3MhRows r = new Sbr3MhRows(); L.findSetter(C, "sf", String.class).invoke(r, "zz"); return r.sf; });
        rowx("findSetter.type", () -> L.findSetter(C, "f", int.class).type());
        rowx("findSetter.final", () -> L.findSetter(C, "ff", int.class));
        rowx("findSetter.nosuch", () -> L.findSetter(C, "nope", int.class));
        rowx("findSetter.static", () -> L.findSetter(C, "sfield", int.class));
        rowx("findSetter.cce", () -> L.findSetter(C, "sf", String.class).invoke(new Sbr3MhRows(), 5));
        rowx("findStaticGetter.int", () -> L.findStaticGetter(C, "sfield", int.class).invoke());
        rowx("findStaticGetter.const", () -> L.findStaticGetter(C, "SFINAL", int.class).invoke());
        rowx("findStaticGetter.type", () -> L.findStaticGetter(C, "sfield", int.class).type());
        rowx("findStaticGetter.nosuch", () -> L.findStaticGetter(C, "nope", int.class));
        rowx("findStaticGetter.inst", () -> L.findStaticGetter(C, "f", int.class));
        rowx("findStaticGetter.jdk", () -> L.findStaticGetter(Integer.class, "MAX_VALUE", int.class).invoke());
        rowx("findStaticSetter.int", () -> { L.findStaticSetter(C, "tfield", int.class).invoke(12); int r = tfield; tfield = 11; return r; });
        rowx("findStaticSetter.final", () -> L.findStaticSetter(C, "SFINAL", int.class));
        rowx("findVarHandle.get", () -> L.findVarHandle(C, "f", int.class).get(new Sbr3MhRows()));
        rowx("findVarHandle.set", () -> { Sbr3MhRows r = new Sbr3MhRows(); VarHandle v = L.findVarHandle(C, "f", int.class); v.set(r, 3); return r.f; });
        rowx("findVarHandle.cas", () -> { Sbr3MhRows r = new Sbr3MhRows(); VarHandle v = L.findVarHandle(C, "f", int.class);
            return v.compareAndSet(r, 5, 9) + " " + v.compareAndSet(r, 5, 10) + " " + r.f + " " + (int) v.getAndAdd(r, 2) + " " + r.f; });
        rowx("findVarHandle.long", () -> { Sbr3MhRows r = new Sbr3MhRows(); VarHandle v = L.findVarHandle(C, "lf", long.class);
            v.setVolatile(r, 99L); return (long) v.getAcquire(r); });
        rowx("findVarHandle.ref", () -> { Sbr3MhRows r = new Sbr3MhRows(); VarHandle v = L.findVarHandle(C, "sf", String.class);
            return v.getAndSet(r, "n") + " " + r.sf; });
        rowx("findVarHandle.varType", () -> L.findVarHandle(C, "f", int.class).varType());
        rowx("findVarHandle.coords", () -> L.findVarHandle(C, "f", int.class).coordinateTypes());
        rowx("findVarHandle.nosuch", () -> L.findVarHandle(C, "nope", int.class));
        rowx("findVarHandle.wrongtype", () -> L.findVarHandle(C, "f", long.class));
        rowx("findVarHandle.static", () -> L.findVarHandle(C, "sfield", int.class));
        rowx("findVarHandle.final", () -> L.findVarHandle(C, "ff", int.class).get(new Sbr3MhRows()));
        rowx("findVarHandle.finalset", () -> { L.findVarHandle(C, "ff", int.class).set(new Sbr3MhRows(), 1); return "no"; });
        rowx("findVarHandle.cce", () -> L.findVarHandle(C, "f", int.class).get((Object) "x"));
        rowx("findVarHandle.npe", () -> L.findVarHandle(C, "f", int.class).get((Sbr3MhRows) null));
        rowx("findVarHandle.modes", () -> L.findVarHandle(C, "f", int.class).isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD)
                + " " + L.findVarHandle(C, "sf", String.class).isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD));
        rowx("findVarHandle.modetype", () -> L.findVarHandle(C, "f", int.class).accessModeType(VarHandle.AccessMode.COMPARE_AND_SET));
        rowx("findVarHandle.toMH", () -> (int) L.findVarHandle(C, "f", int.class).toMethodHandle(VarHandle.AccessMode.GET).invoke(new Sbr3MhRows()));
        rowx("findVarHandle.hasInvokeExact", () -> L.findVarHandle(C, "f", int.class).hasInvokeExactBehavior());
        rowx("findVarHandle.withExact", () -> L.findVarHandle(C, "f", int.class).withInvokeExactBehavior().hasInvokeExactBehavior());
        rowx("findVarHandle.withExactCall", () -> (int) L.findVarHandle(C, "f", int.class).withInvokeExactBehavior().get(new Sbr3MhRows()));
        rowx("findVarHandle.withExactBad", () -> L.findVarHandle(C, "f", int.class).withInvokeExactBehavior().get((Object) new Sbr3MhRows()));
        rowx("findVarHandle.withGeneric", () -> L.findVarHandle(C, "f", int.class).withInvokeExactBehavior().withInvokeBehavior().hasInvokeExactBehavior());
        rowx("findVarHandle.toString", () -> L.findVarHandle(C, "f", int.class).toString().replaceAll("@[0-9a-f]+", "@N"));
        rowx("findVarHandle.desc", () -> L.findVarHandle(C, "f", int.class).describeConstable().isPresent());
        rowx("findStaticVarHandle.get", () -> L.findStaticVarHandle(C, "sfield", int.class).get());
        rowx("findStaticVarHandle.set", () -> { VarHandle v = L.findStaticVarHandle(C, "tfield", int.class); v.set(21); int r = tfield; tfield = 11; return r; });
        rowx("findStaticVarHandle.cas", () -> { VarHandle v = L.findStaticVarHandle(C, "tfield", int.class); tfield = 21; String r = v.compareAndSet(21, 22) + " " + tfield; tfield = 11; return r; });
        rowx("findStaticVarHandle.types", () -> { VarHandle v = L.findStaticVarHandle(C, "sfield", int.class); return v.varType() + " " + v.coordinateTypes(); });
        rowx("findStaticVarHandle.nosuch", () -> L.findStaticVarHandle(C, "nope", int.class));
        rowx("findStaticVarHandle.inst", () -> L.findStaticVarHandle(C, "f", int.class));
        rowx("findStaticVarHandle.jdk", () -> L.findStaticVarHandle(Integer.class, "MAX_VALUE", int.class).get());
        rowx("unreflect.basic", () -> L.unreflect(C.getMethod("hello", String.class)).invoke(new Sbr3MhRows(), "u"));
        rowx("unreflect.type", () -> L.unreflect(C.getMethod("hello", String.class)).type());
        rowx("unreflect.static", () -> L.unreflect(C.getDeclaredMethod("inc", int.class)).invoke(1));
        rowx("unreflect.private", () -> L.unreflect(C.getDeclaredMethod("priv", int.class)).invoke(new Sbr3MhRows(), 2));
        rowx("unreflect.accessible", () -> { java.lang.reflect.Method m = String.class.getDeclaredMethod("isLatin1"); return L.unreflect(m); });
        rowx("unreflect.varargs", () -> L.unreflect(C.getDeclaredMethod("join", Object[].class)).isVarargsCollector());
        rowx("unreflect.jdk", () -> L.unreflect(String.class.getMethod("length")).invoke("hey"));
        rowx("unreflect.null", () -> L.unreflect(null));
        rowx("unreflectConstructor.basic", () -> ((Sbr3MhRows) L.unreflectConstructor(C.getConstructor(int.class)).invoke(4)).f);
        rowx("unreflectGetter.basic", () -> L.unreflectGetter(C.getField("f")).invoke(new Sbr3MhRows()));
        rowx("unreflectSetter.final", () -> L.unreflectSetter(C.getField("ff")));
        rowx("unreflectSetter.basic", () -> { Sbr3MhRows r = new Sbr3MhRows(); L.unreflectSetter(C.getField("f")).invoke(r, 6); return r.f; });
        rowx("unreflectVarHandle.basic", () -> L.unreflectVarHandle(C.getField("f")).get(new Sbr3MhRows()));
        rowx("findSpecial.basic", () -> L.findSpecial(C, "priv", ii, C).invoke(new Sbr3MhRows(), 2));
        rowx("bind.basic", () -> L.bind(new Sbr3MhRows(), "hello", MethodType.methodType(String.class, String.class)).invoke("b"));
        rowx("revealDirect.virtual", () -> L.revealDirect(L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class))).toString());
        rowx("revealDirect.getter", () -> L.revealDirect(L.findGetter(C, "f", int.class)).toString());
        rowx("revealDirect.ctor", () -> L.revealDirect(L.findConstructor(C, MethodType.methodType(void.class))).toString());
        rowx("revealDirect.combinator", () -> L.revealDirect(MethodHandles.insertArguments(ADD, 0, 1)));

        // ---- defineClass / defineHiddenClass
        rowx("defineHiddenClass.basic", () -> {
            Lookup h = L.defineHiddenClass(classBytes(Hid.class), true);
            Class<?> c = h.lookupClass();
            return c.isHidden() + " " + c.getName().replaceAll("/0x?[0-9a-f]+|/[0-9a-f]+$", "/N") + " "
                    + h.findStatic(c, "tag", MethodType.methodType(String.class)).invoke();
        });
        rowx("defineHiddenClass.nestmate", () -> {
            Lookup h = L.defineHiddenClass(classBytes(Hid.class), false, Lookup.ClassOption.NESTMATE);
            return h.lookupClass().getNestHost().getName() + " " + h.lookupModes();
        });
        rowx("defineHiddenClass.bad", () -> L.defineHiddenClass(new byte[] {1, 2, 3}, true));
        rowx("defineHiddenClass.null", () -> L.defineHiddenClass(null, true));
        rowx("defineHiddenClassWithClassData", () -> {
            Lookup h = L.defineHiddenClassWithClassData(classBytes(Hid.class), "data", true);
            return MethodHandles.classData(h, "_", String.class);
        });
        rowx("defineClass.bad", () -> L.defineClass(new byte[] {1, 2, 3}));
        rowx("defineClass.null", () -> L.defineClass(null));
        rowx("defineClass.nopackage", () -> MethodHandles.publicLookup().defineClass(new byte[0]));

        // ---- VarHandle helpers
        rowx("VarHandle.varType.field", () -> L.findVarHandle(C, "sf", String.class).varType());
        rowx("VarHandle.coordinateTypes.field", () -> L.findVarHandle(C, "sf", String.class).coordinateTypes());
        rowx("VarHandle.varType.array", () -> MethodHandles.arrayElementVarHandle(int[].class).varType());
        rowx("VarHandle.coordinateTypes.array", () -> MethodHandles.arrayElementVarHandle(int[].class).coordinateTypes());
        rowx("VarHandle.array.getset", () -> { int[] a = new int[3]; VarHandle v = MethodHandles.arrayElementVarHandle(int[].class); v.set(a, 1, 5);
            return (int) v.getAndAdd(a, 1, 2) + " " + a[1] + " " + v.compareAndSet(a, 1, 7, 8) + " " + Arrays.toString(a); });
        rowx("VarHandle.array.oob", () -> MethodHandles.arrayElementVarHandle(int[].class).get(new int[1], 4));
        rowx("VarHandle.array.store", () -> { Object[] a = new String[1]; MethodHandles.arrayElementVarHandle(Object[].class).set(a, 0, (Object) 5); return "no"; });
        rowx("VarHandle.accessModeType.array", () -> MethodHandles.arrayElementVarHandle(long[].class).accessModeType(VarHandle.AccessMode.SET));
        rowx("VarHandle.modes.array", () -> MethodHandles.arrayElementVarHandle(String[].class).isAccessModeSupported(VarHandle.AccessMode.GET_AND_BITWISE_OR));
        rowx("VarHandle.toMethodHandle.array", () -> (int) MethodHandles.arrayElementVarHandle(int[].class).toMethodHandle(VarHandle.AccessMode.GET).invoke(new int[] {4, 5}, 1));
        rowx("VarHandle.fences", () -> { VarHandle.fullFence(); VarHandle.acquireFence(); VarHandle.releaseFence(); return "ok"; });
        rowx("VarHandle.varType.view", () -> MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.BIG_ENDIAN).varType());

        // ---- lambda linkage (metafactory / altMetafactory)
        rowx("metafactory.lambda", () -> ((IntUnaryOperator) x -> x + 1).applyAsInt(1));
        rowx("metafactory.capture", () -> { int k = 5; IntUnaryOperator o = x -> x + k; return o.applyAsInt(1); });
        rowx("metafactory.methodref", () -> ((Supplier<String>) "abc"::toUpperCase).get());
        rowx("metafactory.staticref", () -> ((IntUnaryOperator) Sbr3MhRows::inc).applyAsInt(4));
        rowx("metafactory.ctorref", () -> ((Supplier<Nested>) Nested::new).get().v);
        rowx("metafactory.class", () -> { Runnable r = () -> { }; return r.getClass().isSynthetic() + " " + r.getClass().isHidden()
                + " " + r.getClass().getName().replaceAll("[0-9a-fx]+$", "N").replaceAll("/.*", "/N"); });
        rowx("metafactory.interfaces", () -> { Runnable r = () -> { }; return Arrays.toString(r.getClass().getInterfaces()); });
        rowx("metafactory.toString", () -> { Runnable r = () -> { }; return r.toString().replaceAll("[0-9a-fx@$/]+$", "N"); });
        rowx("altMetafactory.serializable", () -> { Runnable r = (Runnable & Serializable) () -> { }; return r instanceof Serializable; });
        rowx("altMetafactory.marker", () -> { Runnable r = (Runnable & java.util.RandomAccess) () -> { }; return r instanceof java.util.RandomAccess; });
        rowx("altMetafactory.serial", () -> {
            Supplier<String> s = (Supplier<String> & Serializable) () -> "ser";
            java.io.ByteArrayOutputStream bo = new java.io.ByteArrayOutputStream();
            try (java.io.ObjectOutputStream oo = new java.io.ObjectOutputStream(bo)) { oo.writeObject(s); }
            try (java.io.ObjectInputStream oi = new java.io.ObjectInputStream(new java.io.ByteArrayInputStream(bo.toByteArray()))) {
                return ((Supplier<?>) oi.readObject()).get();
            }
        });
        rowx("altMetafactory.bridges", () -> { Cmp c = (a, b) -> a.length() - b.length(); return c.compare("aa", "b") + " " + ((java.util.Comparator<String>) c).compare("a", "bb"); });
        rowx("LambdaMetafactory.direct", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.metafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class),
                    MethodType.methodType(int.class, int.class), INC, ii).getTarget();
            return ((IntUnaryOperator) mh.invoke()).applyAsInt(6);
        });
        rowx("LambdaMetafactory.badname", () -> java.lang.invoke.LambdaMetafactory.metafactory(L, "nope", MethodType.methodType(IntUnaryOperator.class),
                MethodType.methodType(int.class, int.class), INC, ii).getTarget().invoke());
        rowx("LambdaMetafactory.badtype", () -> java.lang.invoke.LambdaMetafactory.metafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class),
                MethodType.methodType(int.class, int.class), CAT, ii));
        rowx("LambdaMetafactory.nonInterface", () -> java.lang.invoke.LambdaMetafactory.metafactory(L, "x", MethodType.methodType(Object.class),
                MethodType.methodType(void.class), NOTHING, MethodType.methodType(void.class)));
        rowx("LambdaMetafactory.alt", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.altMetafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class),
                    MethodType.methodType(int.class, int.class), INC, ii, java.lang.invoke.LambdaMetafactory.FLAG_SERIALIZABLE).getTarget();
            return ((IntUnaryOperator) mh.invoke()).applyAsInt(6) + " " + (mh.invoke() instanceof Serializable);
        });
        rowx("LambdaMetafactory.altBadFlags", () -> java.lang.invoke.LambdaMetafactory.altMetafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class),
                MethodType.methodType(int.class, int.class), INC, ii, 1 << 7));
        rowx("ConstantCallSite.constant", () -> new java.lang.invoke.ConstantCallSite(MethodHandles.constant(String.class, "cs")).dynamicInvoker().invoke());
        rowx("MutableCallSite.setTarget", () -> { java.lang.invoke.MutableCallSite cs = new java.lang.invoke.MutableCallSite(INC);
            MethodHandle d = cs.dynamicInvoker(); int a = (int) d.invoke(1); cs.setTarget(NEG); return a + " " + (int) d.invoke(1); });

        // ---- second block: the non-suite siblings (round 2 wave 7 trial extras)
        rowx("toString.static", () -> INC.toString());
        rowx("toString.virtual", () -> L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class)).toString());
        rowx("toString.combinator", () -> MethodHandles.insertArguments(ADD, 0, 1).toString());
        rowx("toString.array", () -> MethodHandles.arrayElementGetter(int[].class).toString());
        rowx("arrayElementSetter.obj", () -> { String[] a = new String[2]; MethodHandles.arrayElementSetter(String[].class).invoke(a, 1, "z"); return a; });
        rowx("arrayElementSetter.type", () -> MethodHandles.arrayElementSetter(long[].class).type());
        rowx("arrayElementSetter.oob", () -> MethodHandles.arrayElementSetter(int[].class).invoke(new int[1], 5, 1));
        rowx("arrayElementSetter.store", () -> { Object[] a = new String[1]; MethodHandles.arrayElementSetter(Object[].class).invoke(a, 0, (Object) 5); return "no"; });
        rowx("arrayElementSetter.notarray", () -> MethodHandles.arrayElementSetter(String.class));
        rowx("arrayElementSetter.null", () -> MethodHandles.arrayElementSetter(null));
        rowx("arrayLength.int", () -> MethodHandles.arrayLength(int[].class).invoke(new int[3]));
        rowx("arrayLength.obj", () -> MethodHandles.arrayLength(String[].class).invoke(new String[2]));
        rowx("arrayLength.type", () -> MethodHandles.arrayLength(byte[].class).type());
        rowx("arrayLength.notarray", () -> MethodHandles.arrayLength(String.class));
        rowx("arrayLength.npe", () -> MethodHandles.arrayLength(int[].class).invoke((int[]) null));
        rowx("byteBufferViewVarHandle.get", () -> { java.nio.ByteBuffer bb = java.nio.ByteBuffer.allocate(8); VarHandle v = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
            v.set(bb, 0, 0x01020304); return bb.get(0) + " " + (int) v.get(bb, 0); });
        rowx("byteBufferViewVarHandle.type", () -> MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN).varType());
        rowx("byteBufferViewVarHandle.bad", () -> MethodHandles.byteBufferViewVarHandle(byte[].class, ByteOrder.BIG_ENDIAN));
        rowx("zero.int", () -> MethodHandles.zero(int.class).invoke());
        rowx("zero.ref", () -> MethodHandles.zero(String.class).invoke());
        rowx("zero.void", () -> MethodHandles.zero(void.class).type());
        rowx("empty.basic", () -> MethodHandles.empty(MethodType.methodType(int.class, String.class)).invoke("x"));
        rowx("empty.type", () -> MethodHandles.empty(MethodType.methodType(String.class, int.class, long.class)).type());
        rowx("invoker.basic", () -> MethodHandles.invoker(ii).invoke(INC, 4));
        rowx("invoker.type", () -> MethodHandles.invoker(ii).type());
        rowx("exactInvoker.basic", () -> (int) MethodHandles.exactInvoker(ii).invokeExact(INC, 4));
        rowx("exactInvoker.wrong", () -> (int) MethodHandles.exactInvoker(ii).invokeExact(ADD, 4));
        rowx("spreadInvoker.basic", () -> MethodHandles.spreadInvoker(iii, 0).invoke(ADD, new Object[] {3, 4}));
        rowx("spreadInvoker.type", () -> MethodHandles.spreadInvoker(iii, 1).type());
        rowx("explicitCastArguments.basic", () -> MethodHandles.explicitCastArguments(INC, MethodType.methodType(long.class, long.class)).invoke(5L));
        rowx("explicitCastArguments.ref", () -> MethodHandles.explicitCastArguments(INC, MethodType.methodType(Object.class, Object.class)).invoke((Object) 5));
        rowx("explicitCastArguments.bad", () -> MethodHandles.explicitCastArguments(INC, MethodType.methodType(int.class, int.class, int.class)));
        rowx("explicitCastArguments.bool", () -> MethodHandles.explicitCastArguments(INC, MethodType.methodType(boolean.class, boolean.class)).invoke(true));
        rowx("tryFinally.basic", () -> MethodHandles.tryFinally(INC, L.findStatic(C, "cleanup", MethodType.methodType(int.class, Throwable.class, int.class, int.class))).invoke(3));
        rowx("tryFinally.thrown", () -> MethodHandles.tryFinally(L.findStatic(C, "thrower", ii), L.findStatic(C, "cleanup", MethodType.methodType(int.class, Throwable.class, int.class, int.class))).invoke(3));
        rowx("tryFinally.bad", () -> MethodHandles.tryFinally(INC, INC));
        rowx("countedLoop.basic", () -> MethodHandles.countedLoop(MethodHandles.constant(int.class, 4), MethodHandles.constant(int.class, 0),
                L.findStatic(C, "body", MethodType.methodType(int.class, int.class, int.class))).invoke());
        rowx("countedLoop.type", () -> MethodHandles.countedLoop(MethodHandles.constant(int.class, 4), MethodHandles.constant(int.class, 0),
                L.findStatic(C, "body", MethodType.methodType(int.class, int.class, int.class))).type());
        rowx("whileLoop.bad", () -> MethodHandles.whileLoop(INC, INC, INC));
        rowx("doWhileLoop.bad", () -> MethodHandles.doWhileLoop(INC, INC, INC));
        rowx("iteratedLoop.bad", () -> MethodHandles.iteratedLoop(null, null, INC));
        rowx("findStaticSetter.basic", () -> { L.findStaticSetter(C, "tfield", int.class).invoke(14); int r = tfield; tfield = 11; return r; });
        rowx("findStaticSetter.type", () -> L.findStaticSetter(C, "tfield", int.class).type());
        rowx("findStaticSetter.nosuch", () -> L.findStaticSetter(C, "nope", int.class));
        rowx("findStaticSetter.nonstatic", () -> L.findStaticSetter(C, "f", int.class));
        rowx("findSpecial.type", () -> L.findSpecial(C, "priv", ii, C).type());
        rowx("findSpecial.nosuch", () -> L.findSpecial(C, "nope", ii, C));
        rowx("findSpecial.badcaller", () -> L.findSpecial(C, "priv", ii, String.class));
        rowx("findSpecial.super", () -> L.findSpecial(Object.class, "toString", MethodType.methodType(String.class), C).invoke(new Sbr3MhRows()).toString().startsWith("Sbr3MhRows@"));
        rowx("unreflectConstructor.type", () -> L.unreflectConstructor(C.getConstructor(int.class)).type());
        rowx("unreflectConstructor.null", () -> L.unreflectConstructor(null));
        rowx("unreflectGetter.type", () -> L.unreflectGetter(C.getField("lf")).type());
        rowx("unreflectGetter.null", () -> L.unreflectGetter(null));
        rowx("unreflectSetter.type", () -> L.unreflectSetter(C.getField("sf")).type());
        rowx("unreflectSetter.string", () -> { Sbr3MhRows r = new Sbr3MhRows(); L.unreflectSetter(C.getField("sf")).invoke(r, "q"); return r.sf; });
        rowx("unreflectSpecial.basic", () -> L.unreflectSpecial(C.getDeclaredMethod("priv", int.class), C).invoke(new Sbr3MhRows(), 4));
        rowx("unreflectSpecial.type", () -> L.unreflectSpecial(C.getDeclaredMethod("priv", int.class), C).type());
        rowx("unreflectVarHandle.type", () -> L.unreflectVarHandle(C.getField("f")).varType());
        rowx("unreflectVarHandle.set", () -> { Sbr3MhRows r = new Sbr3MhRows(); L.unreflectVarHandle(C.getField("f")).set(r, 9); return r.f; });
        rowx("unreflectVarHandle.static", () -> L.unreflectVarHandle(C.getDeclaredField("sfield")).get());
        rowx("toMethodHandle.set", () -> { Sbr3MhRows r = new Sbr3MhRows(); L.findVarHandle(C, "f", int.class).toMethodHandle(VarHandle.AccessMode.SET).invoke(r, 12); return r.f; });
        rowx("toMethodHandle.type", () -> L.findVarHandle(C, "f", int.class).toMethodHandle(VarHandle.AccessMode.COMPARE_AND_SET).type());
        rowx("toMethodHandle.array.type", () -> MethodHandles.arrayElementVarHandle(String[].class).toMethodHandle(VarHandle.AccessMode.GET_AND_SET).type());
        rowx("isAccessModeSupported.array", () -> MethodHandles.arrayElementVarHandle(int[].class).isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD));
        rowx("isAccessModeSupported.ref", () -> MethodHandles.arrayElementVarHandle(Object[].class).isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD));
        rowx("isAccessModeSupported.bool", () -> MethodHandles.arrayElementVarHandle(boolean[].class).isAccessModeSupported(VarHandle.AccessMode.GET_AND_BITWISE_AND));
        rowx("isAccessModeSupported.float", () -> MethodHandles.arrayElementVarHandle(float[].class).isAccessModeSupported(VarHandle.AccessMode.GET_AND_BITWISE_AND));
        rowx("isAccessModeSupported.view", () -> MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN).isAccessModeSupported(VarHandle.AccessMode.COMPARE_AND_SET));
        rowx("findClass.basic", () -> L.findClass("java.lang.String"));
        rowx("findClass.nosuch", () -> L.findClass("no.such.Klass"));
        rowx("findClass.nested", () -> L.findClass("Sbr3MhRows$Nested"));
        rowx("findClass.inaccessible", () -> MethodHandles.publicLookup().findClass("Sbr3MhRows$Nested"));

        // ---- third block: interactions the candidate rows must not disturb
        rowx("arrayElementGetter.bindTo", () -> MethodHandles.arrayElementGetter(int[].class).bindTo(new int[] {7, 8}).type());
        rowx("arrayElementGetter.bindTo.call", () -> MethodHandles.arrayElementGetter(int[].class).bindTo(new int[] {7, 8}).invoke(1));
        rowx("arrayElementGetter.insert", () -> MethodHandles.insertArguments(MethodHandles.arrayElementGetter(int[].class), 0, new int[] {7, 8}).invoke(1));
        rowx("arrayElementGetter.asType", () -> MethodHandles.arrayElementGetter(int[].class).asType(MethodType.methodType(Object.class, int[].class, int.class)).invoke(new int[] {7, 8}, 0));
        rowx("arrayElementGetter.filter", () -> MethodHandles.filterReturnValue(MethodHandles.arrayElementGetter(int[].class), NEG).invoke(new int[] {7, 8}, 0));
        rowx("arrayElementGetter.guard", () -> MethodHandles.dropArguments(MethodHandles.arrayElementGetter(int[].class), 0, String.class).invoke("x", new int[] {7, 8}, 1));
        rowx("arrayElementGetter.reveal", () -> L.revealDirect(MethodHandles.arrayElementGetter(int[].class)));
        rowx("arrayElementGetter.spread", () -> MethodHandles.arrayElementGetter(int[].class).asSpreader(Object[].class, 2).invoke(new Object[] {new int[] {7, 8}, 1}));
        rowx("arrayElementGetter.viaInvoker", () -> MethodHandles.invoker(MethodType.methodType(int.class, int[].class, int.class)).invoke(MethodHandles.arrayElementGetter(int[].class), new int[] {7, 8}, 1));
        rowx("VarHandle.varType.static", () -> L.findStaticVarHandle(C, "sfield", int.class).varType());
        rowx("VarHandle.coord.static", () -> L.findStaticVarHandle(C, "sfield", int.class).coordinateTypes());
        rowx("VarHandle.varType.byteview", () -> MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN).varType());
        rowx("VarHandle.coord.bufview", () -> MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN).coordinateTypes());
        rowx("VarHandle.varType.ms", () -> java.lang.foreign.ValueLayout.JAVA_INT.varHandle().varType());
        rowx("VarHandle.coord.ms", () -> java.lang.foreign.ValueLayout.JAVA_INT.varHandle().coordinateTypes());
        rowx("VarHandle.varType.ms.path", () -> java.lang.foreign.MemoryLayout.structLayout(java.lang.foreign.ValueLayout.JAVA_INT.withName("a"),
                java.lang.foreign.ValueLayout.JAVA_LONG.withName("b")).varHandle(java.lang.foreign.MemoryLayout.PathElement.groupElement("b")).varType());
        rowx("VarHandle.coord.ms.path", () -> java.lang.foreign.MemoryLayout.structLayout(java.lang.foreign.ValueLayout.JAVA_INT.withName("a"),
                java.lang.foreign.ValueLayout.JAVA_LONG.withName("b")).varHandle(java.lang.foreign.MemoryLayout.PathElement.groupElement("b")).coordinateTypes());
        rowx("VarHandle.varType.exact", () -> L.findVarHandle(C, "f", int.class).withInvokeExactBehavior().varType());
        rowx("VarHandle.coord.exact", () -> L.findVarHandle(C, "f", int.class).withInvokeExactBehavior().coordinateTypes());
        rowx("VarHandle.varType.ref", () -> L.findVarHandle(C, "sf", String.class).varType());
        rowx("VarHandle.varType.bool", () -> MethodHandles.arrayElementVarHandle(boolean[].class).varType());
        rowx("VarHandle.coord.immutable", () -> { java.util.List<Class<?>> l = L.findVarHandle(C, "f", int.class).coordinateTypes(); l.add(int.class); return l; });
        rowx("VarHandle.coord.same", () -> { VarHandle v = L.findVarHandle(C, "f", int.class); return v.coordinateTypes().equals(v.coordinateTypes()); });
        rowx("VarHandle.varType.atomic", () -> L.findVarHandle(java.util.concurrent.atomic.AtomicInteger.class, "value", int.class).varType());
        rowx("VarHandle.varType.hidden", () -> L.defineHiddenClass(classBytes(Hid.class), true).findVarHandle(Hid.class, "nothing", int.class));
        rowx("LambdaMetafactory.altMarkers", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.altMetafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class),
                    MethodType.methodType(int.class, int.class), INC, ii, java.lang.invoke.LambdaMetafactory.FLAG_MARKERS, 1, java.util.RandomAccess.class).getTarget();
            Object o = mh.invoke();
            return ((IntUnaryOperator) o).applyAsInt(1) + " " + (o instanceof java.util.RandomAccess);
        });
        rowx("LambdaMetafactory.altBridges", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.altMetafactory(L, "compare", MethodType.methodType(Cmp.class),
                    MethodType.methodType(int.class, String.class, String.class), L.findStatic(C, "cmpLen", MethodType.methodType(int.class, String.class, String.class)),
                    MethodType.methodType(int.class, String.class, String.class), java.lang.invoke.LambdaMetafactory.FLAG_BRIDGES, 1,
                    MethodType.methodType(int.class, Object.class, Object.class)).getTarget();
            Cmp c = (Cmp) mh.invoke();
            return c.compare("aa", "b") + " " + ((java.util.Comparator) c).compare("a", "bb");
        });
        rowx("LambdaMetafactory.capture", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.metafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class, int.class),
                    ii, ADD, ii).getTarget();
            return ((IntUnaryOperator) mh.invoke(10)).applyAsInt(5);
        });
        rowx("LambdaMetafactory.instance", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.metafactory(L, "apply", MethodType.methodType(java.util.function.Function.class, Sbr3MhRows.class),
                    MethodType.methodType(Object.class, Object.class), L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class)),
                    MethodType.methodType(String.class, String.class)).getTarget();
            return ((java.util.function.Function<String, String>) mh.invoke(new Sbr3MhRows())).apply("w");
        });
        rowx("LambdaMetafactory.nullLookup", () -> java.lang.invoke.LambdaMetafactory.metafactory(null, "applyAsInt", MethodType.methodType(IntUnaryOperator.class), ii, INC, ii));
        rowx("LambdaMetafactory.badLookup", () -> java.lang.invoke.LambdaMetafactory.metafactory(MethodHandles.publicLookup(), "applyAsInt", MethodType.methodType(IntUnaryOperator.class), ii, INC, ii));
        rowx("LambdaMetafactory.wrongType", () -> java.lang.invoke.LambdaMetafactory.metafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class), ii, INC, MethodType.methodType(long.class, long.class)));
        rowx("defineHiddenClass.options", () -> {
            Lookup h = L.defineHiddenClass(classBytes(Hid.class), false, Lookup.ClassOption.NESTMATE, Lookup.ClassOption.STRONG);
            return h.lookupClass().isHidden() + " " + h.hasFullPrivilegeAccess();
        });
        rowx("defineHiddenClass.noPackage", () -> MethodHandles.publicLookup().defineHiddenClass(classBytes(Hid.class), true));
        rowx("defineHiddenClass.dup", () -> { L.defineHiddenClass(classBytes(Hid.class), true); L.defineHiddenClass(classBytes(Hid.class), true); return "ok"; });
        rowx("defineHiddenClass.otherPkg", () -> L.defineHiddenClass(java.nio.file.Files.readAllBytes(java.nio.file.Path.of(System.getProperty("java.home"), "nothing.class")), true));
        rowx("defineHiddenClass.getName", () -> L.defineHiddenClass(classBytes(Hid.class), true).lookupClass().getName().replaceAll("/.*", "/N"));
        rowx("defineHiddenClass.init", () -> L.defineHiddenClass(classBytes(Hid.class), true).lookupClass().getSimpleName().replaceAll("/.*", "/N"));
        rowx("defineClass.dup", () -> L.defineClass(classBytes(Nested.class)));
        rowx("defineClass.samepkg", () -> { java.lang.invoke.MethodHandles.Lookup l = L; return l.defineClass(classBytes(DefMe.class)).getName(); });
        rowx("defineClass.getpd", () -> L.defineClass(classBytes(DefMe2.class)).getProtectionDomain() == C.getProtectionDomain());
        rowx("foldArguments.combinerBad", () -> MethodHandles.foldArguments(ADD, NEG));
        rowx("foldArguments.null", () -> MethodHandles.foldArguments(ADD, null));
        rowx("foldArguments.nullTarget", () -> MethodHandles.foldArguments(null, INC));
        rowx("foldArguments.voidChain", () -> MethodHandles.foldArguments(MethodHandles.foldArguments(INC, NOTHING), NOTHING).invoke(2));
        rowx("foldArguments.twoArgs", () -> MethodHandles.foldArguments(ADD3, MethodHandles.dropArguments(INC, 1, int.class)).invoke(1, 2));
        rowx("foldArguments.refs", () -> MethodHandles.foldArguments(CAT3, CAT).type());

        // ---- fourth block: positive rows for the loop / setter / length / empty / classData siblings
        final MethodHandle LT4 = L.findStatic(C, "lt4", MethodType.methodType(boolean.class, int.class));
        rowx("whileLoop.basic", () -> MethodHandles.whileLoop(MethodHandles.constant(int.class, 0), LT4, INC).invoke());
        rowx("whileLoop.type", () -> MethodHandles.whileLoop(MethodHandles.constant(int.class, 0), LT4, INC).type());
        rowx("doWhileLoop.basic", () -> MethodHandles.doWhileLoop(MethodHandles.constant(int.class, 0), INC, LT4).invoke());
        rowx("iteratedLoop.basic", () -> MethodHandles.iteratedLoop(null, MethodHandles.constant(int.class, 0),
                L.findStatic(C, "sumIter", MethodType.methodType(int.class, int.class, Object.class))).invoke(List.of(1, 2, 3)));
        rowx("countedLoop.four", () -> MethodHandles.countedLoop(MethodHandles.constant(int.class, 0), MethodHandles.constant(int.class, 3),
                MethodHandles.constant(int.class, 0), L.findStatic(C, "body", MethodType.methodType(int.class, int.class, int.class))).invoke());
        rowx("arrayElementSetter.bindTo", () -> MethodHandles.arrayElementSetter(int[].class).bindTo(new int[2]).type());
        rowx("arrayElementSetter.insert", () -> { int[] a = new int[2]; MethodHandles.insertArguments(MethodHandles.arrayElementSetter(int[].class), 0, a).invoke(1, 9); return a; });
        rowx("arrayElementSetter.asType", () -> { int[] a = new int[2]; MethodHandles.arrayElementSetter(int[].class).asType(MethodType.methodType(void.class, int[].class, int.class, Integer.class)).invoke(a, 1, (Integer) 4); return a; });
        rowx("arrayElementSetter.drop", () -> { int[] a = new int[2]; MethodHandles.dropArguments(MethodHandles.arrayElementSetter(int[].class), 0, String.class).invoke("x", a, 0, 5); return a; });
        rowx("arrayElementSetter.filter", () -> { int[] a = new int[2]; MethodHandles.filterArguments(MethodHandles.arrayElementSetter(int[].class), 2, NEG).invoke(a, 0, 5); return a; });
        rowx("arrayLength.bindTo", () -> MethodHandles.arrayLength(int[].class).bindTo(new int[4]).invoke());
        rowx("arrayLength.insert", () -> MethodHandles.insertArguments(MethodHandles.arrayLength(int[].class), 0, new int[5]).invoke());
        rowx("arrayLength.filter", () -> MethodHandles.filterReturnValue(MethodHandles.arrayLength(int[].class), NEG).invoke(new int[6]));
        rowx("arrayLength.asType", () -> MethodHandles.arrayLength(int[].class).asType(MethodType.methodType(Object.class, int[].class)).invoke(new int[7]));
        rowx("arrayLength.reveal", () -> L.revealDirect(MethodHandles.arrayLength(int[].class)));
        rowx("empty.bindTo", () -> MethodHandles.empty(MethodType.methodType(int.class, String.class)).bindTo("x").invoke());
        rowx("empty.insert", () -> MethodHandles.insertArguments(MethodHandles.empty(MethodType.methodType(String.class, int.class, int.class)), 0, 3).invoke(4));
        rowx("empty.asType", () -> MethodHandles.empty(MethodType.methodType(int.class, int.class)).asType(MethodType.methodType(Object.class, Integer.class)).invoke((Integer) 3));
        rowx("defineHiddenClassWithClassData.get", () -> { Lookup h = L.defineHiddenClassWithClassData(classBytes(Hid.class), "cd", true);
            return MethodHandles.classData(h, "_", String.class) + " " + h.lookupClass().isHidden(); });
        rowx("defineHiddenClassWithClassData.null", () -> L.defineHiddenClassWithClassData(classBytes(Hid.class), null, true));
        rowx("defineHiddenClassWithClassData.bad", () -> L.defineHiddenClassWithClassData(new byte[] {1, 2, 3}, "cd", true));
        rowx("defineHiddenClassWithClassData.opts", () -> L.defineHiddenClassWithClassData(classBytes(Hid.class), "cd", false, Lookup.ClassOption.NESTMATE).hasFullPrivilegeAccess());

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }

    interface Cmp extends java.util.Comparator<String> {
        int compare(String a, String b);
    }
}
