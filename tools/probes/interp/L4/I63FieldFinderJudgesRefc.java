// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// docs/internal/fixed-bugs/i63-L4-findgetter-resolves-refc-by-name-FIXED-20261008.md:
// `Lookup.findGetter` / `findSetter` judge the field in exactly the class
// `refc` IS, not in whatever class of that NAME the caller's loader sees.
//
// Each row defines its own `I63Holder` in a fresh loader (hand-emitted class
// bytes: a public class whose field `f` differs per row) and asks this
// class's lookup for `f` with that class as `refc`. The probe's own
// (app-loader) `I63Holder` below declares `public int f`, so a finder that
// re-loads `refc` by name through the CALLER's loader judges the wrong class
// and answers a handle where HotSpot refuses.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3, default flags and -Xint)
// absent: NoSuchFieldException
// other-type: NoSuchFieldException
// static: IllegalAccessException
// final-setter: IllegalAccessException
// same-getter: (I63Holder)int
// same-invoke: 7
// same-invoke-app-instance: ClassCastException
// same-setter: (I63Holder,int)void
// same-setter-invoke: 9
// HOTSPOT_EXPECTED_END
//
// CratonVM: `--jdk-only` matched on the base (the JDK's own `Lookup`);
// `--compatible` on the base 861b7c23b2 printed a handle type in all four
// refusal rows. With LOOKUP_FIELD_FINDERS_JUDGE_THE_MIRROR, HotSpot's lines
// in every mode (also `--compatible --nojit`).
//
// Run: javac -d out I63FieldFinderJudgesRefc.java
//      cratonvm [--compatible] [--nojit] -cp out I63FieldFinderJudgesRefc
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

class I63Holder {
    public int f;
}

public class I63FieldFinderJudgesRefc {
    static final int ACC_PUBLIC = 0x0001;
    static final int ACC_STATIC = 0x0008;
    static final int ACC_FINAL = 0x0010;

    static final class Definer extends ClassLoader {
        Definer() {
            super(null);
        }

        Class<?> define(byte[] bytes) {
            return defineClass("I63Holder", bytes, 0, bytes.length);
        }
    }

    /** A public `I63Holder` with a no-arg constructor and, unless `fieldAccess < 0`, field `f`. */
    static byte[] holder(int fieldAccess, String desc) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(12); // constant pool count
        out.writeByte(1); out.writeUTF("I63Holder");         // #1
        out.writeByte(7); out.writeShort(1);                  // #2 Class I63Holder
        out.writeByte(1); out.writeUTF("java/lang/Object");  // #3
        out.writeByte(7); out.writeShort(3);                  // #4 Class Object
        out.writeByte(1); out.writeUTF("f");                  // #5
        out.writeByte(1); out.writeUTF(desc);                 // #6
        out.writeByte(1); out.writeUTF("<init>");             // #7
        out.writeByte(1); out.writeUTF("()V");                // #8
        out.writeByte(12); out.writeShort(7); out.writeShort(8); // #9 NameAndType
        out.writeByte(10); out.writeShort(4); out.writeShort(9); // #10 Methodref
        out.writeByte(1); out.writeUTF("Code");               // #11
        out.writeShort(0x0021); // public super
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0); // interfaces
        if (fieldAccess < 0) {
            out.writeShort(0);
        } else {
            out.writeShort(1);
            out.writeShort(fieldAccess);
            out.writeShort(5);
            out.writeShort(6);
            out.writeShort(0);
        }
        out.writeShort(1); // methods
        out.writeShort(ACC_PUBLIC);
        out.writeShort(7);
        out.writeShort(8);
        out.writeShort(1);
        out.writeShort(11);
        out.writeInt(17);
        out.writeShort(1); // max_stack
        out.writeShort(1); // max_locals
        out.writeInt(5);
        out.write(new byte[] {0x2a, (byte) 0xb7, 0x00, 0x0a, (byte) 0xb1});
        out.writeShort(0); // exception table
        out.writeShort(0); // code attributes
        out.writeShort(0); // class attributes
        out.flush();
        return bytes.toByteArray();
    }

    static Class<?> define(int fieldAccess, String desc) throws IOException {
        return new Definer().define(holder(fieldAccess, desc));
    }

    static String shown(Throwable t) {
        return t.getClass().getSimpleName();
    }

    static String type(MethodHandle mh) {
        return mh.type().toString();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        // The caller's loader sees its own `I63Holder` with `public int f`.
        new I63Holder().f = 1;

        Object[][] refusals = {
            {"absent", -1, "I", false},
            {"other-type", ACC_PUBLIC, "J", false},
            {"static", ACC_PUBLIC | ACC_STATIC, "I", false},
            {"final-setter", ACC_PUBLIC | ACC_FINAL, "I", true},
        };
        for (Object[] row : refusals) {
            Class<?> refc = define((Integer) row[1], (String) row[2]);
            String out;
            try {
                MethodHandle mh = (Boolean) row[3]
                        ? lookup.findSetter(refc, "f", int.class)
                        : lookup.findGetter(refc, "f", int.class);
                out = type(mh);
            } catch (Throwable t) {
                out = shown(t);
            }
            System.out.println(row[0] + ": " + out);
        }

        Class<?> same = define(ACC_PUBLIC, "I");
        Object instance = same.getConstructor().newInstance();
        same.getField("f").setInt(instance, 7);
        MethodHandle getter = lookup.findGetter(same, "f", int.class);
        System.out.println("same-getter: " + type(getter));
        System.out.println("same-invoke: " + (int) getter.invoke(instance));
        String appInstance;
        try {
            appInstance = String.valueOf((int) getter.invoke(new I63Holder()));
        } catch (Throwable t) {
            appInstance = shown(t);
        }
        System.out.println("same-invoke-app-instance: " + appInstance);
        MethodHandle setter = lookup.findSetter(same, "f", int.class);
        System.out.println("same-setter: " + type(setter));
        setter.invoke(instance, 9);
        System.out.println("same-setter-invoke: " + same.getField("f").getInt(instance));
    }
}
