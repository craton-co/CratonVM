// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 85, lane L5 -- `Lookup.defineHiddenClass` as
// HotSpot 25 answers it: the name's `/0x<16 hex>` suffix, `Class.forName` on
// a hidden name (ClassNotFoundException), the NESTMATE option's nest host,
// `defineHiddenClassWithClassData` + `MethodHandles.classData`, a hidden
// class's array type, and unloading: a hidden class defined WITHOUT
// `ClassOption.STRONG` is collected once unreachable, a STRONG one lives as
// long as its loader.
//
// Run:
//   $J/javac -d out L5W85HiddenClasses.java
//   $J/java -cp out L5W85HiddenClasses                     # HotSpot
//   $B --java-home $J/.. -cp out L5W85HiddenClasses
//   $B --compatible --java-home $J/.. -cp out L5W85HiddenClasses
//   (and each with -XX:+UseG1GC / -XX:+UseZGC for the unloading rows)
// Expected block read from JDK 25's MethodHandles.Lookup / ClassDefiner and
// HotSpot's `mangle_hidden_class_name` (INTPTR_FORMAT, 16 hex digits on a
// 64-bit VM); confirm it on the host first.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, both CratonVM modes)
// name-shape: true hidden=true canonical=null simple-has-suffix=true
// forName: java.lang.ClassNotFoundException message-is-name=true
// nest-host plain: self
// nest-host NESTMATE: L5W85HiddenClasses
// classData: data
// classData wrong type: java.lang.ClassCastException
// classData none: null
// array-type: true
// non-strong unloaded: true
// strong unloaded: false
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodHandles.Lookup.ClassOption;
import java.lang.ref.WeakReference;

public class L5W85HiddenClasses {
    /** A version-69 public class `name` with no members. */
    static byte[] classFile(String name) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bos);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(69);
        out.writeShort(5);
        out.writeByte(1);
        out.writeUTF(name);
        out.writeByte(7);
        out.writeShort(1);
        out.writeByte(1);
        out.writeUTF("java/lang/Object");
        out.writeByte(7);
        out.writeShort(3);
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bos.toByteArray();
    }

    static boolean unloads(Lookup lookup, byte[] bytes, ClassOption... options) throws Exception {
        WeakReference<Class<?>> ref = new WeakReference<>(lookup.defineHiddenClass(bytes, false, options).lookupClass());
        for (int i = 0; i < 50 && ref.get() != null; i++) {
            System.gc();
            Thread.sleep(10);
        }
        return ref.get() == null;
    }

    public static void main(String[] args) throws Exception {
        Lookup lookup = MethodHandles.lookup();
        byte[] bytes = classFile("HiddenTarget");

        Class<?> h = lookup.defineHiddenClass(bytes, false).lookupClass();
        String name = h.getName();
        System.out.println("name-shape: " + name.matches("HiddenTarget/0x[0-9a-f]{16}")
                + " hidden=" + h.isHidden()
                + " canonical=" + h.getCanonicalName()
                + " simple-has-suffix=" + h.getSimpleName().startsWith("HiddenTarget/0x"));

        try {
            Class.forName(name);
            System.out.println("forName: found");
        } catch (ClassNotFoundException e) {
            System.out.println("forName: " + e.getClass().getName() + " message-is-name=" + name.equals(e.getMessage()));
        }

        System.out.println("nest-host plain: " + (h.getNestHost() == h ? "self" : h.getNestHost().getName()));
        Class<?> n = lookup.defineHiddenClass(bytes, false, ClassOption.NESTMATE).lookupClass();
        System.out.println("nest-host NESTMATE: " + n.getNestHost().getName());

        Lookup withData = lookup.defineHiddenClassWithClassData(bytes, "data", false);
        System.out.println("classData: " + MethodHandles.classData(withData, "_", String.class));
        try {
            Integer wrong = MethodHandles.classData(withData, "_", Integer.class);
            System.out.println("classData wrong type: " + wrong);
        } catch (ClassCastException e) {
            System.out.println("classData wrong type: " + e.getClass().getName());
        }
        Lookup noData = lookup.defineHiddenClass(bytes, false);
        System.out.println("classData none: " + MethodHandles.classData(noData, "_", Object.class));

        System.out.println("array-type: " + h.arrayType().getName().startsWith("[LHiddenTarget/0x"));

        System.out.println("non-strong unloaded: " + unloads(lookup, bytes));
        System.out.println("strong unloaded: " + unloads(lookup, bytes, ClassOption.STRONG));
    }
}
