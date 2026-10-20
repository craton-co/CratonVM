// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L7: a handler whose catch type is not a
// Throwable. HotSpot refuses it while verifying the method
// (`verify_exception_handler_table`, before the bytecode walk) with the split
// verifier's header and `Exception Details:` -- Location at the handler,
// Reason "Type 'java/lang/String' (constant pool N) is not assignable to
// 'java/lang/Throwable'", no frame, the bytecode, the handler table and the
// stack map table -- and, below major 50, with libverify's
// "(class: p/Catch2, method: run signature: ()V) catch_type not a subclass of
// Throwable". We raised "p/Catch1.run: Catch type is not a subclass of
// Throwable in exception handler 2" (our prefix, no details).
//   major-52: |Catch type is not a subclass of Throwable in exception handler 2|
//             |Exception Details:| ... (orchestrator records HotSpot's lines)
//   major-49: |(class: p/Catch2, method: run signature: ()V) catch_type not a subclass of Throwable|
// Code: 0: nop; 1: return; 2: pop; 3: return, handler [0, 1) => 2 catching
// java/lang/String (major 52: same_locals_1_stack_item_frame(@2, String)).
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W102VerifyCatchTypeText   (both modes).
public class L7W102VerifyCatchTypeText {

    static final class Asm {
        final java.io.ByteArrayOutputStream cpBytes = new java.io.ByteArrayOutputStream();
        final java.io.DataOutputStream cp = new java.io.DataOutputStream(cpBytes);
        final java.util.Map<String, Integer> idx = new java.util.HashMap<>();
        int next = 1;
        int major = 52;

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

        int fieldref(String owner, String name, String desc) throws java.io.IOException {
            int c = cls(owner);
            int n = utf8(name);
            int d = utf8(desc);
            cp.writeByte(12);
            cp.writeShort(n);
            cp.writeShort(d);
            int nat = next++;
            cp.writeByte(9);
            cp.writeShort(c);
            cp.writeShort(nat);
            return next++;
        }

        /** One method: name, descriptor, max_stack, max_locals, code, StackMapTable bytes or null. */
        byte[] build(String className, String name, String desc, int maxStack, int maxLocals,
                byte[] code, byte[] stackMap) throws java.io.IOException {
            return build(className, name, desc, maxStack, maxLocals, code, stackMap, new int[0][]);
        }

        /** As above, with exception-table entries {start, end, handler, catchType}. */
        byte[] build(String className, String name, String desc, int maxStack, int maxLocals,
                byte[] code, byte[] stackMap, int[][] handlers) throws java.io.IOException {
            int thisC = cls(className);
            int superC = cls("java/lang/Object");
            int codeU = utf8("Code");
            int smtU = stackMap == null ? 0 : utf8("StackMapTable");
            int nameU = utf8(name);
            int descU = utf8(desc);
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
            out.writeShort(0); // interfaces
            out.writeShort(0); // fields
            out.writeShort(1); // methods
            out.writeShort(0x09); // public static
            out.writeShort(nameU);
            out.writeShort(descU);
            out.writeShort(1);
            out.writeShort(codeU);
            int smtLen = stackMap == null ? 0 : 6 + stackMap.length;
            out.writeInt(12 + code.length + 8 * handlers.length + smtLen);
            out.writeShort(maxStack);
            out.writeShort(maxLocals);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(handlers.length); // exception table
            for (int[] h : handlers) {
                for (int v : h) {
                    out.writeShort(v);
                }
            }
            out.writeShort(stackMap == null ? 0 : 1);
            if (stackMap != null) {
                out.writeShort(smtU);
                out.writeInt(stackMap.length);
                out.write(stackMap);
            }
            out.writeShort(0); // class attributes
            return bytes.toByteArray();
        }
    }

    static final class Loader extends ClassLoader {
        final String name;
        final byte[] bytes;

        Loader(String name, byte[] bytes) {
            super(Loader.class.getClassLoader());
            this.name = name;
            this.bytes = bytes;
        }

        @Override
        protected Class<?> findClass(String n) throws ClassNotFoundException {
            if (n.equals(name)) {
                return defineClass(n, bytes, 0, bytes.length);
            }
            throw new ClassNotFoundException(n);
        }
    }

    static byte[] b(int... v) {
        byte[] r = new byte[v.length];
        for (int i = 0; i < v.length; i++) {
            r[i] = (byte) v[i];
        }
        return r;
    }

    /** Define and initialize (= link and verify) the class; print what is thrown, line by line. */
    static void run(String label, String binaryName, byte[] bytes) {
        System.out.println("== " + label);
        try {
            Class.forName(binaryName, true, new Loader(binaryName, bytes));
            System.out.println("initialized");
        } catch (Throwable t) {
            System.out.println("threw " + t.getClass().getName());
            String m = t.getMessage();
            if (m == null) {
                System.out.println("message null");
            } else {
                for (String line : m.split("\n", -1)) {
                    System.out.println("|" + line + "|");
                }
            }
        }
    }



    public static void main(String[] args) throws Exception {
        Asm a = new Asm();
        int string = a.cls("java/lang/String");
        // same_locals_1_stack_item_frame at offset 2 (frame type 64 + 2),
        // stack item Object(String).
        byte[] smt = b(0x00, 0x01, 66, 0x07, string >> 8, string & 0xff);
        run("major-52", "p.Catch1", a.build("p/Catch1", "run", "()V", 1, 0,
                b(0x00, 0xb1, 0x57, 0xb1), smt, new int[][] {{0, 1, 2, string}}));

        Asm o = new Asm();
        o.major = 49;
        int string49 = o.cls("java/lang/String");
        run("major-49", "p.Catch2", o.build("p/Catch2", "run", "()V", 1, 0,
                b(0x00, 0xb1, 0x57, 0xb1), null, new int[][] {{0, 1, 2, string49}}));
    }
}
