// `MethodHandle.invoke` and `bindTo` both end in a CAST, and this VM did not
// perform it.
//
//   cat = findVirtual(String, "concat", (String)String)
//   cat.invoke("ab", (Object) Integer.valueOf(3))
//     HotSpot   ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//     CratonVM  NoSuchMethodError: 'boolean java.lang.Integer.isEmpty()'
//
// The Integer reached the callee and the callee's own body dispatched on it.
// `NoSuchMethodError` extends `Error`, so a `catch (ClassCastException)` -- or
// any `catch (RuntimeException)` around a reflective dispatch -- does not see
// it, and it names a method the caller never wrote.
//
// This probe exists in two halves, and the second half is the point.
//
//   * THE DEFECT rows assert the cast fires.
//   * THE HOT-PATH rows assert it does NOT fire on the shapes that must keep
//     working: interfaces, subclasses, nulls, varargs collectors, lambdas
//     bound to functional interfaces. A cast check on this surface is one
//     `is_subclass` away from refusing correct Groovy-indy / SpEL /
//     log4j-provider calls, and a refusal of working code is worse than the
//     wrong answer it replaces. W7-19 5.1 declined the check for exactly that
//     reason; these rows are what makes taking it on defensible.
//   * THE LIMIT rows are the cases the predicate deliberately does NOT judge.
//     They are expected to DIFFER, and they are here so the gap is measured
//     rather than believed absent.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.List;
import java.util.function.Function;

public class InvokeCastSweep {
    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body b) {
        String v;
        try { v = String.valueOf(b.run()); }
        catch (Throwable e) { v = "throws " + e.getClass().getName(); }
        System.out.println(tag + " = " + v);
    }

    public interface Named { String name(); }
    public static class Animal implements Named {
        public String name() { return "animal"; }
        public String describe(Named other) { return name() + "+" + other.name(); }
    }
    public static class Dog extends Animal {
        @Override public String name() { return "dog"; }
    }

    static MethodHandles.Lookup L = MethodHandles.lookup();

    public static void main(String[] a) throws Throwable {
        theDefect();
        theHotPath();
        theLimit();
        System.out.println("DONE");
    }

    // ---- 1. the defect: a wrong reference must be a ClassCastException ------

    static void theDefect() throws Throwable {
        MethodHandle cat = L.findVirtual(String.class, "concat",
            MethodType.methodType(String.class, String.class));
        MethodHandle len = L.findVirtual(String.class, "length",
            MethodType.methodType(int.class));
        MethodHandle up = L.findStatic(InvokeCastSweep.class, "shout",
            MethodType.methodType(String.class, String.class));

        t("d.paramWrong", () -> (String) cat.invoke("ab", (Object) Integer.valueOf(3)));
        // The RECEIVER is a separate slot and was uncast too.
        t("d.receiverWrong", () -> (int) len.invoke((Object) Integer.valueOf(3)));
        t("d.staticParamWrong", () -> (String) up.invoke((Object) Integer.valueOf(3)));
        t("d.bindToWrong", () -> len.bindTo(Integer.valueOf(1)).type().toString());
        t("d.bindToStaticParamWrong", () -> up.bindTo(Integer.valueOf(1)).type().toString());
        // Two wrong arguments: the FIRST mismatch is the one reported.
        t("d.twoWrong", () -> (String) cat.invoke((Object) Integer.valueOf(1),
            (Object) Integer.valueOf(2)));
        t("d.invokeWithArguments", () -> cat.invokeWithArguments("ab", Integer.valueOf(3)));
    }

    static String shout(String s) { return s.toUpperCase(); }

    // ---- 2. the hot path: none of these may be refused ----------------------

    static void theHotPath() throws Throwable {
        MethodHandle cat = L.findVirtual(String.class, "concat",
            MethodType.methodType(String.class, String.class));
        MethodHandle len = L.findVirtual(String.class, "length",
            MethodType.methodType(int.class));
        MethodHandle name = L.findVirtual(Animal.class, "name",
            MethodType.methodType(String.class));
        MethodHandle describe = L.findVirtual(Animal.class, "describe",
            MethodType.methodType(String.class, Named.class));

        t("h.exact", () -> (String) cat.invoke("ab", "cd"));
        t("h.nullParam", () -> (String) cat.invoke("ab", (Object) null));
        t("h.nullReceiver", () -> (int) len.invoke((Object) null));
        // A SUBCLASS receiver and a subclass argument: the commonest correct
        // shape, and the one a naive identity check would refuse.
        t("h.subclassReceiver", () -> (String) name.invoke(new Dog()));
        t("h.subclassArg", () -> (String) describe.invoke(new Animal(), new Dog()));
        // An INTERFACE-typed parameter. `is_subclass` is a hierarchy walk and
        // answers false here for anything whose relationship is declared rather
        // than inherited, so this is where a false ClassCastException lives.
        t("h.interfaceArg", () -> (String) describe.invoke(new Animal(), new Animal()));
        t("h.interfaceArgLambda", () -> {
            Named n = () -> "lambda";
            return (String) describe.invoke(new Animal(), n);
        });
        // A lambda bound to a functional interface -- the Groovy-indy /
        // SpEL-FunctionReference shape the deferral named.
        t("h.bindLambdaToInterface", () -> {
            MethodHandle apply = L.findVirtual(Function.class, "apply",
                MethodType.methodType(Object.class, Object.class));
            Function<String, String> f = x -> x + "!";
            return apply.bindTo(f).invoke("hi");
        });
        t("h.bindSubclass", () -> {
            MethodHandle nm = L.findVirtual(Animal.class, "name",
                MethodType.methodType(String.class));
            return (String) nm.bindTo(new Dog()).invoke();
        });
        t("h.bindNull", () -> len.bindTo(null).type().toString());
        // Object-typed parameters accept anything, and must not be judged.
        t("h.objectParam", () -> {
            MethodHandle eq = L.findVirtual(Object.class, "equals",
                MethodType.methodType(boolean.class, Object.class));
            return (boolean) eq.invoke("a", (Object) Integer.valueOf(1));
        });
        // A varargs collector presents a different arity at the call site than
        // its descriptor carries; guessing that alignment is how a cast check
        // starts refusing correct calls.
        t("h.varargsCollector", () -> {
            MethodHandle of = L.findStatic(List.class, "of",
                MethodType.methodType(List.class, Object[].class))
                .asVarargsCollector(Object[].class);
            return of.invoke("a", "b", "c").toString();
        });
        t("h.asTypeThenInvoke", () -> {
            MethodHandle h = cat.asType(
                MethodType.methodType(Object.class, Object.class, Object.class));
            return h.invoke("ab", "cd");
        });
        t("h.boxedPrimitiveArg", () -> {
            MethodHandle max = L.findStatic(Math.class, "max",
                MethodType.methodType(int.class, int.class, int.class));
            return (int) max.invoke(Integer.valueOf(1), Integer.valueOf(2));
        });
        // ADAPTERS keep the LEAF member's descriptor while presenting a
        // different parameter list, and `filterArguments` presents the SAME
        // ARITY -- so an arity guard cannot see it and a cast check reading the
        // leaf descriptor would refuse this correct call.
        t("h.filterArguments", () -> {
            MethodHandle intToString = L.findStatic(Integer.class, "toString",
                MethodType.methodType(String.class, int.class));
            MethodHandle filtered = MethodHandles.filterArguments(cat, 1, intToString);
            return filtered.invoke("ab", 3);
        });
        t("h.insertArguments", () -> {
            return MethodHandles.insertArguments(cat, 1, "cd").invoke("ab");
        });
        t("h.dropArguments", () -> {
            MethodHandle dropped = MethodHandles.dropArguments(cat, 1, Integer.class);
            return dropped.invoke("ab", Integer.valueOf(9), "cd");
        });
        t("h.foldArguments", () -> {
            MethodHandle shoutH = L.findStatic(InvokeCastSweep.class, "shout",
                MethodType.methodType(String.class, String.class));
            return MethodHandles.filterReturnValue(cat, shoutH).invoke("ab", "cd");
        });
        // A SECOND bindTo mints an insert adapter over the first; its
        // descriptor is the leaf's and its arity is not.
        t("h.doubleBindTo", () -> {
            return cat.bindTo("ab").bindTo("cd").invoke();
        });
    }

    // ---- 3. the limit: measured, not believed absent -----------------------

    static void theLimit() throws Throwable {
        MethodHandle describe = L.findVirtual(Animal.class, "describe",
            MethodType.methodType(String.class, Named.class));
        MethodHandle arr = L.findStatic(Arrays.class, "toString",
            MethodType.methodType(String.class, int[].class));

        // An INTERFACE parameter given an object that does not implement it.
        // Deliberately not judged: the same clause that keeps `h.interfaceArg`
        // and `h.bindLambdaToInterface` working is what lets this through.
        t("x.interfaceArgWrong", () -> (String) describe.invoke(new Animal(),
            (Object) Integer.valueOf(3)));
        // An ARRAY parameter given a non-array. Array assignability is a
        // covariance rule of its own and is not asked here.
        t("x.arrayArgWrong", () -> (String) arr.invoke((Object) "not-an-array"));
    }
}
