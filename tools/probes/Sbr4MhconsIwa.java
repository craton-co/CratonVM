// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.Callable;

/**
 * SBR round 4, lane mhcons: HotSpot-diffed probe of MethodHandle.invokeWithArguments
 * (both overloads) over the handle shapes the real JDK builds, plus a positive control
 * (the Groovy-shaped CallSite over an adapted handle) that FAILS on the carrier world.
 * Every row prints "name -> result | ExceptionClass: message"; diff against HotSpot 25.
 */
public class Sbr4MhconsIwa {
    static int add(int a, int b) { return a + b; }
    static long wide(long a, double b) { return a + (long) b; }
    static void vd(String s) { sink = s; }
    static String sink;
    static String cat(String a, String b) { return a + b; }
    static String join(String sep, String... parts) { return String.join(sep, parts); }
    static Object boom(int x) throws Exception { throw new java.io.IOException("boom" + x); }
    static boolean lt10(int x) { return x < 10; }
    static int sumBody(int acc, int i) { return acc + i; }
    static int strBody(int acc, String s) { return acc + s.length(); }
    static int cleanup(Throwable t, int r, int x) { return r * 100 + (t == null ? 0 : 1); }
    static boolean pos(int x) { return x > 0; }
    static int neg(int x) { return -x; }
    static int inc(int x) { return x + 1; }
    int f = 7;
    final String name;
    Sbr4MhconsIwa(String n) { name = n; }
    String hello(String who) { return name + ":" + who; }

    interface Row { Object run() throws Throwable; }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof Object[] a ? Arrays.deepToString(a) : String.valueOf(v);
            if (v != null) out += " (" + v.getClass().getName() + ")";
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + " -> " + out);
    }

    public static void main(String[] args) throws Throwable {
        Lookup L = MethodHandles.lookup();
        Class<?> C = Sbr4MhconsIwa.class;
        MethodType ii_i = MethodType.methodType(int.class, int.class, int.class);
        MethodHandle ADD = L.findStatic(C, "add", ii_i);
        MethodHandle WIDE = L.findStatic(C, "wide", MethodType.methodType(long.class, long.class, double.class));
        MethodHandle VD = L.findStatic(C, "vd", MethodType.methodType(void.class, String.class));
        MethodHandle CAT = L.findStatic(C, "cat", MethodType.methodType(String.class, String.class, String.class));
        MethodHandle JOIN = L.findStatic(C, "join", MethodType.methodType(String.class, String.class, String[].class));
        MethodHandle BOOM = L.findStatic(C, "boom", MethodType.methodType(Object.class, int.class));
        MethodHandle POS = L.findStatic(C, "pos", MethodType.methodType(boolean.class, int.class));
        MethodHandle NEG = L.findStatic(C, "neg", MethodType.methodType(int.class, int.class));
        MethodHandle INC = L.findStatic(C, "inc", MethodType.methodType(int.class, int.class));
        MethodHandle CTOR = L.findConstructor(C, MethodType.methodType(void.class, String.class));
        MethodHandle HELLO = L.findVirtual(C, "hello", MethodType.methodType(String.class, String.class));
        MethodHandle GET = L.findGetter(C, "f", int.class);
        MethodHandle LEN = L.findVirtual(String.class, "length", MethodType.methodType(int.class));
        MethodHandle GUARD = MethodHandles.guardWithTest(POS, INC, NEG);
        MethodHandle INS = MethodHandles.insertArguments(ADD, 0, 5);
        MethodHandle FILT = MethodHandles.filterReturnValue(INC, NEG);
        MethodHandle BOUND = CAT.bindTo("pre-");
        MethodHandle SPREAD = ADD.asSpreader(int[].class, 2);
        MethodHandle ASLIST = L.findStatic(Arrays.class, "asList", MethodType.methodType(List.class, Object[].class));
        MethodHandle FMT = L.findStatic(String.class, "format", MethodType.methodType(String.class, String.class, Object[].class));

        row("add", () -> ADD.invokeWithArguments(1, 2));
        row("add.list", () -> ADD.invokeWithArguments(List.of(3, 4)));
        row("add.array", () -> ADD.invokeWithArguments(new Object[] {5, 6}));
        row("add.short", () -> ADD.invokeWithArguments((short) 5, (byte) 6));
        row("add.arity1", () -> ADD.invokeWithArguments(1));
        row("add.arity3", () -> ADD.invokeWithArguments(1, 2, 3));
        row("add.nullarg", () -> ADD.invokeWithArguments(null, 2));
        row("add.string", () -> ADD.invokeWithArguments("a", 2));
        row("add.long", () -> ADD.invokeWithArguments(1L, 2));
        row("add.nullarray", () -> ADD.invokeWithArguments((Object[]) null));
        row("add.nulllist", () -> ADD.invokeWithArguments((List<?>) null));
        row("wide", () -> WIDE.invokeWithArguments(10L, 2.5));
        row("wide.intlong", () -> WIDE.invokeWithArguments(10, 2.5f));
        row("void", () -> VD.invokeWithArguments("x"));
        row("void.sink", () -> sink);
        row("cat", () -> CAT.invokeWithArguments("a", "b"));
        row("cat.cce", () -> CAT.invokeWithArguments("a", 3));
        row("cat.null", () -> CAT.invokeWithArguments("a", null));
        row("join.varargs", () -> JOIN.invokeWithArguments("-", new String[] {"p", "q"}));
        row("join.collected", () -> JOIN.asVarargsCollector(String[].class).invokeWithArguments("-", "p", "q", "r"));
        row("join.collected0", () -> JOIN.asVarargsCollector(String[].class).invokeWithArguments("-"));
        row("boom", () -> BOOM.invokeWithArguments(3));
        row("ctor", () -> ((Sbr4MhconsIwa) CTOR.invokeWithArguments("n1")).name);
        row("ctor.arity", () -> CTOR.invokeWithArguments());
        row("hello", () -> HELLO.invokeWithArguments(new Sbr4MhconsIwa("me"), "you"));
        row("hello.nullrecv", () -> HELLO.invokeWithArguments(null, "you"));
        row("hello.badrecv", () -> HELLO.invokeWithArguments("str", "you"));
        row("getter", () -> GET.invokeWithArguments(new Sbr4MhconsIwa("g")));
        row("len", () -> LEN.invokeWithArguments("four"));
        row("guard.pos", () -> GUARD.invokeWithArguments(5));
        row("guard.neg", () -> GUARD.invokeWithArguments(-5));
        row("insert", () -> INS.invokeWithArguments(4));
        row("insert.arity", () -> INS.invokeWithArguments(4, 4));
        row("filter", () -> FILT.invokeWithArguments(9));
        row("bound", () -> BOUND.invokeWithArguments("x"));
        row("bound.arity", () -> BOUND.invokeWithArguments());
        row("spread", () -> SPREAD.invokeWithArguments((Object) new int[] {2, 3}));
        row("aslist", () -> ((List<?>) ASLIST.invokeWithArguments((Object) new Object[] {1, "b"})).size());
        row("fmt", () -> FMT.asVarargsCollector(Object[].class).invokeWithArguments("%s-%s", "a", 7));
        row("asType", () -> ADD.asType(MethodType.methodType(Object.class, Object.class, Object.class)).invokeWithArguments(8, 9));
        row("asType.cce", () -> ADD.asType(MethodType.methodType(Object.class, Object.class, Object.class)).invokeWithArguments("a", 9));
        row("asType.retcast", () -> ADD.asType(MethodType.methodType(String.class, int.class, int.class)).invokeWithArguments(1, 2));
        row("identity", () -> MethodHandles.identity(String.class).invokeWithArguments("id"));
        row("constant", () -> MethodHandles.constant(Integer.class, 42).invokeWithArguments());
        row("constant.list", () -> MethodHandles.constant(Integer.class, 42).invokeWithArguments(new ArrayList<Object>()));
        row("constant.extra", () -> MethodHandles.constant(Integer.class, 42).invokeWithArguments(1));
        row("dropargs", () -> MethodHandles.dropArguments(ADD, 0, String.class).invokeWithArguments("z", 1, 2));
        row("fold", () -> MethodHandles.foldArguments(ADD, INC).invokeWithArguments(10));
        row("explicit", () -> MethodHandles.explicitCastArguments(ADD, MethodType.methodType(long.class, long.class, long.class)).invokeWithArguments(1L, 2L));
        row("catch", () -> MethodHandles.catchException(BOOM, java.io.IOException.class,
                MethodHandles.dropArguments(MethodHandles.constant(Object.class, "caught"), 0, java.io.IOException.class, int.class))
                .invokeWithArguments(1));
        row("loop", () -> MethodHandles.countedLoop(MethodHandles.constant(int.class, 3), MethodHandles.constant(int.class, 0),
                MethodHandles.dropArguments(INC, 1, int.class)).invokeWithArguments());
        row("callable", () -> {
            Callable<String> c = () -> "called";
            return L.findVirtual(Callable.class, "call", MethodType.methodType(Object.class)).invokeWithArguments(c);
        });
        row("lambda", () -> {
            java.util.function.IntBinaryOperator op = (a, b) -> a * b;
            return L.findVirtual(java.util.function.IntBinaryOperator.class, "applyAsInt", ii_i).invokeWithArguments(op, 6, 7);
        });

        // Positive control (Groovy-shaped): a CallSite typed (int,int)Object over an adapted
        // handle; on the carrier world setTarget's type check over a carrier throws
        // WrongMethodTypeException, on real handles it runs (HotSpot's line).
        row("callsite.adapted", () -> {
            MethodHandle h = ADD.asType(MethodType.methodType(Object.class, int.class, int.class));
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(Object.class, int.class, int.class));
            cs.setTarget(h);
            return cs.dynamicInvoker().invoke(20, 22);
        });
        row("callsite.iwa", () -> {
            MutableCallSite cs = new MutableCallSite(ii_i);
            cs.setTarget(ADD);
            return cs.dynamicInvoker().invokeWithArguments(30, 12);
        });
        row("callsite.retarget", () -> {
            MutableCallSite cs = new MutableCallSite(ii_i);
            cs.setTarget(ADD);
            int a = (int) cs.dynamicInvoker().invokeExact(1, 1);
            cs.setTarget(MethodHandles.dropArguments(NEG, 1, int.class));
            int b = (int) cs.dynamicInvoker().invokeExact(1, 1);
            return a + "/" + b;
        });
        // More Groovy/Nashorn-shaped call-site targets: each is a combinator over a retired
        // `Lookup.find*` handle, installed on a call site of the adapted type and invoked.
        MethodType oo_o = MethodType.methodType(Object.class, Object.class, Object.class);
        MethodType ii_o = MethodType.methodType(Object.class, int.class, int.class);
        row("cs.guard", () -> {
            MethodHandle g = MethodHandles.guardWithTest(
                    MethodHandles.dropArguments(POS, 1, int.class),
                    ADD, MethodHandles.dropArguments(NEG, 1, int.class).asType(ii_i)).asType(ii_o);
            MutableCallSite cs = new MutableCallSite(ii_o);
            cs.setTarget(g);
            return cs.dynamicInvoker().invoke(3, 4) + "/" + cs.dynamicInvoker().invoke(-3, 4);
        });
        row("cs.permute", () -> {
            MethodHandle p = MethodHandles.permuteArguments(CAT, MethodType.methodType(String.class, String.class, String.class), 1, 0)
                    .asType(oo_o);
            MutableCallSite cs = new MutableCallSite(oo_o);
            cs.setTarget(p);
            return cs.dynamicInvoker().invoke("a", "b");
        });
        row("cs.collector", () -> {
            MethodHandle c = ASLIST.asCollector(Object[].class, 2).asType(MethodType.methodType(Object.class, Object.class, Object.class));
            MutableCallSite cs = new MutableCallSite(oo_o);
            cs.setTarget(c);
            return cs.dynamicInvoker().invoke("a", 1);
        });
        row("cs.explicit", () -> {
            MethodHandle e = MethodHandles.explicitCastArguments(ADD, MethodType.methodType(Object.class, Object.class, Object.class));
            MutableCallSite cs = new MutableCallSite(oo_o);
            cs.setTarget(e);
            return cs.dynamicInvoker().invoke(2, 5);
        });
        row("cs.wrongtype", () -> {
            MutableCallSite cs = new MutableCallSite(ii_o);
            cs.setTarget(ADD);
            return "set";
        });
        row("cs.volatile", () -> {
            java.lang.invoke.VolatileCallSite cs = new java.lang.invoke.VolatileCallSite(ii_i);
            cs.setTarget(ADD);
            return (int) cs.dynamicInvoker().invokeExact(2, 3);
        });
        row("cs.const", () -> new java.lang.invoke.ConstantCallSite(INC).dynamicInvoker().invokeWithArguments(41));
        // Construction-heavy combinators the family retirement hands to the JDK's own bytecode.
        row("zero.int", () -> (int) MethodHandles.zero(int.class).invokeExact());
        row("zero.string", () -> (String) MethodHandles.zero(String.class).invokeExact());
        row("zero.void", () -> MethodHandles.zero(void.class).type());
        row("empty", () -> MethodHandles.empty(MethodType.methodType(String.class, int.class, long.class)).invokeWithArguments(1, 2L));
        row("whileLoop", () -> {
            MethodHandle init = MethodHandles.constant(int.class, 0);
            MethodHandle pred = L.findStatic(Sbr4MhconsIwa.class, "lt10", MethodType.methodType(boolean.class, int.class));
            MethodHandle body = INC;
            return MethodHandles.whileLoop(init, pred, body).invokeWithArguments();
        });
        row("doWhileLoop", () -> {
            MethodHandle init = MethodHandles.constant(int.class, 0);
            MethodHandle pred = L.findStatic(Sbr4MhconsIwa.class, "lt10", MethodType.methodType(boolean.class, int.class));
            return MethodHandles.doWhileLoop(init, INC, pred).invokeWithArguments();
        });
        row("countedLoop.sum", () -> {
            MethodHandle body = L.findStatic(Sbr4MhconsIwa.class, "sumBody", MethodType.methodType(int.class, int.class, int.class));
            return MethodHandles.countedLoop(MethodHandles.constant(int.class, 5), MethodHandles.constant(int.class, 0), body).invokeWithArguments();
        });
        row("iteratedLoop", () -> {
            MethodHandle body = L.findStatic(Sbr4MhconsIwa.class, "strBody", MethodType.methodType(int.class, int.class, String.class));
            MethodHandle iter = MethodHandles.iteratedLoop(null, MethodHandles.constant(int.class, 0), body);
            return iter.invokeWithArguments(List.of("a", "bb", "ccc"));
        });
        row("tableSwitch", () -> {
            MethodHandle sw = MethodHandles.tableSwitch(MethodHandles.dropArguments(MethodHandles.constant(String.class, "dflt"), 0, int.class),
                    MethodHandles.dropArguments(MethodHandles.constant(String.class, "zero"), 0, int.class),
                    MethodHandles.dropArguments(MethodHandles.constant(String.class, "one"), 0, int.class));
            return sw.invokeWithArguments(0) + "/" + sw.invokeWithArguments(1) + "/" + sw.invokeWithArguments(9);
        });
        row("tryFinally", () -> {
            MethodHandle cleanup = L.findStatic(Sbr4MhconsIwa.class, "cleanup", MethodType.methodType(int.class, Throwable.class, int.class, int.class));
            return MethodHandles.tryFinally(INC, cleanup).invokeWithArguments(1);
        });
        row("filterArguments", () -> MethodHandles.filterArguments(ADD, 0, INC, NEG).invokeWithArguments(1, 2));
        row("foldArguments.pos", () -> MethodHandles.foldArguments(ADD, 1, MethodHandles.dropArguments(INC, 1, int.class)).type());
        row("collectArguments", () -> MethodHandles.collectArguments(ADD, 0, INC).invokeWithArguments(5, 6));
        row("dropReturn", () -> MethodHandles.dropReturn(ADD).invokeWithArguments(1, 2));
        row("arrayConstructor", () -> ((int[]) MethodHandles.arrayConstructor(int[].class).invokeWithArguments(3)).length);
        row("arrayLength", () -> MethodHandles.arrayLength(int[].class).invokeWithArguments((Object) new int[4]));
        row("arrayGetter", () -> MethodHandles.arrayElementGetter(String[].class).invokeWithArguments(new String[] {"x", "y"}, 1));
        row("arraySetter", () -> { int[] a = new int[2]; MethodHandles.arrayElementSetter(int[].class).invokeWithArguments(a, 1, 9); return a[1]; });
        row("exactInvoker", () -> MethodHandles.exactInvoker(MethodType.methodType(int.class, int.class, int.class)).invokeWithArguments(ADD, 3, 4));
        row("invoker", () -> MethodHandles.invoker(ii_i).invokeWithArguments(ADD, 3, 5));
        row("spreadInvoker", () -> MethodHandles.spreadInvoker(ii_i, 0).invokeWithArguments(ADD, new Object[] {6, 7}));
        row("varHandleInvoker", () -> {
            java.lang.invoke.VarHandle vh = L.findVarHandle(Sbr4MhconsIwa.class, "f", int.class);
            MethodHandle inv = MethodHandles.varHandleInvoker(java.lang.invoke.VarHandle.AccessMode.GET, MethodType.methodType(int.class, Sbr4MhconsIwa.class));
            return inv.invokeWithArguments(vh, new Sbr4MhconsIwa("v"));
        });
        row("revealDirect", () -> { java.lang.invoke.MethodHandleInfo i = L.revealDirect(ADD); return i.getName() + i.getMethodType() + i.getReferenceKind(); });
        row("unreflect", () -> L.unreflect(C.getDeclaredMethod("add", int.class, int.class)).invokeWithArguments(2, 2));
        row("unreflectCtor", () -> ((Sbr4MhconsIwa) L.unreflectConstructor(C.getDeclaredConstructor(String.class)).invokeWithArguments("u")).name);
        row("findSpecial", () -> { try { return L.findSpecial(Object.class, "hashCode", MethodType.methodType(int.class), Object.class).type(); } catch (IllegalAccessException e) { return "IAE"; } });
        row("findClass", () -> L.findClass("java.lang.String").getName());
        row("findClass.missing", () -> L.findClass("no.Such").getName());
        row("findStatic.missing", () -> L.findStatic(C, "nope", ii_i));
        row("findVirtual.badtype", () -> L.findVirtual(String.class, "length", MethodType.methodType(long.class)));
        row("lookup.in", () -> L.in(String.class).lookupClass().getName() + "/" + (L.in(String.class).lookupModes() & Lookup.PUBLIC));
        row("privateLookupIn", () -> MethodHandles.privateLookupIn(C, L).lookupClass().getName());
        row("bindTo.cce", () -> HELLO.bindTo("notRecv"));
        row("bindTo.prim", () -> ADD.bindTo(1).type());
        row("asVarargs", () -> JOIN.asVarargsCollector(String[].class).isVarargsCollector() + "/" + JOIN.isVarargsCollector());
        row("asFixedArity", () -> JOIN.asVarargsCollector(String[].class).asFixedArity().isVarargsCollector());
        row("withVarargs", () -> JOIN.withVarargs(true).isVarargsCollector());
        row("viewAsType", () -> ADD.asType(MethodType.methodType(long.class, int.class, int.class)).type());
        row("describe", () -> ADD.toString() + "|" + ADD.type() + "|" + ADD.describeConstable().isPresent());
        row("hiddenClass", () -> {
            byte[] bytes;
            try (java.io.InputStream in = C.getResourceAsStream("Sbr4MhconsIwaHid.class")) { bytes = in.readAllBytes(); }
            Lookup h = L.defineHiddenClass(bytes, true);
            return h.lookupClass().isHidden() + "/" + h.lookupClass().getSimpleName().startsWith("Sbr4MhconsIwaHid");
        });
        // hot loop: the result must not drift across the JIT thresholds
        row("hot", () -> {
            long s = 0;
            for (int i = 0; i < 20000; i++) s += (Integer) ADD.invokeWithArguments(i, 1);
            return s;
        });
        row("hot.mixed", () -> {
            long s = 0;
            for (int i = 0; i < 5000; i++) {
                s += (Integer) GUARD.invokeWithArguments(i - 2500);
                s += (Long) WIDE.invokeWithArguments((long) i, 0.5);
                s += ((String) CAT.invokeWithArguments("a", "b")).length();
            }
            return s;
        });
    }
}
class Sbr4MhconsIwaHid { }
