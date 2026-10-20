// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L7: a split-verifier type error is
// HotSpot's "Bad type on operand stack" + `Exception Details:` block
// (Location / Reason / Current Frame / Bytecode), not our
// `C.m: at bytecode offset N: expected Int on stack, found ...`.
// Cases: an int passed as Object (invokestatic), an int receiver
// (invokevirtual), an int pair where a long is expected (long_2nd first, as
// HotSpot's pop_stack_2), a float into iadd.
// The message is printed line by line between `|` marks so trailing spaces
// (HotSpot pads the last `Bytecode:` line to sixteen byte columns) show.
// Expected output: HotSpot 25, default flags (the orchestrator records it).
// Run: cratonvm --java-home <jdk25> -cp <dir> <Name>   (both modes; JIT irrelevant).
public class L7W101VerifyBadTypeOnStack {

    // ---- a minimal class-file writer (major 52, one `static` method per
    // case). Hand-assembled rather than built with java.lang.classfile so
    // max_stack / max_locals / StackMapTable are exactly what the case says
    // (the ClassFile API recomputes them and refuses bad code), and so the
    // constant-pool indices printed in HotSpot's text are fixed.
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
            out.writeShort(52);
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
        int valueOf = a.methodref("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;");
        // iconst_0; invokestatic String.valueOf(Object); pop; return
        run("int-as-object", "p.BadType1", a.build("p/BadType1", "run", "()V", 1, 0,
                b(0x03, 0xb8, valueOf >> 8, valueOf & 0xff, 0x57, 0xb1), null));

        Asm c = new Asm();
        int length = c.methodref("java/lang/String", "length", "()I");
        // iconst_0; invokevirtual String.length()I; pop; return
        run("int-receiver", "p.BadType2", c.build("p/BadType2", "run", "()V", 1, 0,
                b(0x03, 0xb6, length >> 8, length & 0xff, 0x57, 0xb1), null));

        Asm d = new Asm();
        int abs = d.methodref("java/lang/Math", "abs", "(J)J");
        // iconst_0; iconst_0; invokestatic Math.abs(J)J; pop2; return
        run("ints-as-long", "p.BadType3", d.build("p/BadType3", "run", "()V", 2, 0,
                b(0x03, 0x03, 0xb8, abs >> 8, abs & 0xff, 0x58, 0xb1), null));

        // fconst_0; iconst_0; iadd; pop; return
        run("float-into-iadd", "p.BadType4", new Asm().build("p/BadType4", "run", "()V", 2, 0,
                b(0x0b, 0x03, 0x60, 0x57, 0xb1), null));
    }
}
