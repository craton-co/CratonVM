// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L4
// (`interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`, the parts
// that need no emitted code): the describe-yourself surface of a VarHandle
// and of its exact twin -- `toString`, `describeConstable`, identity of the
// behaviour builders, `isAccessModeSupported` on exact and read-only
// handles, and `toMethodHandle` of an exact handle.
//
// Run: javac -d out L4W45VarHandleQueries.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W45VarHandleQueries
//
// Expected HotSpot 25 output (default and -Xint, measured locally); the same
// in --compatible except the `exact-*` rows (an exact VarHandle is
// --jdk-only's; --compatible's `withInvokeExactBehavior` answers the plain
// handle):
//   to-string-field: VarHandle[varType=int, coord=[class L4W45VarHandleQueries$H]]
//   to-string-static: VarHandle[varType=java.lang.String, coord=[]]
//   to-string-array: VarHandle[varType=int, coord=[class [I, int]]
//   to-string-exact: VarHandle[varType=int, coord=[class L4W45VarHandleQueries$H]]
//   describe-field: Optional[VarHandleDesc[L4W45VarHandleQueries$H.x:int]]
//   describe-static: Optional[VarHandleDesc[static L4W45VarHandleQueries$H.s:String]]
//   describe-array: Optional[VarHandleDesc[int[][]]]
//   describe-exact: Optional[VarHandleDesc[L4W45VarHandleQueries$H.x:int]]
//   plain-with-plain-same: true
//   exact-with-exact-same: true
//   exact-with-plain-exact: false
//   exact-supported-cas: true
//   final-supported-set: false
//   final-supported-get-volatile: true
//   final-exact-supported-set: false
//   static-supported-get-and-add: false
//   exact-mode-type: (H,int)int
//   exact-to-mh-type: (H,int)int
//   exact-to-mh-invoke: 4
//   exact-to-mh-invoke-exact-wrong: WrongMethodTypeException: handle's method type (H)int but found (H)long
//   final-to-mh-set: UnsupportedOperationException: set
//   exact-var-type: int
//   exact-coords: [class L4W45VarHandleQueries$H]
//   exact-equals-plain: false
//
// On the base (69568bea6), read from the code: every `describe-*` row
// printed `Optional.empty` (a CratonVM-minted handle is a plain
// `java.lang.invoke.VarHandle`, whose own `describeConstable` answers empty;
// the JDK's field and array handle classes override it). The other rows are
// the review's controls (not traced to a divergence).
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;

public class L4W45VarHandleQueries {
    static class H {
        int x;
        final long f = 3;
        static String s = "s";
    }

    static String shortType(MethodType mt) {
        StringBuilder sb = new StringBuilder("(");
        for (int i = 0; i < mt.parameterCount(); i++) {
            if (i > 0) sb.append(",");
            sb.append(mt.parameterType(i).getSimpleName());
        }
        return sb.append(")").append(mt.returnType().getSimpleName()).toString();
    }

    interface Row {
        String run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = r.run();
        } catch (Throwable t) {
            out = t.getClass().getSimpleName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle x = lk.findVarHandle(H.class, "x", int.class);
        VarHandle f = lk.findVarHandle(H.class, "f", long.class);
        VarHandle s = lk.findStaticVarHandle(H.class, "s", String.class);
        VarHandle a = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle xx = x.withInvokeExactBehavior();

        row("to-string-field", () -> x.toString());
        row("to-string-static", () -> s.toString());
        row("to-string-array", () -> a.toString());
        row("to-string-exact", () -> xx.toString());
        row("describe-field", () -> x.describeConstable().toString());
        row("describe-static", () -> s.describeConstable().toString());
        row("describe-array", () -> a.describeConstable().toString());
        row("describe-exact", () -> xx.describeConstable().toString());
        row("plain-with-plain-same", () -> String.valueOf(x.withInvokeBehavior() == x));
        row("exact-with-exact-same", () -> String.valueOf(xx.withInvokeExactBehavior() == xx));
        row("exact-with-plain-exact", () -> String.valueOf(xx.withInvokeBehavior().hasInvokeExactBehavior()));
        row("exact-supported-cas", () -> String.valueOf(xx.isAccessModeSupported(AccessMode.COMPARE_AND_SET)));
        row("final-supported-set", () -> String.valueOf(f.isAccessModeSupported(AccessMode.SET)));
        row("final-supported-get-volatile", () -> String.valueOf(f.isAccessModeSupported(AccessMode.GET_VOLATILE)));
        row("final-exact-supported-set", () -> String.valueOf(
                f.withInvokeExactBehavior().isAccessModeSupported(AccessMode.SET)));
        row("static-supported-get-and-add", () -> String.valueOf(s.isAccessModeSupported(AccessMode.GET_AND_ADD)));
        row("exact-mode-type", () -> shortType(xx.accessModeType(AccessMode.GET_AND_ADD)));
        row("exact-to-mh-type", () -> shortType(xx.toMethodHandle(AccessMode.GET_AND_ADD).type()));
        row("exact-to-mh-invoke", () -> {
            H h = new H();
            h.x = 4;
            MethodHandle m = xx.toMethodHandle(AccessMode.GET);
            return String.valueOf((long) m.invoke(h));
        });
        row("exact-to-mh-invoke-exact-wrong", () -> {
            H h = new H();
            MethodHandle m = xx.toMethodHandle(AccessMode.GET);
            return String.valueOf((long) m.invokeExact(h));
        });
        row("final-to-mh-set", () -> {
            H h = new H();
            MethodHandle m = f.toMethodHandle(AccessMode.SET);
            m.invoke(h, 9L);
            return String.valueOf(h.f);
        });
        row("exact-var-type", () -> xx.varType().getName());
        row("exact-coords", () -> xx.coordinateTypes().toString());
        row("exact-equals-plain", () -> String.valueOf(xx.equals(x)));
    }
}
