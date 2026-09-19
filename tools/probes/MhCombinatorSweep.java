// The `MethodHandles` COMBINATORS, which no probe in this family has swept.
//
// `L8InvokeLookupSweep` covers the `Lookup` surface, `L5ModuleInvokeSweep`
// covers `invoke`/`invokeExact`, and `InvokeCastSweep` uses five adapters as
// GUARDS -- rows that assert a cast check does not fire on them. None of those
// asks whether the combinator itself is right.
//
// Why this surface specifically: the adapter guards established that this VM
// keeps the LEAF member's descriptor in `MH_DESC` while the adapter presents a
// different parameter list to its caller. Everything below is built out of that
// mechanism, so a wrong `type()`, a mis-ordered argument list, or an adapter
// that silently drops its wrapper is exactly the shape to expect.
//
// Two things every row does deliberately:
//
//   * asserts a VALUE or a TYPE STRING, never an implementation class. A
//     MethodHandle's own class is unspecified -- HotSpot answers
//     `BoundMethodHandle$Species_LLLL` -- so asserting it measures the JDK's
//     internals and not the combinator. (That mistake cost a row in
//     P1RemainingSweep.)
//   * pairs a behaviour row with a `type()` row wherever the combinator changes
//     arity, because a combinator can dispatch correctly and still report a
//     stale type -- which is what made SpEL's FunctionReference re-wrap its
//     args into a nested Object[] once already.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;

public class MhCombinatorSweep {
    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body b) {
        String v;
        try { v = String.valueOf(b.run()); }
        catch (Throwable e) {
            Throwable root = e;
            while (root.getCause() != null && root.getCause() != root) root = root.getCause();
            v = "throws " + e.getClass().getName()
              + (root == e ? "" : " <- " + root.getClass().getName());
        }
        System.out.println(tag + " = " + v);
    }

    static final MethodHandles.Lookup L = MethodHandles.lookup();

    // ---- the targets every combinator is built over ------------------------

    public static String cat3(String a, String b, String c) { return a + "|" + b + "|" + c; }
    public static String shout(String s) { return s.toUpperCase(); }
    public static int len(String s) { return s.length(); }
    public static boolean isLong(String s) { return s.length() > 3; }
    public static String yes(String s) { return "Y:" + s; }
    public static String no(String s) { return "N:" + s; }
    public static String boom(String s) { throw new IllegalStateException("boom:" + s); }
    public static String caught(IllegalStateException e, String s) { return "C:" + e.getMessage() + ":" + s; }
    public static void sideEffect(String s) { /* void, for tryFinally/void shapes */ }
    public static int add(int a, int b) { return a + b; }
    public static boolean under(int i, int limit) { return i < limit; }
    public static int step(int i, int limit) { return i + 1; }

    static MethodHandle mh(String name, Class<?> ret, Class<?>... params) throws Throwable {
        return L.findStatic(MhCombinatorSweep.class, name, MethodType.methodType(ret, params));
    }

    public static void main(String[] a) throws Throwable {
        reorder();
        collectAndSpread();
        guardsAndCatches();
        loops();
        casts();
        constantsAndIdentity();
        System.out.println("DONE");
    }

    // ---- permute / insert / drop: pure argument-list surgery ----------------

    static void reorder() throws Throwable {
        MethodHandle c3 = mh("cat3", String.class, String.class, String.class, String.class);

        t("p.permuteReverse", () -> {
            MethodHandle h = MethodHandles.permuteArguments(c3,
                MethodType.methodType(String.class, String.class, String.class, String.class),
                2, 1, 0);
            return h.invoke("a", "b", "c");
        });
        t("p.permuteReverse.type", () -> {
            MethodHandle h = MethodHandles.permuteArguments(c3,
                MethodType.methodType(String.class, String.class, String.class, String.class),
                2, 1, 0);
            return h.type().toString();
        });
        // A DUPLICATED index: one incoming argument feeds two parameters.
        t("p.permuteDuplicate", () -> {
            MethodHandle h = MethodHandles.permuteArguments(c3,
                MethodType.methodType(String.class, String.class, String.class),
                0, 1, 0);
            return h.invoke("x", "y");
        });
        t("p.permuteBadIndex", () -> MethodHandles.permuteArguments(c3,
            MethodType.methodType(String.class, String.class, String.class), 0, 1, 5));
        t("p.insertMiddle", () -> MethodHandles.insertArguments(c3, 1, "MID").invoke("a", "c"));
        t("p.insertMiddle.type", () -> MethodHandles.insertArguments(c3, 1, "MID").type().toString());
        t("p.insertTooMany", () -> MethodHandles.insertArguments(c3, 1, "a", "b", "c"));
        t("p.dropTwo", () -> {
            MethodHandle h = MethodHandles.dropArguments(c3, 1, Integer.class, Long.class);
            return h.invoke("a", Integer.valueOf(1), Long.valueOf(2L), "b", "c");
        });
        t("p.dropTwo.type", () -> MethodHandles.dropArguments(c3, 1, Integer.class, Long.class)
            .type().toString());
        t("p.filterReturn", () -> MethodHandles.filterReturnValue(c3,
            mh("shout", String.class, String.class)).invoke("a", "b", "c"));
        t("p.filterTwoArgs", () -> {
            MethodHandle up = mh("shout", String.class, String.class);
            return MethodHandles.filterArguments(c3, 0, up, up).invoke("a", "b", "c");
        });
        t("p.foldPrefix", () -> {
            // foldArguments prepends the combiner's RESULT as argument 0.
            MethodHandle combiner = mh("shout", String.class, String.class);
            MethodHandle folded = MethodHandles.foldArguments(c3, combiner);
            return folded.invoke("b", "c");
        });
        t("p.foldPrefix.type", () -> MethodHandles.foldArguments(c3,
            mh("shout", String.class, String.class)).type().toString());
    }

    // ---- collect / spread / varargs ---------------------------------------

    static void collectAndSpread() throws Throwable {
        MethodHandle c3 = mh("cat3", String.class, String.class, String.class, String.class);
        MethodHandle listOf = L.findStatic(List.class, "of",
            MethodType.methodType(List.class, Object[].class));

        t("c.asSpreader", () -> {
            MethodHandle sp = c3.asSpreader(String[].class, 3);
            return sp.invoke(new String[] { "a", "b", "c" });
        });
        t("c.asSpreader.type", () -> c3.asSpreader(String[].class, 3).type().toString());
        t("c.asSpreaderPartial", () -> {
            MethodHandle sp = c3.asSpreader(String[].class, 2);
            return sp.invoke("a", new String[] { "b", "c" });
        });
        t("c.asSpreaderWrongLen", () -> c3.asSpreader(String[].class, 3)
            .invoke(new String[] { "a", "b" }));
        t("c.asSpreaderNullArray", () -> c3.asSpreader(String[].class, 3)
            .invoke((String[]) null));
        t("c.asCollector", () -> {
            MethodHandle col = listOf.asCollector(Object[].class, 3);
            return col.invoke("a", "b", "c").toString();
        });
        t("c.asCollector.type", () -> listOf.asCollector(Object[].class, 3).type().toString());
        t("c.asVarargsIsVarargs", () -> listOf.asVarargsCollector(Object[].class)
            .isVarargsCollector());
        t("c.asFixedArity", () -> listOf.asVarargsCollector(Object[].class)
            .asFixedArity().isVarargsCollector());
        t("c.collectArguments", () -> {
            MethodHandle up = mh("shout", String.class, String.class);
            return MethodHandles.collectArguments(c3, 1, up).invoke("a", "b", "c");
        });
        t("c.asTypeWiden", () -> {
            MethodHandle h = mh("add", int.class, int.class, int.class);
            return (long) h.asType(MethodType.methodType(long.class, int.class, int.class))
                .invoke(2, 3);
        });
    }

    // ---- guardWithTest / catchException / tryFinally -----------------------

    static void guardsAndCatches() throws Throwable {
        MethodHandle test = mh("isLong", boolean.class, String.class);
        MethodHandle y = mh("yes", String.class, String.class);
        MethodHandle n = mh("no", String.class, String.class);
        MethodHandle bad = mh("boom", String.class, String.class);
        MethodHandle handler = mh("caught", String.class, IllegalStateException.class, String.class);

        t("g.guardTrue", () -> MethodHandles.guardWithTest(test, y, n).invoke("abcdef"));
        t("g.guardFalse", () -> MethodHandles.guardWithTest(test, y, n).invoke("ab"));
        t("g.guard.type", () -> MethodHandles.guardWithTest(test, y, n).type().toString());
        t("g.catchHandled", () -> MethodHandles
            .catchException(bad, IllegalStateException.class, handler).invoke("x"));
        t("g.catchWrongType", () -> MethodHandles
            .catchException(bad, ClassCastException.class,
                mh("caught", String.class, IllegalStateException.class, String.class)));
        t("g.catchNotThrown", () -> MethodHandles
            .catchException(y, IllegalStateException.class, handler).invoke("x"));
        t("g.tryFinallyNormal", () -> {
            MethodHandle cleanup = L.findStatic(MhCombinatorSweep.class, "cleanup",
                MethodType.methodType(String.class, Throwable.class, String.class, String.class));
            return MethodHandles.tryFinally(y, cleanup).invoke("x");
        });
        t("g.tryFinallyThrows", () -> {
            MethodHandle cleanup = L.findStatic(MhCombinatorSweep.class, "cleanup",
                MethodType.methodType(String.class, Throwable.class, String.class, String.class));
            return MethodHandles.tryFinally(bad, cleanup).invoke("x");
        });
    }

    public static String cleanup(Throwable t, String result, String arg) {
        return "F:" + (t == null ? "ok" : t.getClass().getSimpleName()) + ":" + result + ":" + arg;
    }

    // ---- the loop family ---------------------------------------------------

    static void loops() throws Throwable {
        t("l.countedLoop", () -> {
            MethodHandle body = L.findStatic(MhCombinatorSweep.class, "loopBody",
                MethodType.methodType(int.class, int.class, int.class));
            MethodHandle count = MethodHandles.constant(int.class, 5);
            MethodHandle init = MethodHandles.constant(int.class, 0);
            return MethodHandles.countedLoop(count, init, body).invoke();
        });
        t("l.whileLoop", () -> {
            MethodHandle init = MethodHandles.constant(int.class, 0);
            MethodHandle pred = L.findStatic(MhCombinatorSweep.class, "under10",
                MethodType.methodType(boolean.class, int.class));
            MethodHandle body = L.findStatic(MhCombinatorSweep.class, "plus3",
                MethodType.methodType(int.class, int.class));
            return MethodHandles.whileLoop(init, pred, body).invoke();
        });
        t("l.doWhileLoop", () -> {
            MethodHandle init = MethodHandles.constant(int.class, 20);
            MethodHandle pred = L.findStatic(MhCombinatorSweep.class, "under10",
                MethodType.methodType(boolean.class, int.class));
            MethodHandle body = L.findStatic(MhCombinatorSweep.class, "plus3",
                MethodType.methodType(int.class, int.class));
            // doWhileLoop runs the body BEFORE the first test, so a false
            // predicate still yields one iteration.
            return MethodHandles.doWhileLoop(init, body, pred).invoke();
        });
        t("l.iteratedLoop", () -> {
            MethodHandle iterator = null; // default: Iterable
            MethodHandle init = MethodHandles.constant(String.class, "");
            MethodHandle body = L.findStatic(MhCombinatorSweep.class, "appendItem",
                MethodType.methodType(String.class, String.class, String.class));
            return MethodHandles.iteratedLoop(iterator, init, body)
                .invoke(List.of("a", "b", "c"));
        });
    }

    public static int loopBody(int value, int i) { return value + i; }
    public static boolean under10(int i) { return i < 10; }
    public static int plus3(int i) { return i + 3; }
    public static String appendItem(String acc, String item) { return acc + item; }

    // ---- explicitCastArguments and the constant family ---------------------

    static void casts() throws Throwable {
        MethodHandle l = mh("len", int.class, String.class);

        t("x.explicitCastReturn", () -> {
            MethodHandle h = MethodHandles.explicitCastArguments(l,
                MethodType.methodType(byte.class, String.class));
            return (byte) h.invoke("abcdef");
        });
        // explicitCast permits NARROWING that asType refuses -- the pair is the
        // point: same conversion, two different contracts.
        t("x.asTypeNarrowRefuses", () -> l.asType(MethodType.methodType(byte.class, String.class))
            .invoke("abcdef"));
        t("x.explicitCastObjectArg", () -> {
            MethodHandle h = MethodHandles.explicitCastArguments(l,
                MethodType.methodType(int.class, Object.class));
            return h.invoke((Object) "abcd");
        });
        t("x.explicitCastWrongObj", () -> {
            MethodHandle h = MethodHandles.explicitCastArguments(l,
                MethodType.methodType(int.class, Object.class));
            return h.invoke((Object) Integer.valueOf(3));
        });
    }

    static void constantsAndIdentity() throws Throwable {
        t("k.constant", () -> MethodHandles.constant(String.class, "K").invoke());
        t("k.constant.type", () -> MethodHandles.constant(String.class, "K").type().toString());
        t("k.constantPrimitive", () -> (int) MethodHandles.constant(int.class, 7).invoke());
        t("k.constantNullRef", () -> MethodHandles.constant(String.class, null).invoke());
        t("k.constantNullPrimitive", () -> MethodHandles.constant(int.class, null));
        t("k.identity", () -> MethodHandles.identity(String.class).invoke("id"));
        t("k.identity.type", () -> MethodHandles.identity(String.class).type().toString());
        t("k.zeroRef", () -> MethodHandles.zero(String.class).invoke());
        t("k.zeroInt", () -> (int) MethodHandles.zero(int.class).invoke());
        t("k.emptyVoid", () -> {
            MethodHandles.empty(MethodType.methodType(void.class, String.class)).invoke("x");
            return "no-throw";
        });
        t("k.arrayElementGetter", () -> {
            MethodHandle g = MethodHandles.arrayElementGetter(String[].class);
            return g.invoke(new String[] { "p", "q" }, 1);
        });
        t("k.arrayElementSetter", () -> {
            String[] arr = new String[2];
            MethodHandles.arrayElementSetter(String[].class).invoke(arr, 0, "s");
            return arr[0];
        });
        t("k.arrayLength", () -> (int) MethodHandles.arrayLength(String[].class)
            .invoke(new String[3]));
        t("k.arrayGetterOOB", () -> MethodHandles.arrayElementGetter(String[].class)
            .invoke(new String[1], 5));
        t("k.arrayGetterNull", () -> MethodHandles.arrayElementGetter(String[].class)
            .invoke((String[]) null, 0));
    }
}
