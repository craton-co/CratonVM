// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 105, lane L7 (i103-L7-jsr-methods-are-not-type-checked residues):
// libverify (check_code.c) keys its states by instruction only, so a subroutine called from
// top-level code AND from inside another subroutine is checked once, its registers merged over
// both callers (R4); `verify_subroutines.rs` now does the same. The merged mask list is the
// intersection [B], and a `ret` returns with its own list cut below the subroutine, so after B
// returns into A the list is empty and A's `ret` is orphaned (R5, measured on HotSpot 25 in the
// wave-105 hand-back). R1-R3 are the first
// measurement of `ret`'s register texts and of "Illegal return from subroutine" (the texts
// are from check_code.c as remembered; if HotSpot prints something else, the HotSpot line is
// the truth and the lane fixes the text).
//
// No arguments, no setup:
//   javac -d out L7W105JsrPcKeyedMerge.java
//   cratonvm -cp out L7W105JsrPcKeyedMerge
//   cratonvm --compatible -cp out L7W105JsrPcKeyedMerge
//   (with CRATONVM_VERIFY_SUBROUTINES=0 every row differs: the walk is off)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; libverify texts as remembered, first measurement)
// == R1 ret on an int register (49)
// threw java.lang.VerifyError
// |(class: p/R1, method: run signature: ()V) Register 0 contains wrong type|
// == R2 ret on a register never written (49)
// threw java.lang.VerifyError
// |(class: p/R2, method: run signature: ()V) Accessing value from uninitialized register 0|
// == R3 ret reached again outside its subroutine (49)
// threw java.lang.VerifyError
// |(class: p/R3, method: run signature: ()V) Illegal return from subroutine|
// == R4 one subroutine called from top level and from another subroutine (49)
// threw java.lang.VerifyError
// |(class: p/R4, method: run signature: ()V) Register 1 contains wrong type|
// == R5 same shape, the shared subroutine leaves the register alone (49)
// threw java.lang.VerifyError
// |(class: p/R5, method: run signature: ()V) Illegal return from subroutine|
// == R6 nested subroutine returns into its caller (49)
// initialized
// HOTSPOT_EXPECTED_END
public class L7W105JsrPcKeyedMerge {

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

        byte[] build(int major, String className, int maxStack, int maxLocals, byte[] code) throws java.io.IOException {
            int thisC = cls(className);
            int superC = cls("java/lang/Object");
            int codeU = utf8("Code");
            int nameU = utf8("run");
            int descU = utf8("()V");
            java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
            java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(major);
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

    static void run(String label, String name, int maxStack, int maxLocals, byte[] code) throws Exception {
        System.out.println("== " + label);
        Loader l = new Loader();
        l.classes.put(name, new Asm().build(49, name.replace('.', '/'), maxStack, maxLocals, code));
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
        // 0: iconst_0; 1: istore_0; 2: ret 0
        run("R1 ret on an int register (49)", "p.R1", 1, 1, new byte[] {0x03, 0x3b, (byte) 0xa9, 0x00});
        // 0: ret 0 (static, no arguments: no register written)
        run("R2 ret on a register never written (49)", "p.R2", 1, 1, new byte[] {(byte) 0xa9, 0x00});
        // 0: jsr 6; 3: goto 7 (to the subroutine's ret, outside the subroutine); 6: astore_0; 7: ret 0
        run("R3 ret reached again outside its subroutine (49)", "p.R3", 1, 1, new byte[] {
            (byte) 0xa8, 0x00, 0x06, (byte) 0xa7, 0x00, 0x04, 0x4b, (byte) 0xa9, 0x00});
        // 0: iconst_0; 1: istore_1; 2: jsr 17 (B); 5: jsr 9 (A); 8: return;
        // A 9: astore_2; 10: aconst_null; 11: astore_1; 12: jsr 17 (B, from inside A); 15: ret 2;
        // B 17: astore_3; 18: iload_1 (int from the top-level call, null from A's); 19: pop; 20: ret 3
        run("R4 one subroutine called from top level and from another subroutine (49)", "p.R4", 1, 4, new byte[] {
            0x03, 0x3c, (byte) 0xa8, 0x00, 0x0f, (byte) 0xa8, 0x00, 0x04, (byte) 0xb1,
            0x4d, 0x01, 0x4c, (byte) 0xa8, 0x00, 0x05, (byte) 0xa9, 0x02,
            0x4e, 0x1b, 0x57, (byte) 0xa9, 0x03});
        // R4 with 18: nop; 19: nop
        run("R5 same shape, the shared subroutine leaves the register alone (49)", "p.R5", 1, 4, new byte[] {
            0x03, 0x3c, (byte) 0xa8, 0x00, 0x0f, (byte) 0xa8, 0x00, 0x04, (byte) 0xb1,
            0x4d, 0x01, 0x4c, (byte) 0xa8, 0x00, 0x05, (byte) 0xa9, 0x02,
            0x4e, 0x00, 0x00, (byte) 0xa9, 0x03});
        // javac's nested finally: 0: jsr 4 (A); 3: return; A 4: astore_1; 5: jsr 10 (B, only
        // from inside A); 8: ret 1; B 10: astore_2; 11: ret 2
        run("R6 nested subroutine returns into its caller (49)", "p.R6", 1, 3, new byte[] {
            (byte) 0xa8, 0x00, 0x04, (byte) 0xb1,
            0x4c, (byte) 0xa8, 0x00, 0x05, (byte) 0xa9, 0x01,
            0x4d, (byte) 0xa9, 0x02});
    }
}
