// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L4: `VarHandle.getMethodHandle(mode)`
// -- what the JDK's VarHandle invoker forms call
// (`Invokers.checkVarHandleGenericType`) -- reached WITHOUT passing through
// a method-handle door: reflectively, so wave 56's call-side screens in
// `mh_run_foreign_vmentry` cannot answer it. On CratonVM the handles
// `findVarHandle` / `findStaticVarHandle` / `arrayElementVarHandle` mint have
// no `vform`, and the JDK body of `getMethodHandleUncached` read it: a
// `NullPointerException` (wave 56). With `CRATONVM_VH_SYNTHETIC_MH_TABLE`
// the method is answered at the source (proposal
// `interpreter-L4-proposal-give-synthetic-varhandles-the-jdks-method-handle-table-FIXED-20261021`).
// Positive control (`CRATONVM_DBG_MH_DISPATCH=1`): `[VH_MH_TABLE]
// getMethodHandleUncached(..) of a synthetic VarHandle` lines. The first row
// is repeated at the end (cold-VM gaps).
//
// Run: javac -d out L4W57VarHandleMethodHandleTable.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W57VarHandleMethodHandleTable
//   (HotSpot: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out ...)
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   get-type: (VarHandle,L4W57VarHandleMethodHandleTable)int
//   get-invoke: 4
//   get-cached: true
//   set-type: (VarHandle,L4W57VarHandleMethodHandleTable,int)void
//   set-invoke: 11
//   gaa-type: (VarHandle,L4W57VarHandleMethodHandleTable,long)long
//   gaa-invoke: 40 41
//   cas-type: (VarHandle,L4W57VarHandleMethodHandleTable,String,String)boolean
//   cas-invoke: true t
//   static-type: (VarHandle)int
//   static-invoke: 99
//   array-type: (VarHandle,int[],int)int
//   array-invoke: 8
//   get-type-again: (VarHandle,L4W57VarHandleMethodHandleTable)int
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Method;

public class L4W57VarHandleMethodHandleTable {
    int i = 4;
    long l = 40L;
    String s = "s";
    static int st = 99;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static Method getMethodHandle;

    static MethodHandle table(VarHandle vh, VarHandle.AccessMode mode) throws Throwable {
        return (MethodHandle) getMethodHandle.invoke(vh, mode.ordinal());
    }

    public static void main(String[] args) throws Throwable {
        getMethodHandle = VarHandle.class.getDeclaredMethod("getMethodHandle", int.class);
        getMethodHandle.setAccessible(true);
        Class<?> me = L4W57VarHandleMethodHandleTable.class;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle vi = lk.findVarHandle(me, "i", int.class);
        VarHandle vl = lk.findVarHandle(me, "l", long.class);
        VarHandle vs = lk.findVarHandle(me, "s", String.class);
        VarHandle vst = lk.findStaticVarHandle(me, "st", int.class);
        VarHandle va = MethodHandles.arrayElementVarHandle(int[].class);
        L4W57VarHandleMethodHandleTable o = new L4W57VarHandleMethodHandleTable();
        row("get-type", () -> table(vi, VarHandle.AccessMode.GET).type());
        row("get-invoke", () -> (int) table(vi, VarHandle.AccessMode.GET).invoke(vi, o));
        row("get-cached", () -> table(vi, VarHandle.AccessMode.GET) == table(vi, VarHandle.AccessMode.GET));
        row("set-type", () -> table(vi, VarHandle.AccessMode.SET).type());
        row("set-invoke", () -> {
            table(vi, VarHandle.AccessMode.SET).invoke(vi, o, 11);
            return o.i;
        });
        row("gaa-type", () -> table(vl, VarHandle.AccessMode.GET_AND_ADD).type());
        row("gaa-invoke", () -> {
            long was = (long) table(vl, VarHandle.AccessMode.GET_AND_ADD).invoke(vl, o, 1L);
            return was + " " + o.l;
        });
        row("cas-type", () -> table(vs, VarHandle.AccessMode.COMPARE_AND_SET).type());
        row("cas-invoke", () -> {
            boolean ok = (boolean) table(vs, VarHandle.AccessMode.COMPARE_AND_SET).invoke(vs, o, o.s, "t");
            return ok + " " + o.s;
        });
        row("static-type", () -> table(vst, VarHandle.AccessMode.GET).type());
        row("static-invoke", () -> (int) table(vst, VarHandle.AccessMode.GET).invoke(vst));
        row("array-type", () -> table(va, VarHandle.AccessMode.GET).type());
        row("array-invoke", () -> (int) table(va, VarHandle.AccessMode.GET).invoke(va, new int[] {8}, 0));
        row("get-type-again", () -> table(vi, VarHandle.AccessMode.GET).type());
    }
}
