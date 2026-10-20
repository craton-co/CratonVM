// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L7: a `checkcast` of an instance of a hidden class
// (Lookup.defineHiddenClass, not a lambda) named the class with its `/0x<hex>` tail dotted
// (`L7W104H.0x...`), where HotSpot's Klass::external_name() keeps it (`L7W104H/0x...`); the
// message funnel then could not match the class to its name either. The interpreter's
// checkcast, the JIT's cast helper and the funnel now keep the tail.
// The hex differs per run and per VM, so it is printed masked as `/0x<hex>`.
// The cast runs 3000 times so a JIT arm reaches compiled code; every round must give the
// first round's text ("rounds agree: true").
// Expected HotSpot 25 output:
//   name L7W104H/0x<hex>
//   cast class L7W104H/0x<hex> cannot be cast to class java.lang.String (L7W104H/0x<hex> is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
//   rounds agree: true
//   class-cast Cannot cast L7W104H/0x<hex> to java.lang.String
//   array-store L7W104H/0x<hex>
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W104HiddenClassCastText
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;

public class L7W104HiddenClassCastText {

    /** `public class L7W104H { public L7W104H() { super(); } }`, major 52. */
    static byte[] hiddenBytes() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(11);
        out.writeByte(1); out.writeUTF("L7W104H");            // 1
        out.writeByte(7); out.writeShort(1);                   // 2 this
        out.writeByte(1); out.writeUTF("java/lang/Object");   // 3
        out.writeByte(7); out.writeShort(3);                   // 4 super
        out.writeByte(1); out.writeUTF("<init>");             // 5
        out.writeByte(1); out.writeUTF("()V");                // 6
        out.writeByte(12); out.writeShort(5); out.writeShort(6); // 7 NameAndType
        out.writeByte(10); out.writeShort(4); out.writeShort(7); // 8 Object.<init>
        out.writeByte(1); out.writeUTF("Code");               // 9
        out.writeByte(1); out.writeUTF("unused");             // 10
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(1);
        out.writeShort(0x01);
        out.writeShort(5);
        out.writeShort(6);
        out.writeShort(1);
        out.writeShort(9);
        byte[] code = {0x2a, (byte) 0xb7, 0x00, 0x08, (byte) 0xb1};
        out.writeInt(12 + code.length);
        out.writeShort(1);
        out.writeShort(1);
        out.writeInt(code.length);
        out.write(code);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        return bytes.toByteArray();
    }

    static String mask(String s) {
        return s == null ? "null" : s.replaceAll("/0x[0-9a-fA-F]+", "/0x<hex>");
    }

    static String cast(Object o) {
        try {
            return "ok " + ((String) o).length();
        } catch (ClassCastException e) {
            return mask(e.getMessage());
        }
    }

    public static void main(String[] args) throws Throwable {
        Class<?> hc = MethodHandles.lookup().defineHiddenClass(hiddenBytes(), true).lookupClass();
        Object o = hc.getDeclaredConstructor().newInstance();
        System.out.println("name " + mask(hc.getName()));
        String first = cast(o);
        System.out.println("cast " + first);
        boolean agree = true;
        for (int i = 0; i < 3000; i++) {
            agree &= first.equals(cast(o));
        }
        System.out.println("rounds agree: " + agree);
        try {
            String.class.cast(o);
            System.out.println("class-cast ok");
        } catch (ClassCastException e) {
            System.out.println("class-cast " + mask(e.getMessage()));
        }
        Object[] arr = new String[1];
        try {
            arr[0] = o;
            System.out.println("array-store ok");
        } catch (ArrayStoreException e) {
            System.out.println("array-store " + mask(e.getMessage()));
        }
    }
}
