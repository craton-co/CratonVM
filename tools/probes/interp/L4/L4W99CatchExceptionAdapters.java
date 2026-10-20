// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 99, lane L4 -- page
// `docs/internal/fixed-bugs/interpreter-i98-L4-catch-exception-refusal-texts-FIXED-20261015.md`
// ("OPEN residue").
//
// JDK 25 `MethodHandles.catchException(target, exType, handler)` judges both
// handles by their `type()`, whatever kind of handle they are:
//   htype.returnType() != ttype.returnType()
//       -> misMatchedTypes("target and handler return types", ttype, htype)
//   dropArgumentsToMatch(handler, 1, ttype.ptypes(), 0, true) == null
//       -> misMatchedTypes("target and handler types", ttype, htype)
// (`misMatchedTypes`: `what + " must match: " + t1 + " != " + t2`, the
// `MethodType.toString()` spelling with simple names). `--compatible`'s native
// ran these two checks only when both handles were plain direct handles; a
// `bindTo` result or an `asType` / `insertArguments` / `dropArguments` /
// `filterReturnValue` adapter built an adapter HotSpot refuses. The `ok-*`
// rows are shapes that must still be accepted, and run.
//
// Run: javac L4W99CatchExceptionAdapters.java && java L4W99CatchExceptionAdapters
// No setup.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the JDK sources)
// ret-mismatch-bound-target: java.lang.IllegalArgumentException: target and handler return types must match: (String)String != (RuntimeException,String)Object
// ret-mismatch-astype-handler: java.lang.IllegalArgumentException: target and handler return types must match: (String)String != (RuntimeException,String)Object
// ret-mismatch-astype-target: java.lang.IllegalArgumentException: target and handler return types must match: (String)Object != (RuntimeException,String)String
// ret-mismatch-filter-return-target: java.lang.IllegalArgumentException: target and handler return types must match: (String)int != (RuntimeException,String)String
// lead-mismatch-bound-target: java.lang.IllegalArgumentException: target and handler types must match: (String)String != (RuntimeException,int)String
// lead-mismatch-insert-target: java.lang.IllegalArgumentException: target and handler types must match: (String)String != (RuntimeException,int)String
// lead-mismatch-drop-target: java.lang.IllegalArgumentException: target and handler types must match: (int,String)String != (RuntimeException,String)String
// more-params-drop-handler: java.lang.IllegalArgumentException: target and handler types must match: (String)String != (RuntimeException,String,int)String
// more-params-bound-target: java.lang.IllegalArgumentException: target and handler types must match: (String)String != (RuntimeException,String,String)String
// not-accept-astype-handler: java.lang.IllegalArgumentException: handler does not accept exception type class java.lang.RuntimeException
// ok-bound-target: h:bi:x/x
// ok-insert-handler: m:b:x/x/E
// ok-fewer-handler-params: h:b:x/fixed
// ok-astype-handler-throwable: h:b:x/x
// ok-astype-both-object: h:b:x/x
// ok-no-throw: fine:y
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W99CatchExceptionAdapters {
    static String boom(String s) {
        if (s.equals("y")) {
            return "fine:" + s;
        }
        throw new IllegalStateException("b:" + s);
    }

    String boomI(String s) {
        throw new IllegalStateException("bi:" + s);
    }

    static String boom2(String s, int n) {
        throw new IllegalStateException("b2:" + s + n);
    }

    static String onRe(RuntimeException e, String s) {
        return "h:" + e.getMessage() + "/" + s;
    }

    static Object onReObj(RuntimeException e, String s) {
        return "obj";
    }

    static String onReInt(RuntimeException e, int n) {
        return "int";
    }

    static String onReMore(RuntimeException e, String s, String extra) {
        return "m:" + e.getMessage() + "/" + s + "/" + extra;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W99CatchExceptionAdapters.class;
        Class<RuntimeException> re = RuntimeException.class;
        MethodType ss = MethodType.methodType(String.class, String.class);
        MethodHandle boom = l.findStatic(me, "boom", ss);
        MethodHandle bound = l.findVirtual(me, "boomI", ss).bindTo(new L4W99CatchExceptionAdapters());
        MethodHandle boom2 = l.findStatic(me, "boom2",
                MethodType.methodType(String.class, String.class, int.class));
        MethodHandle onRe = l.findStatic(me, "onRe",
                MethodType.methodType(String.class, RuntimeException.class, String.class));
        MethodHandle onReObj = l.findStatic(me, "onReObj",
                MethodType.methodType(Object.class, RuntimeException.class, String.class));
        MethodHandle onReInt = l.findStatic(me, "onReInt",
                MethodType.methodType(String.class, RuntimeException.class, int.class));
        MethodHandle onReMore = l.findStatic(me, "onReMore",
                MethodType.methodType(String.class, RuntimeException.class, String.class, String.class));
        MethodHandle length = l.findVirtual(String.class, "length", MethodType.methodType(int.class));

        // Return-type mismatches.
        row("ret-mismatch-bound-target", () -> MethodHandles.catchException(bound, re, onReObj));
        row("ret-mismatch-astype-handler", () -> MethodHandles.catchException(boom, re,
                onRe.asType(MethodType.methodType(Object.class, RuntimeException.class, String.class))));
        row("ret-mismatch-astype-target", () -> MethodHandles.catchException(
                boom.asType(MethodType.methodType(Object.class, String.class)), re, onRe));
        row("ret-mismatch-filter-return-target", () -> MethodHandles.catchException(
                MethodHandles.filterReturnValue(boom, length), re, onRe));

        // Leading-parameter mismatches.
        row("lead-mismatch-bound-target", () -> MethodHandles.catchException(bound, re, onReInt));
        row("lead-mismatch-insert-target", () -> MethodHandles.catchException(
                MethodHandles.insertArguments(boom2, 1, 5), re, onReInt));
        row("lead-mismatch-drop-target", () -> MethodHandles.catchException(
                MethodHandles.dropArguments(boom, 0, int.class), re, onRe));

        // Handler with more parameters (after the exception) than the target.
        row("more-params-drop-handler", () -> MethodHandles.catchException(boom, re,
                MethodHandles.dropArguments(onRe, 2, int.class)));
        row("more-params-bound-target", () -> MethodHandles.catchException(bound, re, onReMore));

        // The handler's first parameter, read from an adapter's type.
        row("not-accept-astype-handler", () -> MethodHandles.catchException(boom, re,
                onRe.asType(MethodType.methodType(String.class, IllegalArgumentException.class, String.class))));

        // Accepted shapes: built and run.
        row("ok-bound-target", () ->
                (String) MethodHandles.catchException(bound, re, onRe).invokeExact("x"));
        row("ok-insert-handler", () -> (String) MethodHandles.catchException(boom, re,
                MethodHandles.insertArguments(onReMore, 2, "E")).invokeExact("x"));
        row("ok-fewer-handler-params", () -> (String) MethodHandles.catchException(
                MethodHandles.dropArguments(boom, 0, int.class), re,
                MethodHandles.insertArguments(onRe, 1, "fixed")).invokeExact(7, "x"));
        row("ok-astype-handler-throwable", () -> (String) MethodHandles.catchException(boom, re,
                onRe.asType(MethodType.methodType(String.class, Throwable.class, String.class)))
                .invokeExact("x"));
        row("ok-astype-both-object", () -> (Object) MethodHandles.catchException(
                boom.asType(MethodType.methodType(Object.class, String.class)), re,
                onRe.asType(MethodType.methodType(Object.class, RuntimeException.class, String.class)))
                .invokeExact("x"));
        row("ok-no-throw", () -> (String) MethodHandles.catchException(boom, re, onRe).invokeExact("y"));
    }
}
