// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L4
// (`docs/internal/fixed-bugs/interpreter-L4-proposal-a-varhandle-row-keeps-its-field-holder-apart-from-its-coordinate-FIXED-20261010.md`):
// a PROTECTED instance field of another package, reached through a
// subclass's lookup, is restricted to the lookup class (JDK 25
// `getFieldVarHandleCommon`: `refc = lookupClass()`) even when that subclass
// declares a HIDING field of the same name. The handle still names the
// superclass's field (HotSpot's handle holds the field's offset).
//
// `Sub extends ByteArrayOutputStream` and declares its own `int count`,
// hiding the protected `ByteArrayOutputStream.count`.
//
// Run: javac -d out L4W46VarHandleHidingField.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W46VarHandleHidingField
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   find-coords: [Sub]
//   find-get: 3 42
//   find-set: 1 42
//   find-plain-receiver: ClassCastException: Cannot cast java.io.ByteArrayOutputStream to L4W46VarHandleHidingField$Sub
//   find-to-mh-get: (Sub)int 1
//   find-to-mh-set: (Sub,int)void 7 42
//   unreflect-coords: [Sub]
//   unreflect-get: 7
//
// On the base (55834015b), every mode: the restriction was applied only when
// the lookup class resolved the name to the same slot
// (`vh_protected_coordinate`), so a hiding field kept the unrestricted
// handle: `find-coords: [ByteArrayOutputStream]`, `find-plain-receiver: 0`
// (the read succeeds), `find-to-mh-get: (ByteArrayOutputStream)int 1`,
// `find-to-mh-set: (ByteArrayOutputStream,int)void 7 42`,
// `unreflect-coords: [ByteArrayOutputStream]`. The `get`/`set` rows read the
// right field on the base too (the slot came from the declaring class).
import java.io.ByteArrayOutputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W46VarHandleHidingField {
    static class Sub extends ByteArrayOutputStream {
        int count = 42;

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }

        int superCount() {
            return super.count;
        }
    }

    static String names(VarHandle vh) {
        StringBuilder sb = new StringBuilder("[");
        for (Class<?> c : vh.coordinateTypes()) {
            if (sb.length() > 1) sb.append(", ");
            sb.append(c.getSimpleName());
        }
        return sb.append(']').toString();
    }

    static String type(MethodType t) {
        StringBuilder sb = new StringBuilder("(");
        for (int i = 0; i < t.parameterCount(); i++) {
            if (i > 0) sb.append(',');
            sb.append(t.parameterType(i).getSimpleName());
        }
        return sb.append(')').append(t.returnType().getSimpleName()).toString();
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
        MethodHandles.Lookup lk = Sub.lookup();
        VarHandle vh = lk.findVarHandle(ByteArrayOutputStream.class, "count", int.class);
        Sub sub = new Sub();
        sub.write(new byte[] {1, 2, 3}, 0, 3);
        row("find-coords", () -> names(vh));
        row("find-get", () -> (int) vh.get(sub) + " " + sub.count);
        row("find-set", () -> {
            vh.set(sub, 1);
            return sub.superCount() + " " + sub.count;
        });
        row("find-plain-receiver", () -> String.valueOf((int) vh.get(new ByteArrayOutputStream())));
        row("find-to-mh-get", () -> {
            MethodHandle mh = vh.toMethodHandle(VarHandle.AccessMode.GET);
            return type(mh.type()) + " " + (int) mh.invoke(sub);
        });
        row("find-to-mh-set", () -> {
            MethodHandle mh = vh.toMethodHandle(VarHandle.AccessMode.SET);
            mh.invoke(sub, 7);
            return type(mh.type()) + " " + sub.superCount() + " " + sub.count;
        });
        VarHandle un = lk.unreflectVarHandle(ByteArrayOutputStream.class.getDeclaredField("count"));
        row("unreflect-coords", () -> names(un));
        row("unreflect-get", () -> String.valueOf((int) un.get(sub)));
    }
}
