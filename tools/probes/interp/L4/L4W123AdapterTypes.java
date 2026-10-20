// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L4 (`CRATONVM_MH_LEAF_DESC`, default off;
// proposal docs/internal/proposals/interpreter/i42-L4-proposal-native-adapters-carry-hotspots-type-20261006.md):
// the `type()` of every native combinator adapter in the Groovy- and
// JRuby-shaped chains the page names, and the `asType` /
// `explicitCastArguments` answers over them, including the ones the
// raw-descriptor escape used to accept where HotSpot refuses (rows `under-*`).
//
// Groovy shapes: a `boolean` / `void` leaf retyped to `(Object,Object)Object`
// behind `guardWithTest` (Selector.setGuards), the guard's `type()` and
// parameter count, `MutableCallSite.setTarget` of it, and the call through
// `dynamicInvoker`. JRuby shapes: `insertArguments` / `dropArguments` /
// `filterReturnValue` (nil substitution) / `foldArguments` / `catchException`
// / `permuteArguments` / `SwitchPoint.guardWithTest` chains whose inputs are
// themselves adapters.
//
// Every row must be SAME in both modes, JIT and `--nojit`, with the switch on.
// With it off, the `under-*` rows are the known under-refusals (an answer
// instead of HotSpot's `WrongMethodTypeException`) in `--compatible`.
// `CRATONVM_DBG_MH_DISPATCH=1` with the switch on prints one `[MH_LEAF_DESC]`
// line per escape it declined.
//
// Run: javac -d . L4W123AdapterTypes.java
//      java -cp . L4W123AdapterTypes                                   (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W123AdapterTypes
//      CRATONVM_MH_LEAF_DESC=1 cratonvm --compatible [--nojit] -cp . L4W123AdapterTypes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; -Xint identical)
// g-ends-with type (String,String)boolean params 2
// g-ends-with-oo type (Object,Object)Object params 2
// g-guard type (Object,Object)Object params 2
// g-guard-call -> Boolean:true
// g-guard-fallback -> String:fallback
// g-site setTarget ok
// g-site-call -> Boolean:true
// g-guard-back asType (String,String)boolean ok (String,String)boolean
// g-guard-back asType-call -> Boolean:true
// g-guard-int asType (Object,Object)int ok (Object,Object)int
// g-void-guard type (Object)Object params 1
// g-void-guard-call -> null
// j-insert type (String)Object params 1
// j-drop type (Object,int,String)Object params 3
// j-filter-return type (Object,int,String)Object params 3
// j-filter-return-call -> String:self:x
// j-filter-args type (String,String)Object params 2
// j-filter-args-call -> String:a<b>
// j-fold type (String,String)Object params 2
// j-fold-call -> String:<p>|pq
// j-collect type (String,String)Object params 2
// j-catch type (Object,Object)Object params 2
// j-catch-call -> String:rescued boom x
// j-permute type (String,String)Object params 2
// j-permute-call -> String:ba
// j-switchpoint type (Object,Object)Object params 2
// j-switchpoint-call -> String:a<b>
// j-switchpoint-invalidated -> String:fallback
// j-bind type (String)Object params 1
// a-filter-args-obj asType (Object,Object)Object ok (Object,Object)Object
// a-filter-args-obj asType-call -> String:a<b>
// a-insert-int asType (int)Object java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String)Object to (int)Object
// a-drop-wrong-arity asType (Object,int)Object java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Object,int,String)Object to (Object,int)Object
// a-catch-long asType (Object,Object)long ok (Object,Object)long
// under-id-string type (String)Object params 1
// under-id-string-int asType (int)Object java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String)Object to (int)Object
// under-id-string-object asType (Object)Object ok (Object)Object
// under-id-string-object asType-call -> String:s
// under-id-integer-string asType (Object)String ok (Object)String
// under-id-integer-string asType-call threw java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
// under-guard-string type (String)Object params 1
// under-guard-string-int asType (int)Object java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String)Object to (int)Object
// under-filter-string-long asType (long)Object java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String)Object to (long)Object
// under-explicit-drop explicitCast (Object)Object java.lang.invoke.WrongMethodTypeException: cannot explicitly cast MethodHandle(Object,int,String)Object to (Object)Object
// under-explicit-insert explicitCast (String,String)Object java.lang.invoke.WrongMethodTypeException: cannot explicitly cast MethodHandle(String)Object to (String,String)Object
// under-explicit-ok explicitCast (String)int ok (String)int
// done
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.invoke.SwitchPoint;
import java.util.ArrayList;
import java.util.List;

public class L4W123AdapterTypes {

    static final MethodHandles.Lookup L = MethodHandles.lookup();
    static final Object NIL = "nil";

    static Object ident(Object o) {
        return o;
    }

    static Object nilIfNull(Object o) {
        return o == null ? NIL : o;
    }

    static boolean isString(Object a, Object b) {
        return a instanceof String;
    }

    static Object fallback(Object a, Object b) {
        return "fallback";
    }

    static Object concat(String a, String b) {
        return a + b;
    }

    static String prefix(String a) {
        return "<" + a + ">";
    }

    static Object fold3(String folded, String a, String b) {
        return folded + "|" + a + b;
    }

    static Object boom(Object a, Object b) {
        throw new IllegalStateException("boom " + a);
    }

    static Object rescue(IllegalStateException e, Object a, Object b) {
        return "rescued " + e.getMessage();
    }

    static Object nothing(Object a) {
        return null;
    }

    static String show(Object o) {
        if (o == null) {
            return "null";
        }
        return o.getClass().getSimpleName() + ":" + o;
    }

    static String err(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static void type(String row, MethodHandle mh) {
        System.out.println(row + " type " + mh.type() + " params " + mh.type().parameterCount());
    }

    static void call(String row, MethodHandle mh, Object... args) {
        try {
            System.out.println(row + " -> " + show(mh.invokeWithArguments(args)));
        } catch (Throwable t) {
            System.out.println(row + " threw " + err(t));
        }
    }

    static void asType(String row, MethodHandle mh, MethodType to, Object... args) {
        MethodHandle a;
        try {
            a = mh.asType(to);
        } catch (Throwable t) {
            System.out.println(row + " asType " + to + " " + err(t));
            return;
        }
        System.out.println(row + " asType " + to + " ok " + a.type());
        if (args.length == to.parameterCount()) {
            call(row + " asType-call", a, args);
        }
    }

    static void explicitCast(String row, MethodHandle mh, MethodType to) {
        try {
            MethodHandle a = MethodHandles.explicitCastArguments(mh, to);
            System.out.println(row + " explicitCast " + to + " ok " + a.type());
        } catch (Throwable t) {
            System.out.println(row + " explicitCast " + to + " " + err(t));
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodType oo = MethodType.methodType(Object.class, Object.class, Object.class);

        // --- Groovy: a boolean leaf retyped to (Object,Object)Object behind a guard.
        MethodHandle endsWith = L.findVirtual(String.class, "endsWith", MethodType.methodType(boolean.class, String.class));
        MethodHandle endsWithOO = endsWith.asType(oo);
        type("g-ends-with", endsWith);
        type("g-ends-with-oo", endsWithOO);
        MethodHandle test = L.findStatic(L4W123AdapterTypes.class, "isString", MethodType.methodType(boolean.class, Object.class, Object.class));
        MethodHandle fb = L.findStatic(L4W123AdapterTypes.class, "fallback", oo);
        MethodHandle guard = MethodHandles.guardWithTest(test, endsWithOO, fb);
        type("g-guard", guard);
        call("g-guard-call", guard, "1.0-SNAPSHOT", "-SNAPSHOT");
        call("g-guard-fallback", guard, 5, "x");
        MutableCallSite site = new MutableCallSite(oo);
        try {
            site.setTarget(guard);
            System.out.println("g-site setTarget ok");
            call("g-site-call", site.dynamicInvoker(), "abc", "c");
        } catch (Throwable t) {
            System.out.println("g-site setTarget " + err(t));
        }
        asType("g-guard-back", guard, MethodType.methodType(boolean.class, String.class, String.class), "ab", "b");
        asType("g-guard-int", guard, MethodType.methodType(int.class, Object.class, Object.class));
        // A void leaf retyped to (Object)Object behind a guard (Groovy's void method).
        MethodHandle clear = L.findVirtual(List.class, "clear", MethodType.methodType(void.class));
        MethodHandle clearO = clear.asType(MethodType.methodType(Object.class, Object.class));
        MethodHandle clearGuard = MethodHandles.guardWithTest(
                MethodHandles.dropArguments(MethodHandles.constant(boolean.class, true), 0, Object.class),
                clearO,
                L.findStatic(L4W123AdapterTypes.class, "nothing", MethodType.methodType(Object.class, Object.class)));
        type("g-void-guard", clearGuard);
        call("g-void-guard-call", clearGuard, new ArrayList<>(List.of(1, 2)));

        // --- JRuby: adapters over adapters.
        MethodHandle concat = L.findStatic(L4W123AdapterTypes.class, "concat", MethodType.methodType(Object.class, String.class, String.class));
        MethodHandle nil = L.findStatic(L4W123AdapterTypes.class, "nilIfNull", MethodType.methodType(Object.class, Object.class));
        MethodHandle ins = MethodHandles.insertArguments(concat, 0, "self:");
        type("j-insert", ins);
        MethodHandle drop = MethodHandles.dropArguments(ins, 0, Object.class, int.class);
        type("j-drop", drop);
        MethodHandle filt = MethodHandles.filterReturnValue(drop, nil);
        type("j-filter-return", filt);
        call("j-filter-return-call", filt, "ctx", 1, "x");
        MethodHandle pre = L.findStatic(L4W123AdapterTypes.class, "prefix", MethodType.methodType(String.class, String.class));
        MethodHandle filtArgs = MethodHandles.filterArguments(concat, 1, pre);
        type("j-filter-args", filtArgs);
        call("j-filter-args-call", filtArgs, "a", "b");
        MethodHandle fold3 = L.findStatic(L4W123AdapterTypes.class, "fold3", MethodType.methodType(Object.class, String.class, String.class, String.class));
        MethodHandle fold = MethodHandles.foldArguments(fold3, MethodHandles.filterReturnValue(pre, MethodHandles.identity(String.class)));
        type("j-fold", fold);
        call("j-fold-call", fold, "p", "q");
        MethodHandle collect = MethodHandles.collectArguments(concat, 0, pre);
        type("j-collect", collect);
        MethodHandle boom = L.findStatic(L4W123AdapterTypes.class, "boom", oo);
        MethodHandle rescue = L.findStatic(L4W123AdapterTypes.class, "rescue",
                MethodType.methodType(Object.class, IllegalStateException.class, Object.class, Object.class));
        MethodHandle caught = MethodHandles.catchException(boom, IllegalStateException.class, rescue);
        type("j-catch", caught);
        call("j-catch-call", caught, "x", "y");
        MethodHandle perm = MethodHandles.permuteArguments(concat, MethodType.methodType(Object.class, String.class, String.class), 1, 0);
        type("j-permute", perm);
        call("j-permute-call", perm, "a", "b");
        SwitchPoint sp = new SwitchPoint();
        MethodHandle spGuard = sp.guardWithTest(filtArgs.asType(oo), fb);
        type("j-switchpoint", spGuard);
        call("j-switchpoint-call", spGuard, "a", "b");
        SwitchPoint.invalidateAll(new SwitchPoint[] {sp});
        call("j-switchpoint-invalidated", spGuard, "a", "b");
        MethodHandle bound = concat.bindTo("b:");
        type("j-bind", bound);

        // --- asType over adapters: legal widenings and HotSpot's refusals.
        asType("a-filter-args-obj", filtArgs, oo, "a", "b");
        asType("a-insert-int", ins, MethodType.methodType(Object.class, int.class));
        asType("a-drop-wrong-arity", drop, MethodType.methodType(Object.class, Object.class, int.class));
        asType("a-catch-long", caught, MethodType.methodType(long.class, Object.class, Object.class));

        // --- under-refusals: the handle's `type` refuses, its leaf would not.
        MethodHandle id = L.findStatic(L4W123AdapterTypes.class, "ident", MethodType.methodType(Object.class, Object.class));
        MethodHandle idS = id.asType(MethodType.methodType(Object.class, String.class));
        type("under-id-string", idS);
        asType("under-id-string-int", idS, MethodType.methodType(Object.class, int.class), 5);
        asType("under-id-string-object", idS, MethodType.methodType(Object.class, Object.class), "s");
        MethodHandle idSI = id.asType(MethodType.methodType(Integer.class, Object.class));
        asType("under-id-integer-string", idSI, MethodType.methodType(String.class, Object.class), 7);
        MethodHandle guardS = MethodHandles.guardWithTest(
                MethodHandles.dropArguments(MethodHandles.constant(boolean.class, true), 0, String.class),
                idS,
                MethodHandles.dropArguments(MethodHandles.constant(Object.class, "fb"), 0, String.class));
        type("under-guard-string", guardS);
        asType("under-guard-string-int", guardS, MethodType.methodType(Object.class, int.class), 5);
        MethodHandle filtS = MethodHandles.filterReturnValue(idS, nil);
        asType("under-filter-string-long", filtS, MethodType.methodType(Object.class, long.class), 5L);
        explicitCast("under-explicit-drop", drop, MethodType.methodType(Object.class, Object.class));
        explicitCast("under-explicit-insert", ins, MethodType.methodType(Object.class, String.class, String.class));
        explicitCast("under-explicit-ok", idS, MethodType.methodType(int.class, String.class));
        System.out.println("done");
    }
}
