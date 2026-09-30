// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L4
// (`interpreter-L4-findvarhandle-of-a-protected-field-keeps-the-declaring-class-coordinate-FIXED-20261009`):
// JDK 25 `Lookup.getFieldVarHandleCommon` restricts a PROTECTED instance
// field of a class in another run-time package, found through a subclass's
// lookup, to the LOOKUP class (`refc = lookupClass()`), for `findVarHandle`
// and `unreflectVarHandle`. Unlike `restrictReceiver` (the MethodHandle
// finders), there is no "already narrow" exemption: `findVarHandle(SubSub.class,
// ..)` from `Sub` is `[Sub]` too.
//
// Run: javac -d out L4W45ProtectedVarHandle.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W45ProtectedVarHandle
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible except `find-exact-declaring` (an exact VarHandle is
// judged in --jdk-only only; --compatible's `withInvokeExactBehavior` answers
// the plain handle and prints `0`):
//   find-coords: [Sub]
//   find-narrower-refc: [Sub]
//   find-mode-type: (Sub,int)int
//   find-get: 3
//   find-get-subsub: 1
//   find-plain-receiver: ClassCastException: Cannot cast java.io.ByteArrayOutputStream to L4W45ProtectedVarHandle$Sub
//   find-set-cas: true 8
//   find-exact-sub: 2
//   find-exact-declaring: WrongMethodTypeException: handle's method type (Sub)int but found (ByteArrayOutputStream)int
//   find-to-method-handle: (Sub)int
//   unreflect-coords: [Sub]
//   unreflect-get: 1
//   unrelated: IllegalAccessException
//   sub-refc: [Sub]
//
// On the base (69568bea6) CratonVM minted the handle for the REQUESTED class:
// every `*-coords` / `*-mode-type` row names `ByteArrayOutputStream` (or
// `SubSub`), `find-plain-receiver` reads `1` instead of throwing, and
// `find-exact-sub` is a `WrongMethodTypeException` (the exact handle's type
// was `(ByteArrayOutputStream)int`).
import java.io.ByteArrayOutputStream;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Field;

public class L4W45ProtectedVarHandle {
    static class Sub extends ByteArrayOutputStream {
        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    static class SubSub extends Sub {
    }

    static String names(VarHandle vh) {
        StringBuilder sb = new StringBuilder("[");
        for (Class<?> c : vh.coordinateTypes()) {
            if (sb.length() > 1) sb.append(", ");
            sb.append(c.getSimpleName());
        }
        return sb.append("]").toString();
    }

    static String shortType(MethodType mt) {
        StringBuilder sb = new StringBuilder("(");
        for (int i = 0; i < mt.parameterCount(); i++) {
            if (i > 0) sb.append(",");
            sb.append(mt.parameterType(i).getSimpleName());
        }
        return sb.append(")").append(mt.returnType().getSimpleName()).toString();
    }

    static String err(Throwable t) {
        return t.getClass().getSimpleName() + ": " + t.getMessage();
    }

    interface Row {
        String run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = r.run();
        } catch (Throwable t) {
            out = err(t);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = Sub.lookup();
        row("find-coords", () -> names(lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class)));
        row("find-narrower-refc", () -> names(lk.findVarHandle(SubSub.class, "count", int.class)));
        row("find-mode-type", () -> shortType(
                lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class)
                        .accessModeType(VarHandle.AccessMode.GET_AND_ADD)));
        row("find-get", () -> {
            VarHandle vh = lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class);
            Sub s = new Sub();
            s.write(1);
            s.write(2);
            s.write(3);
            return String.valueOf((int) vh.get(s));
        });
        row("find-get-subsub", () -> {
            VarHandle vh = lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class);
            SubSub s = new SubSub();
            s.write(1);
            return String.valueOf((int) vh.get(s));
        });
        row("find-plain-receiver", () -> {
            VarHandle vh = lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class);
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            b.write(1);
            return String.valueOf((int) vh.get((Object) b));
        });
        row("find-set-cas", () -> {
            VarHandle vh = lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class);
            Sub s = new Sub();
            s.write(new byte[16], 0, 16);
            vh.set(s, 7);
            boolean ok = vh.compareAndSet(s, 7, 8);
            return ok + " " + s.size();
        });
        row("find-exact-sub", () -> {
            VarHandle vh = lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class)
                    .withInvokeExactBehavior();
            Sub s = new Sub();
            s.write(1);
            s.write(2);
            return String.valueOf((int) vh.get(s));
        });
        row("find-exact-declaring", () -> {
            VarHandle vh = lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class)
                    .withInvokeExactBehavior();
            ByteArrayOutputStream s = new Sub();
            return String.valueOf((int) vh.get(s));
        });
        row("find-to-method-handle", () -> shortType(
                lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class)
                        .toMethodHandle(VarHandle.AccessMode.GET).type()));
        row("unreflect-coords", () -> {
            Field f = ByteArrayOutputStream.class.getDeclaredField("count");
            return names(lk.unreflectVarHandle(f));
        });
        row("unreflect-get", () -> {
            Field f = ByteArrayOutputStream.class.getDeclaredField("count");
            VarHandle vh = lk.unreflectVarHandle(f);
            Sub s = new Sub();
            s.write(5);
            return String.valueOf((int) vh.get(s));
        });
        row("unrelated", () -> {
            try {
                return names(MethodHandles.lookup()
                        .findVarHandle(ByteArrayOutputStream.class, "count", int.class));
            } catch (IllegalAccessException e) {
                // The message ends in the module's identity hash.
                return "IllegalAccessException";
            }
        });
        row("sub-refc", () -> names(lk.findVarHandle(Sub.class, "count", int.class)));
    }
}
