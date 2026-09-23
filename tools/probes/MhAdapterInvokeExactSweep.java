// Does `MethodHandle.invokeExact` enforce its exact signature on an ADAPTER
// (a combinator's own return value), the same way it now enforces it on a
// direct STATIC/VIRTUAL/SPECIAL/CONSTRUCTOR/GETTER/SETTER handle?
//
// The retired L5-residuals write-up fixed the direct
// kinds and left the adapter kinds unchecked on purpose -- `kind_has_
// authoritative_type`'s doc comment: "type is maintained for them ... so the
// check would probably be correct there too. 'Probably' is not a measurement,
// and the cost of being wrong is a refusal of working code on Groovy's hot
// path." `MhCombinatorSweep.java` (56 rows, byte-identical to HotSpot in both
// modes, 2026-09-17) already proves the adapted `type()` VALUE is right for
// every combinator below -- but that probe calls `invoke`, which tolerates a
// call-site/declared mismatch by construction. This probe calls `invokeExact`
// instead, on the exact combinator shapes that probe already validated, so a
// correct row here is evidence the STRICT check is safe to turn on for that
// kind, not just that the combinator's arithmetic is right.
//
// Every "ok" row must match HotSpot's VALUE. Every "bad" row must match
// HotSpot's WrongMethodTypeException -- either an argument the call site
// spells with the wrong static type (`(Object) x` where the declared
// parameter is a narrower reference, which invokeExact must refuse: an
// erased-to-Object call site is never identical to a concrete-class one) or
// the wrong argument COUNT. Two negative-control rows at the end (tryFinally,
// a loop) assert the kinds this probe's fix deliberately leaves OUT still
// accept a mismatched call site -- if either throws, the fix's kind list grew
// wider than intended.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;

public class MhAdapterInvokeExactSweep {
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

    public static String cat3(String a, String b, String c) { return a + "|" + b + "|" + c; }
    public static String shout(String s) { return s.toUpperCase(); }
    public static boolean isLong(String s) { return s.length() > 3; }
    public static String yes(String s) { return "Y:" + s; }
    public static String no(String s) { return "N:" + s; }
    public static String boom(String s) { throw new IllegalStateException("boom:" + s); }
    public static String caught(IllegalStateException e, String s) { return "C:" + e.getMessage() + ":" + s; }
    public static String cleanup(Throwable t, String result, String arg) {
        return "F:" + (t == null ? "ok" : t.getClass().getSimpleName()) + ":" + result + ":" + arg;
    }
    public static boolean under10(int i) { return i < 10; }
    public static int plus3(int i) { return i + 3; }

    static MethodHandle mh(String name, Class<?> ret, Class<?>... params) throws Throwable {
        return L.findStatic(MhAdapterInvokeExactSweep.class, name, MethodType.methodType(ret, params));
    }

    public static void main(String[] a) throws Throwable {
        insertAndDrop();
        filtersAndFold();
        collectAndSpread();
        guardCatchPermute();
        negativeControls();
        System.out.println("DONE");
    }

    static void insertAndDrop() throws Throwable {
        MethodHandle c3 = mh("cat3", String.class, String.class, String.class, String.class);

        MethodHandle ins = MethodHandles.insertArguments(c3, 1, "MID");
        t("ins.ok", () -> (String) ins.invokeExact("a", "c"));
        t("ins.badArity", () -> (String) ins.invokeExact("a", "b", "c"));
        t("ins.badType", () -> (String) ins.invokeExact("a", (Object) "c"));

        MethodHandle drop = MethodHandles.dropArguments(c3, 1, Integer.class, Long.class);
        t("drop.ok", () -> (String) drop.invokeExact("a", Integer.valueOf(1), Long.valueOf(2L), "b", "c"));
        t("drop.badType", () -> (String) drop.invokeExact("a", Long.valueOf(2L), Integer.valueOf(1), "b", "c"));
    }

    static void filtersAndFold() throws Throwable {
        MethodHandle c3 = mh("cat3", String.class, String.class, String.class, String.class);
        MethodHandle up = mh("shout", String.class, String.class);

        MethodHandle fr = MethodHandles.filterReturnValue(c3, up);
        t("filterReturn.ok", () -> (String) fr.invokeExact("a", "b", "c"));
        t("filterReturn.badType", () -> (String) fr.invokeExact("a", "b", (Object) "c"));

        MethodHandle fa = MethodHandles.filterArguments(c3, 0, up, up);
        t("filterArgs.ok", () -> (String) fa.invokeExact("a", "b", "c"));
        t("filterArgs.badArity", () -> (String) fa.invokeExact("a", "b"));

        MethodHandle fold = MethodHandles.foldArguments(c3, up);
        t("fold.ok", () -> (String) fold.invokeExact("b", "c"));
        t("fold.badArity", () -> (String) fold.invokeExact("b", "c", "d"));

        MethodHandle ca = MethodHandles.collectArguments(c3, 1, up);
        t("collectArgs.ok", () -> (String) ca.invokeExact("a", "b", "c"));
        t("collectArgs.badArity", () -> (String) ca.invokeExact("a", "b"));
    }

    static void collectAndSpread() throws Throwable {
        MethodHandle c3 = mh("cat3", String.class, String.class, String.class, String.class);
        MethodHandle listOf = L.findStatic(List.class, "of", MethodType.methodType(List.class, Object[].class));

        MethodHandle spFull = c3.asSpreader(String[].class, 3);
        t("spread.ok", () -> (String) spFull.invokeExact(new String[] { "a", "b", "c" }));
        t("spread.badType", () -> (String) spFull.invokeExact((Object) "x"));

        MethodHandle spPartial = c3.asSpreader(String[].class, 2);
        t("spreadPartial.ok", () -> (String) spPartial.invokeExact("a", new String[] { "b", "c" }));
        t("spreadPartial.badArity",
            () -> (String) spPartial.invokeExact("a", new String[] { "b", "c" }, "extra"));

        MethodHandle col = listOf.asCollector(Object[].class, 3);
        t("collect.ok", () -> ((List<?>) col.invokeExact((Object) "a", (Object) "b", (Object) "c")).toString());
        t("collect.badArity", () -> col.invokeExact((Object) "a", (Object) "b"));
    }

    static void guardCatchPermute() throws Throwable {
        MethodHandle test = mh("isLong", boolean.class, String.class);
        MethodHandle y = mh("yes", String.class, String.class);
        MethodHandle n = mh("no", String.class, String.class);
        MethodHandle bad = mh("boom", String.class, String.class);
        MethodHandle handler = mh("caught", String.class, IllegalStateException.class, String.class);

        MethodHandle guard = MethodHandles.guardWithTest(test, y, n);
        t("guard.true", () -> (String) guard.invokeExact("abcdef"));
        t("guard.false", () -> (String) guard.invokeExact("ab"));
        t("guard.badType", () -> (String) guard.invokeExact((Object) "ab"));

        MethodHandle caughtMh = MethodHandles.catchException(bad, IllegalStateException.class, handler);
        t("catch.ok", () -> (String) caughtMh.invokeExact("x"));
        t("catch.badType", () -> (String) caughtMh.invokeExact((Object) "x"));

        MethodHandle c3 = mh("cat3", String.class, String.class, String.class, String.class);
        MethodHandle perm = MethodHandles.permuteArguments(c3,
            MethodType.methodType(String.class, String.class, String.class, String.class), 2, 1, 0);
        t("permute.ok", () -> (String) perm.invokeExact("a", "b", "c"));
        t("permute.badArity", () -> (String) perm.invokeExact("a", "b"));
    }

    // Kinds this fix deliberately leaves OUT of `kind_has_authoritative_
    // adapted_type` -- TRY_FINALLY and the LOOP_* family -- because
    // `MhCombinatorSweep` never exercised them with `invokeExact` and this
    // probe has not measured them either. HotSpot enforces `invokeExact`
    // strictly on EVERY handle regardless of kind, so a KNOWN, ACCEPTED
    // divergence from the oracle here is not itself surprising. Only the
    // FIRST row below is actually a regression guard on this fix, though: a
    // throw there would mean the fix's `matches!` grew a kind it was not
    // supposed to. The second row is not -- see its own comment.
    static void negativeControls() throws Throwable {
        MethodHandle y = mh("yes", String.class, String.class);
        MethodHandle cleanupMh = L.findStatic(MhAdapterInvokeExactSweep.class, "cleanup",
            MethodType.methodType(String.class, Throwable.class, String.class, String.class));
        MethodHandle tf = MethodHandles.tryFinally(y, cleanupMh);
        t("tryFinally.wrongTypeStillRuns[CratonVM-only]", () -> (String) tf.invokeExact((Object) "x"));

        // NOT a parallel control to the row above: measured, this ALREADY
        // throws WrongMethodTypeException, byte-identical to HotSpot, through
        // some mechanism this fix does not touch (LOOP_* is provably absent
        // from `kind_has_authoritative_adapted_type`'s `matches!` -- this row
        // cannot be reaching it). Kept as a documented FYI, not a regression
        // gate: unlike the row above, a throw here proves nothing about this
        // fix's own kind list.
        MethodHandle init = MethodHandles.constant(int.class, 0);
        MethodHandle pred = mh("under10", boolean.class, int.class);
        MethodHandle body = mh("plus3", int.class, int.class);
        MethodHandle loop = MethodHandles.whileLoop(init, pred, body);
        t("loop.wrongReturnType[not-a-control]", () -> (long) loop.invokeExact());
    }
}
