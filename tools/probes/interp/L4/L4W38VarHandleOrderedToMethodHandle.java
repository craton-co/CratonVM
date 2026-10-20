// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L4: `VarHandle.toMethodHandle` of an
// ORDERED access mode on a field handle (item 3 of
// docs/internal/fixed-bugs/interpreter-L4-review-of-waves-30-36-invoke-and-indy-small-divergences-FIXED-20261003.md).
//
// Before wave 38 every `GET*` / `SET*` mode of a field handle became the plain
// field accessor, so `SET_VOLATILE`, `SET_RELEASE`, `GET_ACQUIRE`, ... on a
// non-volatile field were a plain store or load (the ordering is not
// observable from one thread; this probe pins the answers and types of the
// mode-invoker form they take now, as every read-modify-write mode already
// did). A final field's ordered write is the mode's
// `UnsupportedOperationException` when invoked.
//
// Run: javac -d out L4W38VarHandleOrderedToMethodHandle.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W38VarHandleOrderedToMethodHandle
//
// Expected HotSpot 25 output (default and -Xint); every mode prints the same:
//   GET_VOLATILE: (H)int -> 10
//   GET_ACQUIRE: (H)int -> 10
//   GET_OPAQUE: (H)int -> 10
//   SET_VOLATILE: (H,int)void -> now 11
//   SET_RELEASE: (H,int)void -> now 12
//   SET_OPAQUE: (H,int)void -> now 13
//   static GET_VOLATILE: ()String -> s
//   static SET_RELEASE: (String)void -> now t
//   final GET_ACQUIRE: (H)int -> 7
//   final SET_VOLATILE: (H,int)void -> java.lang.UnsupportedOperationException: setVolatile
//   final SET_RELEASE: (H,int)void -> java.lang.UnsupportedOperationException: setRelease
//   GET: (H)int -> 13
//   SET: (H,int)void -> now 14
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;

public class L4W38VarHandleOrderedToMethodHandle {
    static class H {
        int v = 10;
        final int f = 7;
        static String s = "s";
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

    static Body read(VarHandle vh, AccessMode mode, H h) {
        return () -> {
            MethodHandle mh = vh.toMethodHandle(mode);
            return type(mh) + " -> " + (int) mh.invoke(h);
        };
    }

    static Body write(VarHandle vh, AccessMode mode, H h, int value) {
        return () -> {
            MethodHandle mh = vh.toMethodHandle(mode);
            String t = type(mh);
            try {
                mh.invoke(h, value);
            } catch (Throwable e) {
                return t + " -> " + describe(e);
            }
            return t + " -> now " + h.v;
        };
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle v = l.findVarHandle(H.class, "v", int.class);
        VarHandle f = l.findVarHandle(H.class, "f", int.class);
        VarHandle s = l.findStaticVarHandle(H.class, "s", String.class);
        H h = new H();

        row("GET_VOLATILE", read(v, AccessMode.GET_VOLATILE, h));
        row("GET_ACQUIRE", read(v, AccessMode.GET_ACQUIRE, h));
        row("GET_OPAQUE", read(v, AccessMode.GET_OPAQUE, h));
        row("SET_VOLATILE", write(v, AccessMode.SET_VOLATILE, h, 11));
        row("SET_RELEASE", write(v, AccessMode.SET_RELEASE, h, 12));
        row("SET_OPAQUE", write(v, AccessMode.SET_OPAQUE, h, 13));
        row("static GET_VOLATILE", () -> {
            MethodHandle mh = s.toMethodHandle(AccessMode.GET_VOLATILE);
            return type(mh) + " -> " + (String) mh.invoke();
        });
        row("static SET_RELEASE", () -> {
            MethodHandle mh = s.toMethodHandle(AccessMode.SET_RELEASE);
            mh.invoke("t");
            return type(mh) + " -> now " + H.s;
        });
        row("final GET_ACQUIRE", read(f, AccessMode.GET_ACQUIRE, h));
        row("final SET_VOLATILE", write(f, AccessMode.SET_VOLATILE, h, 99));
        row("final SET_RELEASE", write(f, AccessMode.SET_RELEASE, h, 99));
        row("GET", read(v, AccessMode.GET, h));
        row("SET", write(v, AccessMode.SET, h, 14));
    }
}
