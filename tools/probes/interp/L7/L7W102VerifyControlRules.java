// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L7: split-verifier (major 52) rules this
// verifier skipped or reported in its own words.
//   ireturn-in-void      accepted before: "Method does not expect a return value"
//                        (Type integer (current frame, stack[0]) is not
//                        assignable to top (from method signature))
//   return-in-int        accepted before: "Method expects a return value"
//   float-ret-in-int     accepted before: "Bad return type" (float ... integer)
//   string-ret-integer   "Bad return type" ('java/lang/String' ... 'java/lang/Integer')
//   falls-off-end        accepted before: "Control flow falls through code end"
//                        (Location `@2: <invalid>`, no frame)
//   iaload-float-array   accepted before: "Bad type on operand stack in iaload"
//                        (Type '[F' (current frame, stack[0]) is not assignable to '[I')
//   baload-int-array     accepted before: "Bad type on operand stack in baload"
//                        (Invalid type: '[I' (current frame, stack[0]))
//   arraylength-string   "Bad type on operand stack in arraylength"
//                        (Invalid type: 'java/lang/String' (current frame, stack[0]))
//   aaload-int           accepted before (!): "Bad type on operand stack"
//                        (Type integer ... is not assignable to reference type)
//   init-no-super        "Constructor must call super() or this() before return"
//   athrow-string        "Bad type on operand stack" ('java/lang/String' ... 'java/lang/Throwable')
//   monitorenter-int     "Bad type on operand stack" (integer ... reference type)
// each with HotSpot's whole `Exception Details:` block, printed line by line
// between `|` marks. `CRATONVM_VERIFY_CONTROL_RULES=0` restores the old
// acceptance of the first, second, third, fifth, sixth and seventh cases.
// Expected output: HotSpot 25, default flags (the orchestrator records it).
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W102VerifyControlRules   (both modes).
public class L7W102VerifyControlRules {

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
        final int major = 52;
        // iconst_0; ireturn  in a void method
        run("ireturn-in-void", "p.Ctl1", new Asm(major).build("p/Ctl1", STATIC, "run", "()V", 1, 0,
                b(0x03, 0xac)));
        // return  in an int method
        run("return-in-int", "p.Ctl2", new Asm(major).build("p/Ctl2", STATIC, "run", "()I", 1, 0,
                b(0xb1)));
        // fconst_0; freturn  in an int method
        run("float-ret-in-int", "p.Ctl3", new Asm(major).build("p/Ctl3", STATIC, "run", "()I", 1, 0,
                b(0x0b, 0xae)));
        Asm t = new Asm(major);
        int s = t.str("x");
        // ldc "x"; areturn  in a method returning Integer
        run("string-ret-integer", "p.Ctl4", t.build("p/Ctl4", STATIC, "run", "()Ljava/lang/Integer;", 1, 0,
                b(0x12, s, 0xb0)));
        // iconst_0; pop  -- no return
        run("falls-off-end", "p.Ctl5", new Asm(major).build("p/Ctl5", STATIC, "run", "()V", 1, 0,
                b(0x03, 0x57)));
        // iconst_1; newarray float; iconst_0; iaload; pop; return
        run("iaload-float-array", "p.Ctl6", new Asm(major).build("p/Ctl6", STATIC, "run", "()V", 2, 0,
                b(0x04, 0xbc, 0x06, 0x03, 0x2e, 0x57, 0xb1)));
        // iconst_1; newarray int; iconst_0; baload; pop; return
        run("baload-int-array", "p.Ctl7", new Asm(major).build("p/Ctl7", STATIC, "run", "()V", 2, 0,
                b(0x04, 0xbc, 0x0a, 0x03, 0x33, 0x57, 0xb1)));
        Asm u = new Asm(major);
        int s2 = u.str("x");
        // ldc "x"; arraylength; pop; return
        run("arraylength-string", "p.Ctl8", u.build("p/Ctl8", STATIC, "run", "()V", 1, 0,
                b(0x12, s2, 0xbe, 0x57, 0xb1)));
        // iconst_0; iconst_0; aaload; pop; return
        run("aaload-int", "p.Ctl9", new Asm(major).build("p/Ctl9", STATIC, "run", "()V", 2, 0,
                b(0x03, 0x03, 0x32, 0x57, 0xb1)));
        // <init>()V { return; }  -- no super()
        run("init-no-super", "p.Ctl10", new Asm(major).build("p/Ctl10", INSTANCE, "<init>", "()V", 0, 1,
                b(0xb1)));
        Asm v = new Asm(major);
        int s3 = v.str("x");
        // ldc "x"; athrow
        run("athrow-string", "p.Ctl11", v.build("p/Ctl11", STATIC, "run", "()V", 1, 0,
                b(0x12, s3, 0xbf)));
        // iconst_0; monitorenter; return
        run("monitorenter-int", "p.Ctl12", new Asm(major).build("p/Ctl12", STATIC, "run", "()V", 1, 0,
                b(0x03, 0xc2, 0xb1)));
    }
}
