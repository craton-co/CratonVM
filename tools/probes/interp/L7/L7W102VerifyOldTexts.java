// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L7: a class file below major 50 is
// verified by libverify's inference verifier on HotSpot, whose VerifyError
// message is `(class: p/C, method: m signature: (..)V) <reason>` (check_code.c
// `print_CCerror_info` + `CCerror`), not the split verifier's `Exception
// Details:` block and not our `C.m: at bytecode offset N: ...`. One major-49
// class per case, defined by a custom loader and initialized; the message is
// printed line by line between `|` marks.
// Expected HotSpot 25 output (reasons from check_code.c, jdk-25+36):
//   int-as-object       Expecting to find object/array on stack
//   float-into-iadd     Expecting to find integer on stack
//   pop-empty           Unable to pop operand off an empty stack
//   iload-unset         Accessing value from uninitialized register 0
//   overflow            Stack size too large
//   ireturn-in-void     Wrong return type in function
//   return-in-int       Wrong return type in function
//   falls-off-end       Falling off the end of the code
//   merge-int-null      Mismatched stack types
//   init-no-super       Constructor must call super() or this()
//   athrow-string       Can only throw Throwable objects
//   iaload-float-array  Expecting to find array of ints on stack
// each as `|(class: p/OldN, method: <m> signature: <sig>) <reason>|`.
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W102VerifyOldTexts   (both modes).
public class L7W102VerifyOldTexts {

    // ---- a minimal class-file writer: one method, any major version and
    // access flags. Hand-assembled so max_stack / max_locals are exactly what
    // the case says and the constant-pool indices printed are fixed.
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

        int str(String s) throws java.io.IOException {
            int u = utf8(s);
            cp.writeByte(8);
            cp.writeShort(u);
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

        /** One method with `access` flags; no StackMapTable, no handlers. */
        byte[] build(String className, int access, String name, String desc, int maxStack,
                int maxLocals, byte[] code) throws java.io.IOException {
            int thisC = cls(className);
            int superC = cls("java/lang/Object");
            int codeU = utf8("Code");
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
            out.writeShort(access);
            out.writeShort(nameU);
            out.writeShort(descU);
            out.writeShort(1);
            out.writeShort(codeU);
            out.writeInt(12 + code.length);
            out.writeShort(maxStack);
            out.writeShort(maxLocals);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(0); // exception table
            out.writeShort(0); // code attributes
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

    static final int STATIC = 0x09;
    static final int INSTANCE = 0x01;

    public static void main(String[] args) throws Exception {
        final int major = 49;
        Asm a = new Asm(major);
        int valueOf = a.methodref("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;");
        // iconst_0; invokestatic String.valueOf(Object); pop; return
        run("int-as-object", "p.Old1", a.build("p/Old1", STATIC, "run", "()V", 1, 0,
                b(0x03, 0xb8, valueOf >> 8, valueOf & 0xff, 0x57, 0xb1)));
        // fconst_0; iconst_0; iadd; pop; return
        run("float-into-iadd", "p.Old2", new Asm(major).build("p/Old2", STATIC, "run", "()V", 2, 0,
                b(0x0b, 0x03, 0x60, 0x57, 0xb1)));
        // pop; return
        run("pop-empty", "p.Old3", new Asm(major).build("p/Old3", STATIC, "run", "()V", 1, 0,
                b(0x57, 0xb1)));
        // iload_0; pop; return  (no argument, nothing stored in local 0)
        run("iload-unset", "p.Old4", new Asm(major).build("p/Old4", STATIC, "run", "()V", 1, 1,
                b(0x1a, 0x57, 0xb1)));
        // iconst_0; iconst_0; pop2; return  with max_stack 1
        run("overflow", "p.Old5", new Asm(major).build("p/Old5", STATIC, "run", "()V", 1, 0,
                b(0x03, 0x03, 0x58, 0xb1)));
        // iconst_0; ireturn  in a void method
        run("ireturn-in-void", "p.Old6", new Asm(major).build("p/Old6", STATIC, "run", "()V", 1, 0,
                b(0x03, 0xac)));
        // return  in an int method
        run("return-in-int", "p.Old7", new Asm(major).build("p/Old7", STATIC, "run", "()I", 1, 0,
                b(0xb1)));
        // iconst_0; pop  -- no return
        run("falls-off-end", "p.Old8", new Asm(major).build("p/Old8", STATIC, "run", "()V", 1, 0,
                b(0x03, 0x57)));
        // iconst_0; ifeq 8; aconst_null; goto 9; 8: iconst_0; 9: astore_1; return
        run("merge-int-null", "p.Old9", new Asm(major).build("p/Old9", STATIC, "run", "()V", 1, 2,
                b(0x03, 0x99, 0x00, 0x07, 0x01, 0xa7, 0x00, 0x04, 0x03, 0x4c, 0xb1)));
        // <init>()V { return; }  -- no super()
        run("init-no-super", "p.Old10", new Asm(major).build("p/Old10", INSTANCE, "<init>", "()V", 0, 1,
                b(0xb1)));
        Asm t = new Asm(major);
        int s = t.str("x");
        // ldc "x"; athrow
        run("athrow-string", "p.Old11", t.build("p/Old11", STATIC, "run", "()V", 1, 0,
                b(0x12, s, 0xbf)));
        // iconst_1; newarray float; iconst_0; iaload; pop; return
        run("iaload-float-array", "p.Old12", new Asm(major).build("p/Old12", STATIC, "run", "()V", 2, 0,
                b(0x04, 0xbc, 0x06, 0x03, 0x2e, 0x57, 0xb1)));
    }
}
