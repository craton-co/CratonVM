// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L7 (i102-L7-structural-scan-refuses-at-define):
// HotSpot's ClassFileParser checks only the class-file FORMAT inside defineClass
// ("Arguments can't fit into locals" is a ClassFormatError there); a branch out of
// the code, a handler inside an instruction or a local index past max_locals is its
// VERIFIER's, at the first link, in the verifier's words:
//   * split verifier (major 52): "Expecting a stackmap frame at branch target N" at
//     the jump, "Local variable table overflow" (Reason "Local index N is invalid"),
//     and a ClassFormatError "Illegal exception table handler_pc N in method '...'"
//     for a handler inside an instruction;
//   * libverify (major 49): "(class: p/X, method: run signature: ()V) Illegal target
//     of jump or branch" / "... Illegal local variable number".
// We refused all of them inside defineClass with our own texts.
// Each case: define (loadClass), then Class.forName(n, true, l); the message is
// printed line by line between `|` marks (trailing spaces of the Bytecode: line show).
// Expected HotSpot 25 output (first lines; the orchestrator records the rest):
//   == S1 goto past the end (52)
//   defined p.S1
//   threw java.lang.VerifyError
//   |Expecting a stackmap frame at branch target 100|
//   |Exception Details:|
//   |  Location:|
//   |    p/S1.run()V @0: goto|
//   |  Reason:|
//   |    Expected stackmap frame at this location.|
//   |  Bytecode:|
//   |    0000000: a700 64b1                              |
//   == S2 handler inside an instruction (52)
//   defined p.S2
//   threw java.lang.ClassFormatError
//   |Illegal exception table handler_pc 1 in method 'void p.S2.run()'|
//   == S3 istore past max_locals (52)
//   defined p.S3
//   threw java.lang.VerifyError
//   |Local variable table overflow|
//   |Exception Details:|
//   |  Location:|
//   |    p/S3.run()V @1: istore|
//   |  Reason:|
//   |    Local index 5 is invalid|
//   |  Bytecode:|
//   |    0000000: 0336 05b1                              |
//   == S4 goto past the end (49)
//   defined p.S4
//   threw java.lang.VerifyError
//   |(class: p/S4, method: run signature: ()V) Illegal target of jump or branch|
//   == S5 iload past max_locals (49)
//   defined p.S5
//   threw java.lang.VerifyError
//   |(class: p/S5, method: run signature: ()V) Illegal local variable number|
//   == S6 arguments do not fit (52)
//   threw java.lang.ClassFormatError: Arguments can't fit into locals in class file p/S6
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W103StructuralAtLink   (both modes;
// also with CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK=0, where S1-S5 throw at define).
public class L7W103StructuralAtLink {

    static final class Asm {
        final java.io.ByteArrayOutputStream cpBytes = new java.io.ByteArrayOutputStream();
        final java.io.DataOutputStream cp = new java.io.DataOutputStream(cpBytes);
        final java.util.Map<String, Integer> idx = new java.util.HashMap<>();
        int next = 1;
        final int major;

        Asm(int major) {
            this.major = major;
        }

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

        /** One `public static` method; handlers are {start, end, handler, catchType}. */
        byte[] build(String className, String desc, int maxStack, int maxLocals, byte[] code,
                int[][] handlers) throws java.io.IOException {
            int thisC = cls(className);
            int superC = cls("java/lang/Object");
            int codeU = utf8("Code");
            int nameU = utf8("run");
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
            out.writeInt(12 + code.length + 8 * handlers.length);
            out.writeShort(maxStack);
            out.writeShort(maxLocals);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(handlers.length);
            for (int[] h : handlers) {
                for (int v : h) {
                    out.writeShort(v);
                }
            }
            out.writeShort(0); // code attributes
            out.writeShort(0); // class attributes
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

    static byte[] b(int... v) {
        byte[] r = new byte[v.length];
        for (int i = 0; i < v.length; i++) {
            r[i] = (byte) v[i];
        }
        return r;
    }

    static void printThrown(Throwable t) {
        System.out.println("threw " + t.getClass().getName());
        String m = t.getMessage();
        if (m == null) {
            System.out.println("message null");
            return;
        }
        for (String line : m.split("\n", -1)) {
            System.out.println("|" + line + "|");
        }
    }

    static void run(String label, String name, byte[] bytes) {
        System.out.println("== " + label);
        Loader l = new Loader();
        l.classes.put(name, bytes);
        try {
            System.out.println("defined " + l.loadClass(name).getName());
        } catch (Throwable t) {
            System.out.println("threw " + t.getClass().getName() + ": " + t.getMessage());
            return;
        }
        try {
            Class.forName(name, true, l);
            System.out.println("initialized");
        } catch (Throwable t) {
            printThrown(t);
        }
    }

    public static void main(String[] args) throws Exception {
        // goto +100; return
        run("S1 goto past the end (52)", "p.S1",
                new Asm(52).build("p/S1", "()V", 0, 0, b(0xa7, 0x00, 0x64, 0xb1), new int[0][]));
        // sipush 1; pop; return -- a catch-all handler at 1, inside the sipush
        run("S2 handler inside an instruction (52)", "p.S2",
                new Asm(52).build("p/S2", "()V", 1, 0, b(0x11, 0x00, 0x01, 0x57, 0xb1),
                        new int[][] {{0, 4, 1, 0}}));
        // iconst_0; istore 5; return   (max_locals 1)
        run("S3 istore past max_locals (52)", "p.S3",
                new Asm(52).build("p/S3", "()V", 1, 1, b(0x03, 0x36, 0x05, 0xb1), new int[0][]));
        // goto +100; return
        run("S4 goto past the end (49)", "p.S4",
                new Asm(49).build("p/S4", "()V", 0, 0, b(0xa7, 0x00, 0x64, 0xb1), new int[0][]));
        // iload 5; pop; return   (max_locals 1)
        run("S5 iload past max_locals (49)", "p.S5",
                new Asm(49).build("p/S5", "()V", 1, 1, b(0x15, 0x05, 0x57, 0xb1), new int[0][]));
        // static run(J)V with max_locals 1: the long argument needs two slots
        run("S6 arguments do not fit (52)", "p.S6",
                new Asm(52).build("p/S6", "(J)V", 0, 1, b(0xb1), new int[0][]));
    }
}
