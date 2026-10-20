// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L4 (review): the `MethodHandles`
// combinators CratonVM serves natively (`filterArguments`,
// `filterReturnValue`, `foldArguments`, `collectArguments`,
// `guardWithTest`, `catchException`, `permuteArguments`) against HotSpot's
// argument checks: a valid use (its type and one invocation), a `null`
// argument (the exception's class only: HotSpot's helpful NPE text comes from
// JDK bytecode the natives do not run), and a type mismatch (the
// `IllegalArgumentException`'s message).
//
// Run: javac -d out L4W42CombinatorChecks.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W42CombinatorChecks
//
// The type checks are made natively only for DIRECT handles (`findStatic` /
// `findVirtual`); the `chain-*` rows pass adapters and must link.
// `--compatible` is not recorded: the checks are `--jdk-only` (it answered a
// null handle, the bare target, or an adapter over refused types).
//
// Positive control: the rows themselves (before wave 42 `filterArgs-null-target`
// printed `null`, `filterArgs-too-many` a handle).
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   filterArgs-ok: abb
//   filterArgs-null-target: java.lang.NullPointerException
//   filterArgs-null-array: java.lang.NullPointerException
//   filterArgs-null-element: handle (String,String)String
//   filterArgs-too-many: java.lang.IllegalArgumentException: too many filters
//   filterArgs-mismatch: java.lang.IllegalArgumentException: target and filter types do not match: (String,String)String, (String)int
//   filterArgs-negative: java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 2
//   filterReturn-ok: 4
//   filterReturn-null-target: java.lang.NullPointerException
//   filterReturn-null-filter: java.lang.NullPointerException
//   filterReturn-mismatch: java.lang.IllegalArgumentException: target and filter types do not match: (String)int, (String)String
//   filterReturn-arity: java.lang.IllegalArgumentException: target and filter types do not match: (String)String, (String,String)String
//   fold-ok: xxx
//   fold-null-target: java.lang.NullPointerException
//   fold-null-combiner: java.lang.NullPointerException
//   fold-mismatch: java.lang.IllegalArgumentException: target and combiner types must match: (String,String)String != (String)int
//   collect-ok: abb
//   collect-null-target: java.lang.NullPointerException
//   collect-null-filter: java.lang.NullPointerException
//   collect-mismatch: java.lang.IllegalArgumentException: target and filter types do not match: (String,String)String, (String)int
//   collect-pos: java.lang.IllegalArgumentException: position is out of range for target: MethodHandle(String,String)String, 5
//   guard-ok: 
//   guard-null-test: java.lang.NullPointerException
//   guard-null-target: java.lang.NullPointerException
//   guard-null-fallback: java.lang.NullPointerException
//   guard-test-type: java.lang.IllegalArgumentException: guard type is not a predicate (String)int
//   guard-branch-types: java.lang.IllegalArgumentException: target and fallback types must match: (String)String != (String)int
//   catch-ok: caught e
//   catch-null-target: java.lang.NullPointerException
//   catch-null-type: java.lang.NullPointerException
//   catch-null-handler: java.lang.NullPointerException
//   catch-handler-return: java.lang.IllegalArgumentException: target and handler return types must match: (String)String != (IllegalStateException,String)int
//   catch-handler-type: java.lang.IllegalArgumentException: handler does not accept exception type class java.lang.IllegalStateException
//   catch-handler-params: java.lang.IllegalArgumentException: target and handler types must match: (String)String != (IllegalStateException,String,String)String
//   catch-not-throwable: java.lang.ClassCastException: java.lang.String
//   chain-ok: 4
//   chain-fold-ok: xxxxx
//   permute-ok: ba
//   permute-null-target: java.lang.NullPointerException
//   permute-null-type: java.lang.NullPointerException
//   permute-null-reorder: java.lang.NullPointerException
//   permute-length: java.lang.IllegalArgumentException: old type parameter count and reorder array length do not match: (String,String)String, [0]
//   permute-index: java.lang.IllegalArgumentException: index is out of bounds for new type: 1, (String)String
//   permute-return: java.lang.IllegalArgumentException: return types do not match: (String,String)String, (String)Object
//   permute-param: java.lang.IllegalArgumentException: parameter types do not match after reorder: (String,String)String, (Object)String
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W42CombinatorChecks {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof MethodHandle h ? "handle " + h.type() : String.valueOf(v);
        } catch (NullPointerException e) {
            out = "java.lang.NullPointerException";
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static final MethodHandles.Lookup L = MethodHandles.lookup();

    public static String concat(String a, String b) {
        return a + b;
    }

    public static int len(String s) {
        return s.length();
    }

    public static String twice(String s) {
        return s + s;
    }

    public static boolean isEmpty(String s) {
        return s.isEmpty();
    }

    public static String boom(String s) {
        throw new IllegalStateException(s);
    }

    public static String caught(IllegalStateException e, String s) {
        return "caught " + e.getMessage();
    }

    public static int lenIse(IllegalStateException e, String s) {
        return -1;
    }

    public static String handleNfe(NumberFormatException e, String s) {
        return "nfe";
    }

    public static String caughtWide(IllegalStateException e, String s, String extra) {
        return "wide";
    }

    static MethodHandle mh(String name, Class<?> r, Class<?>... p) throws Exception {
        return L.findStatic(L4W42CombinatorChecks.class, name, MethodType.methodType(r, p));
    }

    public static void main(String[] args) throws Exception {
        MethodHandle concat = mh("concat", String.class, String.class, String.class);
        MethodHandle len = mh("len", int.class, String.class);
        MethodHandle twice = mh("twice", String.class, String.class);
        MethodHandle isEmpty = mh("isEmpty", boolean.class, String.class);
        MethodHandle boom = mh("boom", String.class, String.class);
        MethodHandle caught = mh("caught", String.class, IllegalStateException.class, String.class);

        // filterArguments
        row("filterArgs-ok", () -> (String) MethodHandles.filterArguments(concat, 1, twice).invokeExact("a", "b"));
        row("filterArgs-null-target", () -> MethodHandles.filterArguments(null, 0, twice));
        row("filterArgs-null-array", () -> MethodHandles.filterArguments(concat, 0, (MethodHandle[]) null));
        row("filterArgs-null-element", () -> MethodHandles.filterArguments(concat, 0, (MethodHandle) null));
        row("filterArgs-too-many", () -> MethodHandles.filterArguments(concat, 1, twice, twice));
        row("filterArgs-mismatch", () -> MethodHandles.filterArguments(concat, 0, len));
        row("filterArgs-negative", () -> MethodHandles.filterArguments(concat, -1, twice));

        // filterReturnValue
        row("filterReturn-ok", () -> (int) MethodHandles.filterReturnValue(twice, len).invokeExact("ab"));
        row("filterReturn-null-target", () -> MethodHandles.filterReturnValue(null, len));
        row("filterReturn-null-filter", () -> MethodHandles.filterReturnValue(twice, null));
        row("filterReturn-mismatch", () -> MethodHandles.filterReturnValue(len, twice));
        row("filterReturn-arity", () -> MethodHandles.filterReturnValue(twice, concat));

        // foldArguments
        row("fold-ok", () -> (String) MethodHandles.foldArguments(concat, twice).invokeExact("x"));
        row("fold-null-target", () -> MethodHandles.foldArguments(null, twice));
        row("fold-null-combiner", () -> MethodHandles.foldArguments(concat, null));
        row("fold-mismatch", () -> MethodHandles.foldArguments(concat, len));

        // collectArguments
        row("collect-ok", () -> (String) MethodHandles.collectArguments(concat, 1, twice).invokeExact("a", "b"));
        row("collect-null-target", () -> MethodHandles.collectArguments(null, 0, twice));
        row("collect-null-filter", () -> MethodHandles.collectArguments(concat, 0, null));
        row("collect-mismatch", () -> MethodHandles.collectArguments(concat, 0, len));
        row("collect-pos", () -> MethodHandles.collectArguments(concat, 5, twice));

        // guardWithTest
        row("guard-ok", () -> (String) MethodHandles.guardWithTest(isEmpty, twice, boom).invokeExact(""));
        row("guard-null-test", () -> MethodHandles.guardWithTest(null, twice, twice));
        row("guard-null-target", () -> MethodHandles.guardWithTest(isEmpty, null, twice));
        row("guard-null-fallback", () -> MethodHandles.guardWithTest(isEmpty, twice, null));
        row("guard-test-type", () -> MethodHandles.guardWithTest(len, twice, twice));
        row("guard-branch-types", () -> MethodHandles.guardWithTest(isEmpty, twice, len));

        // catchException
        row("catch-ok", () -> (String) MethodHandles.catchException(boom, IllegalStateException.class, caught)
                .invokeExact("e"));
        row("catch-null-target", () -> MethodHandles.catchException(null, IllegalStateException.class, caught));
        row("catch-null-type", () -> MethodHandles.catchException(boom, null, caught));
        row("catch-null-handler", () -> MethodHandles.catchException(boom, IllegalStateException.class, null));
        row("catch-handler-return", () -> MethodHandles.catchException(boom, IllegalStateException.class,
                mh("lenIse", int.class, IllegalStateException.class, String.class)));
        row("catch-handler-type", () -> MethodHandles.catchException(boom, IllegalStateException.class,
                mh("handleNfe", String.class, NumberFormatException.class, String.class)));
        row("catch-handler-params", () -> MethodHandles.catchException(boom, IllegalStateException.class,
                mh("caughtWide", String.class, IllegalStateException.class, String.class, String.class)));
        row("catch-not-throwable", () -> {
            @SuppressWarnings({"unchecked", "rawtypes"})
            Class<? extends Throwable> notThrowable = (Class) String.class;
            return MethodHandles.catchException(boom, notThrowable, caught);
        });

        // an adapter as an argument (not type-checked natively; must link)
        row("chain-ok", () -> (int) MethodHandles.filterReturnValue(
                MethodHandles.guardWithTest(isEmpty, twice, twice), len).invokeExact("ab"));
        row("chain-fold-ok", () -> (String) MethodHandles.foldArguments(
                MethodHandles.filterArguments(concat, 0, twice), twice).invokeExact("x"));

        // permuteArguments
        row("permute-ok", () -> (String) MethodHandles.permuteArguments(concat,
                MethodType.methodType(String.class, String.class, String.class), 1, 0).invokeExact("a", "b"));
        row("permute-null-target", () -> MethodHandles.permuteArguments(null,
                MethodType.methodType(String.class, String.class), 0, 0));
        row("permute-null-type", () -> MethodHandles.permuteArguments(concat, null, 0, 0));
        row("permute-null-reorder", () -> MethodHandles.permuteArguments(concat,
                MethodType.methodType(String.class, String.class), (int[]) null));
        row("permute-length", () -> MethodHandles.permuteArguments(concat,
                MethodType.methodType(String.class, String.class), 0));
        row("permute-index", () -> MethodHandles.permuteArguments(concat,
                MethodType.methodType(String.class, String.class), 0, 1));
        row("permute-return", () -> MethodHandles.permuteArguments(concat,
                MethodType.methodType(Object.class, String.class), 0, 0));
        row("permute-param", () -> MethodHandles.permuteArguments(concat,
                MethodType.methodType(String.class, Object.class), 0, 0));
    }
}
