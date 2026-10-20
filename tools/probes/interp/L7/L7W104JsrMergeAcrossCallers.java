// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L7 (i103-L7-jsr-methods-are-not-type-checked residue):
// libverify (check_code.c) merges a subroutine's registers over ALL its callers and, at each
// `ret`, restores the registers the subroutine did not modify from the matching `jsr`; a
// major-50 class that both verifiers refuse reports the split verifier's message.
// `verify_subroutines.rs` now walks the same way (CRATONVM_VERIFY_SUBROUTINES, default on).
// Expected HotSpot 25 output (K1/K2 from libverify's merge rules; K3/K4 texts are from
// check_code.c as remembered and are the first measurement of them; K5's Bytecode dump lines
// are whatever HotSpot prints, the orchestrator records them):
//   == K1 callers disagree on a register the subroutine reads (49)
//   threw java.lang.VerifyError
//   |(class: p/K1, method: run signature: ()V) Register 1 contains wrong type|
//   == K2 callers disagree, subroutine leaves the register alone (49)
//   initialized
//   == K3 recursive jsr (49)
//   threw java.lang.VerifyError
//   |(class: p/K3, method: run signature: ()V) Recursive call to jsr entry|
//   == K4 ret of a subroutine that already returned (49)
//   threw java.lang.VerifyError
//   |(class: p/K4, method: run signature: ()V) Multiple returns to single jsr|
//   == K5 type error in a subroutine (50: the split verifier's message)
//   threw java.lang.VerifyError
//   |Bad instruction: a8|
//   |Exception Details:|
//   |  Location:|
//   |    p/K5.run()V @0: jsr|
//   |  Reason:|
//   |    Error exists in the bytecode|
//   |  Bytecode:|
//   |    0000000: ...|
//   == K6 well-typed subroutine (50: fails over to libverify, which passes)
//   initialized
//   == K7 sibling of a subroutine method (49)
//   threw java.lang.VerifyError
//   |(class: p/K7, method: bad signature: ()V) Expecting to find object/array on stack|
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W104JsrMergeAcrossCallers   (both modes; with
// CRATONVM_VERIFY_SUBROUTINES=0 at least K1 and K5 print `initialized`).
public class L7W104JsrMergeAcrossCallers {

    static final class Asm {
        final java.io.ByteArrayOutputStream cpBytes = new java.io.ByteArrayOutputStream();
        final java.io.DataOutputStream cp = new java.io.DataOutputStream(cpBytes);
        final java.util.Map<String, Integer> idx = new java.util.HashMap<>();
        int next = 1;
        /** A second method `bad()V` when non-null (K7). */
        byte[] sibling;

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

        byte[] build(int major, String className, int maxStack, int maxLocals, byte[] code) throws java.io.IOException {
            int thisC = cls(className);
            int superC = cls("java/lang/Object");
            int codeU = utf8("Code");
            int nameU = utf8("run");
            int descU = utf8("()V");
            int badU = utf8("bad");
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
            out.writeShort(sibling == null ? 1 : 2);
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
            if (sibling != null) {
                // `public static bad()V`, no subroutine.
                out.writeShort(0x09);
                out.writeShort(badU);
                out.writeShort(descU);
                out.writeShort(1);
                out.writeShort(codeU);
                out.writeInt(12 + sibling.length);
                out.writeShort(1);
                out.writeShort(0);
                out.writeInt(sibling.length);
                out.write(sibling);
                out.writeShort(0);
                out.writeShort(0);
            }
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

    /** The `bad()V` method of the next class built (K7), from the valueOf index. */
    static java.util.function.IntFunction<byte[]> nextSibling;

    static void run(String label, int major, String name, int maxStack, int maxLocals, java.util.function.IntFunction<byte[]> code)
            throws Exception {
        System.out.println("== " + label);
        Asm a = new Asm();
        int valueOf = a.methodref("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;");
        if (nextSibling != null) {
            a.sibling = nextSibling.apply(valueOf);
            nextSibling = null;
        }
        Loader l = new Loader();
        l.classes.put(name, a.build(major, name.replace('.', '/'), maxStack, maxLocals, code.apply(valueOf)));
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
        // 0: iconst_0; 1: istore_1; 2: jsr 11; 5: aconst_null; 6: astore_1; 7: jsr 11; 10: return;
        // 11: astore_2; 12: iload_1 (int from one caller, null from the other); 13: pop; 14: ret 2
        run("K1 callers disagree on a register the subroutine reads (49)", 49, "p.K1", 1, 3, v -> new byte[] {
            0x03, 0x3c, (byte) 0xa8, 0x00, 0x09, 0x01, 0x4c, (byte) 0xa8, 0x00, 0x04, (byte) 0xb1,
            0x4d, 0x1b, 0x57, (byte) 0xa9, 0x02});
        // 0: iconst_0; 1: istore_1; 2: jsr 15; 5: iload_1; 6: pop; 7: aconst_null; 8: astore_1;
        // 9: jsr 15; 12: aload_1; 13: pop; 14: return; 15: astore_2; 16: ret 2
        run("K2 callers disagree, subroutine leaves the register alone (49)", 49, "p.K2", 1, 3, v -> new byte[] {
            0x03, 0x3c, (byte) 0xa8, 0x00, 0x0d, 0x1b, 0x57, 0x01, 0x4c, (byte) 0xa8, 0x00, 0x06,
            0x2b, 0x57, (byte) 0xb1, 0x4d, (byte) 0xa9, 0x02});
        // 0: jsr 4; 3: return; 4: astore_0; 5: jsr 4 (from inside itself); 8: ret 0
        run("K3 recursive jsr (49)", 49, "p.K3", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x04, (byte) 0xb1, 0x4b, (byte) 0xa8, (byte) 0xff, (byte) 0xff,
            (byte) 0xa9, 0x00});
        // 0: jsr 5; 3: ret 0 (the subroutine at 5 has returned); 5: astore_0; 6: ret 0
        run("K4 ret of a subroutine that already returned (49)", 49, "p.K4", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x05, (byte) 0xa9, 0x00, 0x4b, (byte) 0xa9, 0x00});
        // L7W103's J1 at major 50: 0: jsr 4; 3: return; 4: astore_0; 5: iconst_0;
        // 6: invokestatic String.valueOf(Object); 9: pop; 10: ret 0
        run("K5 type error in a subroutine (50: the split verifier's message)", 50, "p.K5", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x04, (byte) 0xb1, 0x4b, 0x03,
            (byte) 0xb8, (byte) (v >> 8), (byte) v, 0x57, (byte) 0xa9, 0x00});
        // L7W103's J2 at major 50: 0: jsr 4; 3: return; 4: astore_0; 5: ret 0
        run("K6 well-typed subroutine (50: fails over to libverify, which passes)", 50, "p.K6", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x04, (byte) 0xb1, 0x4b, (byte) 0xa9, 0x00});
        // A well-typed subroutine method plus a sibling `bad()V` with a type error:
        // 0: iconst_0; 1: invokestatic String.valueOf(Object); 4: pop; 5: return
        // (the sibling's refusal is re-diagnosed with libverify's text, wave 103 item 3).
        nextSibling = v -> new byte[] {0x03, (byte) 0xb8, (byte) (v >> 8), (byte) v, 0x57, (byte) 0xb1};
        run("K7 sibling of a subroutine method (49)", 49, "p.K7", 1, 1, v -> new byte[] {
            (byte) 0xa8, 0x00, 0x04, (byte) 0xb1, 0x4b, (byte) 0xa9, 0x00});
    }
}
