// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L4: the two forced natives
// `docs/internal/fixed-bugs/interpreter-L4-callsite-settarget-and-asfixedarity-diverge-from-hotspot-FIXED-20260927.md`
// names, against HotSpot. `MutableCallSite` / `VolatileCallSite` /
// `CallSite.setTarget` must refuse a target whose type differs from the call
// site's (`CallSite.checkTargetChange`: `WrongMethodTypeException:
// MethodHandle<new> should be of type <old>`), and `asFixedArity()` /
// `asVarargsCollector()` are pure, returning the receiver itself exactly when
// it already has the requested arity kind.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W24CallSiteAndArity
//
// HotSpot 25 (25.0.3) prints:
//   mutable setTarget wrong type: java.lang.invoke.WrongMethodTypeException: MethodHandle()String should be of type ()int
//   mutable setTarget wrong arity: java.lang.invoke.WrongMethodTypeException: MethodHandle(int)int should be of type ()int
//   mutable setTarget same type: 7
//   mutable setTarget null: java.lang.NullPointerException: Cannot invoke "java.lang.invoke.MethodHandle.type()" because "newTarget" is null
//   volatile setTarget wrong type: java.lang.invoke.WrongMethodTypeException: MethodHandle()long should be of type ()int
//   volatile setTarget same type: 3
//   mutable getTarget after refusal: 7
//   varargs isVarargsCollector: true
//   fixed of fixed is same: true
//   fixed of varargs is new: true
//   fixed of varargs flag: false
//   receiver keeps varargs: true
//   varargs of varargs same array is same: true
//   varargs of varargs Object[] is new: java.lang.IllegalArgumentException: array type not assignable to argument: MethodHandle(String[])String, class [Ljava.lang.Object;
//   varargs of fixed is new: true
//   fixed keeps fixed: false
//   varargs collects: a|b
//   fixed does not collect: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(String[])String to (String,String)String
//
// Before wave 24 (read from the code, not run): the four setTarget refusal
// rows printed `accepted` (any target, null included, was stored), and
// `fixed of fixed is same` / `varargs of varargs same array is same` printed
// `false` (a copy was minted). `fixed does not collect` depends on
// `mh_dispatch`'s collector marking and was not examined.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.invoke.VolatileCallSite;

public class L4W24CallSiteAndArity {
    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            Object r = c.run();
            out = r instanceof MethodHandle mh ? "handle " + mh.type() : String.valueOf(r);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static int seven() { return 7; }
    static String cat(String... xs) { return String.join("|", xs); }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle seven = l.findStatic(L4W24CallSiteAndArity.class, "seven", MethodType.methodType(int.class));
        MethodHandle cat = l.findStatic(L4W24CallSiteAndArity.class, "cat",
                MethodType.methodType(String.class, String[].class));

        // setTarget
        row("mutable setTarget wrong type", () -> {
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(int.class));
            cs.setTarget(MethodHandles.constant(String.class, "x"));
            return "accepted";
        });
        row("mutable setTarget wrong arity", () -> {
            MutableCallSite cs = new MutableCallSite(seven);
            cs.setTarget(MethodHandles.identity(int.class));
            return "accepted";
        });
        row("mutable setTarget same type", () -> {
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(int.class));
            cs.setTarget(seven);
            return (int) cs.dynamicInvoker().invokeExact();
        });
        row("mutable setTarget null", () -> {
            MutableCallSite cs = new MutableCallSite(seven);
            cs.setTarget(null);
            return "accepted";
        });
        row("volatile setTarget wrong type", () -> {
            VolatileCallSite cs = new VolatileCallSite(seven);
            cs.setTarget(MethodHandles.constant(long.class, 1L));
            return "accepted";
        });
        row("volatile setTarget same type", () -> {
            VolatileCallSite cs = new VolatileCallSite(MethodType.methodType(int.class));
            cs.setTarget(MethodHandles.constant(int.class, 3));
            return (int) cs.getTarget().invokeExact();
        });
        row("mutable getTarget after refusal", () -> {
            MutableCallSite cs = new MutableCallSite(seven);
            try { cs.setTarget(MethodHandles.constant(String.class, "x")); } catch (RuntimeException e) { }
            return (int) cs.getTarget().invokeExact();
        });

        // asFixedArity / asVarargsCollector identity and purity
        row("varargs isVarargsCollector", () -> cat.isVarargsCollector());
        row("fixed of fixed is same", () -> seven.asFixedArity() == seven);
        row("fixed of varargs is new", () -> cat.asFixedArity() != cat);
        row("fixed of varargs flag", () -> cat.asFixedArity().isVarargsCollector());
        row("receiver keeps varargs", () -> { cat.asFixedArity(); return cat.isVarargsCollector(); });
        row("varargs of varargs same array is same", () -> cat.asVarargsCollector(String[].class) == cat);
        row("varargs of varargs Object[] is new", () -> cat.asVarargsCollector(Object[].class) != cat);
        row("varargs of fixed is new", () -> {
            MethodHandle f = cat.asFixedArity();
            return f.asVarargsCollector(String[].class) != f;
        });
        row("fixed keeps fixed", () -> {
            MethodHandle f = cat.asFixedArity();
            f.asVarargsCollector(String[].class);
            return f.isVarargsCollector();
        });
        row("varargs collects", () -> (String) cat.invoke("a", "b"));
        row("fixed does not collect", () -> (String) cat.asFixedArity().invoke("a", "b"));
    }
}
