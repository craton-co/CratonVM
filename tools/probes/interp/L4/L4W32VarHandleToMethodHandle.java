// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 32 (orchestrator): `VarHandle.toMethodHandle` for
// every access mode, as HotSpot builds it: the mode's invoker bound to the
// handle, typed `accessModeType(mode)`
// (docs/internal/fixed-bugs/interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010.md,
// item 4).
//
// Before wave 32 CratonVM answered only the GET*/SET* forms of a field handle:
// every read-modify-write mode, and every array handle, was an
// `UnsupportedOperationException` from `toMethodHandle` itself, and the SET
// form of a final field stored. An unsupported mode's message was a CratonVM
// sentence; HotSpot's is the mode's method name.
//
// Run: javac -d out L4W32VarHandleToMethodHandle.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W32VarHandleToMethodHandle
//
// Expected HotSpot 25 output (default and -Xint):
//   GET: (H)int -> 10
//   GET_AND_ADD: (H,int)int -> 10 now 15
//   COMPARE_AND_SET: (H,int,int)boolean -> true now 20
//   GET_AND_BITWISE_OR: (H,int)int -> 20 now 21
//   GET_AND_SET static: (int)int -> 3 now 4
//   SET final: (H,int)void -> java.lang.UnsupportedOperationException: set
//   GET final: (H)int -> 7
//   array GET_AND_ADD: (int[],int,int)int -> 2 now 12
//   array GET: (int[],int)int -> 12
//   GET_AND_ADD String: java.lang.UnsupportedOperationException: getAndAdd
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;

public class L4W32VarHandleToMethodHandle {
    static class H {
        int v = 10;
        final int f = 7;
        String s = "s";
        static int sv = 3;
    }

    static String type(MethodHandle mh) {
        MethodType t = mh.type();
        StringBuilder b = new StringBuilder("(");
        for (int i = 0; i < t.parameterCount(); i++) {
            if (i > 0) {
                b.append(',');
            }
            b.append(t.parameterType(i).getSimpleName());
        }
        return b.append(')').append(t.returnType().getSimpleName()).toString();
    }

    static String describe(Throwable t) {
        return t.getMessage() == null ? t.getClass().getName() : t.getClass().getName() + ": " + t.getMessage();
    }

    interface Body {
        String run() throws Throwable;
    }

    static void row(String label, Body body) {
        String out;
        try {
            out = body.run();
        } catch (Throwable t) {
            out = describe(t);
        }
        System.out.println(label + ": " + out);
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle v = l.findVarHandle(H.class, "v", int.class);
        VarHandle f = l.findVarHandle(H.class, "f", int.class);
        VarHandle s = l.findVarHandle(H.class, "s", String.class);
        VarHandle sv = l.findStaticVarHandle(H.class, "sv", int.class);
        VarHandle arr = MethodHandles.arrayElementVarHandle(int[].class);
        H h = new H();
        int[] a = {1, 2, 3};

        row("GET", () -> {
            MethodHandle mh = v.toMethodHandle(AccessMode.GET);
            return type(mh) + " -> " + (int) mh.invoke(h);
        });
        row("GET_AND_ADD", () -> {
            MethodHandle mh = v.toMethodHandle(AccessMode.GET_AND_ADD);
            return type(mh) + " -> " + (int) mh.invoke(h, 5) + " now " + h.v;
        });
        row("COMPARE_AND_SET", () -> {
            MethodHandle mh = v.toMethodHandle(AccessMode.COMPARE_AND_SET);
            return type(mh) + " -> " + (boolean) mh.invoke(h, 15, 20) + " now " + h.v;
        });
        row("GET_AND_BITWISE_OR", () -> {
            MethodHandle mh = v.toMethodHandle(AccessMode.GET_AND_BITWISE_OR);
            return type(mh) + " -> " + (int) mh.invoke(h, 1) + " now " + h.v;
        });
        row("GET_AND_SET static", () -> {
            MethodHandle mh = sv.toMethodHandle(AccessMode.GET_AND_SET);
            return type(mh) + " -> " + (int) mh.invoke(4) + " now " + H.sv;
        });
        row("SET final", () -> {
            MethodHandle mh = f.toMethodHandle(AccessMode.SET);
            String t = type(mh);
            try {
                mh.invoke(h, 99);
                return t + " -> stored, now " + f.get(h);
            } catch (Throwable e) {
                return t + " -> " + describe(e);
            }
        });
        row("GET final", () -> {
            MethodHandle mh = f.toMethodHandle(AccessMode.GET);
            return type(mh) + " -> " + (int) mh.invoke(h);
        });
        row("array GET_AND_ADD", () -> {
            MethodHandle mh = arr.toMethodHandle(AccessMode.GET_AND_ADD);
            return type(mh) + " -> " + (int) mh.invoke(a, 1, 10) + " now " + a[1];
        });
        row("array GET", () -> {
            MethodHandle mh = arr.toMethodHandle(AccessMode.GET);
            return type(mh) + " -> " + (int) mh.invoke(a, 1);
        });
        row("GET_AND_ADD String", () -> {
            MethodHandle mh = s.toMethodHandle(AccessMode.GET_AND_ADD);
            try {
                return type(mh) + " -> " + mh.invoke(h, "x");
            } catch (Throwable e) {
                return describe(e);
            }
        });
    }
}
