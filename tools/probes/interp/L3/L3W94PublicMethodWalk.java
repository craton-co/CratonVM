// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L3 -- the rows where the served
// `Class.getMethod` / `getMethods` walks can differ from JDK 25's
// (`docs/internal/fixed-bugs/interpreter-i52-L3-proposal-one-public-method-walk-for-getmethod-and-getmethods-KEPT-OFF-20261019.md`).
//
// JDK 25 `getMethod0`: `getMethodsRecursive` (the class's own matching public
// methods if any, else the superclass's answer merged with each direct
// superinterface's, statics only along the superclass chain) and
// `PublicMethods.MethodList.getMostSpecific` (the most specific return type).
// `getMethods`: `privateGetPublicMethods` with `PublicMethods.merge` (per
// (name, parameter types): of two methods with one return type a class's
// beats an interface's, a subtype's beats its supertype's; other return types
// stay beside them). An interface has no superclass there, so it has no
// `Object` method.
//
// Rows (each `getMethod` row prints `Method.toString()` or the throwable's
// class and message; each `getMethods` row the sorted `toString()`s of the
// public methods with that name, ` | `-joined):
//   sub-iface-second    C10 implements I1, I0 (I1 extends I0, both default m):
//                       getMethod("m") -- I1's;
//   sub-iface-first     C01 implements I0, I1: the same answer;
//   methods-sub-iface   C10.getMethods() named m: I1's only;
//   unrelated-ifaces    CAB implements A, B (each abstract n()): getMethod --
//                       the first interface's (same return type);
//   methods-unrelated   CAB.getMethods() named n: both;
//   covariant-bridge    Cov overrides Base's Object c() with String c():
//                       getMethod -- the String one;
//   methods-covariant   Cov.getMethods() named c: both (the bridge too);
//   class-beats-default SubD extends SupD implements D: SupD's d();
//   abstract-redeclare  CE implements E1, E0 (E1 re-declares E0's default
//                       e() abstract): E1's;
//   covariant-ifaces    CR implements R1, R0 (String r() / Object r()): R1's;
//   iface-object-method I0.getMethod("hashCode"): NoSuchMethodException;
//   runnable-tostring   Runnable.getMethod("toString"): NoSuchMethodException;
//   methods-iface-object I1.getMethods() named toString: none;
//   field-first-iface   CF implements F1, F2 (each a constant X): getField("X")
//                       -- F1's (`getField0`: superinterfaces in declaration
//                       order; the served walk answered F2's until wave 94);
//   sub-iface-second-again  the first row again.
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources of Class.getMethodsRecursive / PublicMethods; to be measured)
// sub-iface-second: public default java.lang.String L3W94PublicMethodWalk$I1.m()
// sub-iface-first: public default java.lang.String L3W94PublicMethodWalk$I1.m()
// methods-sub-iface: public default java.lang.String L3W94PublicMethodWalk$I1.m()
// unrelated-ifaces: public abstract void L3W94PublicMethodWalk$A.n()
// methods-unrelated: public abstract void L3W94PublicMethodWalk$A.n() | public abstract void L3W94PublicMethodWalk$B.n()
// covariant-bridge: public java.lang.String L3W94PublicMethodWalk$Cov.c()
// methods-covariant: public java.lang.Object L3W94PublicMethodWalk$Cov.c() | public java.lang.String L3W94PublicMethodWalk$Cov.c()
// class-beats-default: public java.lang.String L3W94PublicMethodWalk$SupD.d()
// abstract-redeclare: public abstract java.lang.String L3W94PublicMethodWalk$E1.e()
// covariant-ifaces: public abstract java.lang.String L3W94PublicMethodWalk$R1.r()
// iface-object-method: java.lang.NoSuchMethodException: L3W94PublicMethodWalk$I0.hashCode()
// runnable-tostring: java.lang.NoSuchMethodException: java.lang.Runnable.toString()
// methods-iface-object: (none)
// field-first-iface: public static final int L3W94PublicMethodWalk$F1.X
// sub-iface-second-again: public default java.lang.String L3W94PublicMethodWalk$I1.m()
// HOTSPOT_EXPECTED_END
//
// The old depth-first walks (code reading) answer I0's m for
// `sub-iface-second` and `methods-sub-iface`, B's n for `unrelated-ifaces`,
// B's n alone for `methods-unrelated`, E0's e for `abstract-redeclare` and
// R0's r for `covariant-ifaces`, wherever the served Bridge answers (under
// `--jdk-only` the JDK body of `getMethod` / `getMethods` may run instead).
// `CRATONVM_PUBLIC_METHOD_JDK_WALK=1` switches the Bridges to JDK 25's walk.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W94PublicMethodWalk`,
// and each with `CRATONVM_PUBLIC_METHOD_JDK_WALK=1`.
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public class L3W94PublicMethodWalk {
    public interface I0 { default String m() { return "I0"; } }
    public interface I1 extends I0 { @Override default String m() { return "I1"; } }
    public abstract static class C10 implements I1, I0 { }
    public abstract static class C01 implements I0, I1 { }

    public interface A { void n(); }
    public interface B { void n(); }
    public abstract static class CAB implements A, B { }

    public static class Base { public Object c() { return "base"; } }
    public static class Cov extends Base { @Override public String c() { return "cov"; } }

    public interface D { default String d() { return "D"; } }
    public static class SupD { public String d() { return "SupD"; } }
    public static class SubD extends SupD implements D { }

    public interface E0 { default String e() { return "E0"; } }
    public interface E1 extends E0 { @Override String e(); }
    public abstract static class CE implements E1, E0 { }

    public interface R0 { Object r(); }
    public interface R1 { String r(); }
    public abstract static class CR implements R1, R0 { }

    public interface F1 { int X = 1; }
    public interface F2 { int X = 2; }
    public static class CF implements F1, F2 { }

    interface Call { Object run() throws Throwable; }

    static void row(String label, Call call) {
        String out;
        try {
            out = String.valueOf(call.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + ": " + out);
    }

    static String named(Class<?> c, String name) {
        List<String> out = new ArrayList<>();
        for (Method m : c.getMethods()) {
            if (m.getName().equals(name)) {
                out.add(m.toString());
            }
        }
        if (out.isEmpty()) {
            return "(none)";
        }
        Collections.sort(out);
        return String.join(" | ", out);
    }

    public static void main(String[] args) {
        row("sub-iface-second", () -> C10.class.getMethod("m"));
        row("sub-iface-first", () -> C01.class.getMethod("m"));
        row("methods-sub-iface", () -> named(C10.class, "m"));
        row("unrelated-ifaces", () -> CAB.class.getMethod("n"));
        row("methods-unrelated", () -> named(CAB.class, "n"));
        row("covariant-bridge", () -> Cov.class.getMethod("c"));
        row("methods-covariant", () -> named(Cov.class, "c"));
        row("class-beats-default", () -> SubD.class.getMethod("d"));
        row("abstract-redeclare", () -> CE.class.getMethod("e"));
        row("covariant-ifaces", () -> CR.class.getMethod("r"));
        row("iface-object-method", () -> I0.class.getMethod("hashCode"));
        row("runnable-tostring", () -> Runnable.class.getMethod("toString"));
        row("methods-iface-object", () -> named(I1.class, "toString"));
        row("field-first-iface", () -> CF.class.getField("X"));
        row("sub-iface-second-again", () -> C10.class.getMethod("m"));
    }
}
