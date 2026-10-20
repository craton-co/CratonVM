// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L7 (i103-L7-jsr-methods-are-not-type-checked):
// a major-49 method with a `jsr` subroutine is type-checked by HotSpot's libverify
// (check_code.c's subroutine handling). We accepted such a method on the structural scan
// alone; `verify_subroutines.rs` now walks it per calling context
// (CRATONVM_VERIFY_SUBROUTINES, default on; `=0` restores the old acceptance).
// Expected HotSpot 25 output (J1 measured on w103, Linux; J2-J4 from libverify's rules,
// the orchestrator records them):
//   == J1 type error in a subroutine (49)
//   threw java.lang.VerifyError
//   |(class: p/J1, method: run signature: ()V) Expecting to find object/array on stack|
//   == J2 well-typed subroutine (49)
//   initialized
//   == J3 two call sites (49)
//   initialized
//   == J4 register the subroutine modified (49)
//   threw java.lang.VerifyError
//   |(class: p/J4, method: run signature: ()V) Register 1 contains wrong type|
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W103JsrSubroutineTypeError   (both modes;
// with CRATONVM_VERIFY_SUBROUTINES=0, J1 and J4 print `initialized`).
public class L7W103JsrSubroutineTypeError {

    static final class Asm {
        final java.io.ByteArrayOutputStream cpBytes = new java.io.ByteArrayOutputStream();
        final java.io.DataOutputStream cp = new java.io.DataOutputStream(cpBytes);
        final java.util.Map<String, Integer> idx = new java.util.HashMap<>();
        int next = 1;

        int utf8(String s) throws java.io.IOException {
            Integer i = idx.get("U" + s);
            if (i != null) {
                return i;
            }
            cp.writeByte(1);
            cp.writeUTF(s);
            idx.put("U" + s, next);
            return next++;
        }

        int cls(String n) throws java.io.IOException {
            Integer i = idx.get("C" + n);
            if (i != null) {
                return i;
            }
            int u = utf8(n);
            cp.writeByte(7);
            cp.writeShort(u);
            idx.put("C" + n, next);
            return next++;
        }

        int methodref(String owner, String name, String desc) throws java.io.IOException {
            int c = cls(owner);
            int n = utf8(name);
            int d = utf8(desc);
            cp.writeByte(12);
            cp.writeShort(n);
            cp.writeShort(d);
            int nat = next++;
            cp.writeByte(10);
            cp.writeShort(c);
            cp.writeShort(nat);
            return next++;
        }

        byte[] build(String className, int maxStack, int maxLocals, byte[] code) throws java.io.IOException {
            int thisC = cls(className);
            int superC = cls("java/lang/Object");
            int codeU = utf8("Code");
            int nameU = utf8("run");
            int descU = utf8("()V");
            java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
            java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(49);
            out.writeShort(next);
            cp.flush();
            out.write(cpBytes.toByteArray());
            out.writeShort(0x21);
            out.writeShort(thisC);
            out.writeShort(superC);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(1);
            out.writeShort(0x09);
            out.writeShort(nameU);
            out.writeShort(descU);
            out.writeShort(1);
            out.writeShort(codeU);
            out.writeInt(12 + code.length);
            out.writeShort(maxStack);
            out.writeShort(maxLocals);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(0);
            return bytes.toByteArray();
        }
    }

    static final class Loader extends ClassLoader {
        final java.util.Map<String, byte[]> classes = new java.util.HashMap<>();

        Loader() {
            super(Loader.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] bytes = classes.get(n);
            if (bytes != null) {
                return defineClass(n, bytes, 0, bytes.length);
            }
            throw new ClassNotFoundException(n);
        }
    }

    static void run(String label, String name, int maxStack, int maxLocals, java.util.function.IntFunction<byte[]> code)
            throws Exception {
        System.out.println("== " + label);
        Asm a = new Asm();
        int valueOf = a.methodref("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;");
        Loader l = new Loader();
        l.classes.put(name, a.build(name.replace('.', '/'), maxStack, maxLocals, code.apply(valueOf)));
        try {
            Class.forName(name, true, l);
            System.out.println("initialized");
        } catch (Throwable t) {
            System.out.println("threw " + t.getClass().getName());
            String m = t.getMessage();
            if (m != null) {
                for (String line : m.split("\n", -1)) {
                    System.out.println("|" + line + "|");
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        // 0: jsr 4; 3: return; 4: astore_0; 5: iconst_0;
        // 6: invokestatic String.valueOf(Object) (an int where an Object is expected); 9: pop; 10: ret 0
        run("J1 type error in a subroutine (49)", "p.J1", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x04, (byte) 0xb1, 0x4b, 0x03,
            (byte) 0xb8, (byte) (v >> 8), (byte) v, 0x57, (byte) 0xa9, 0x00});
        // 0: jsr 4; 3: return; 4: astore_0; 5: ret 0
        run("J2 well-typed subroutine (49)", "p.J2", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x04, (byte) 0xb1, 0x4b, (byte) 0xa9, 0x00});
        // 0: jsr 7; 3: jsr 7; 6: return; 7: astore_0; 8: ret 0   (two call sites)
        run("J3 two call sites (49)", "p.J3", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x07, (byte) 0xa8, 0x00, 0x04, (byte) 0xb1, 0x4b, (byte) 0xa9, 0x00});
        // 0: jsr 6; 3: aload_1; 4: pop; 5: return; 6: astore_0; 7: iconst_0; 8: istore_1; 9: ret 0
        // (the subroutine leaves an int in register 1, the caller loads it as a reference)
        run("J4 register the subroutine modified (49)", "p.J4", 1, 2, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x06, 0x2b, 0x57, (byte) 0xb1, 0x4b, 0x03, 0x3c, (byte) 0xa9, 0x00});
    }
}
