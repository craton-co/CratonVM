// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L7 (i101-L7-verifyerror-texts-are-not-hotspots,
// ordering residue): HotSpot checks a method's exception-handler catch types
// (verify_exception_handler_table; libverify's initialize_exception_table) BEFORE it
// walks the method's code. A method whose handler catches java.lang.String AND whose
// code has a type error is refused for the catch type. We refused the type error
// (Pass 3 ran before the link's catch-type loads).
// Each case: define, then Class.forName(n, true, l); the message is printed line by
// line between `|` marks.
// Expected HotSpot 25 output (the orchestrator records it; the constant-pool index of
// java/lang/String is the probe's own, printed as `#cp=` so it can be checked):
//   == C1 split (52)
//   #cp=2
//   threw java.lang.VerifyError
//   |Catch type is not a subclass of Throwable in exception handler 5|
//   |Exception Details:|
//   |  Location:|
//   |    p/C1.run()V @5: return|
//   |  Reason:|
//   |    Type 'java/lang/String' (constant pool 2) is not assignable to 'java/lang/Throwable'|
//   |  Bytecode:|
//   |    0000000: 03b8 0006 57b1                         |
//   |  Exception Handler Table:|
//   |    bci [0, 4] => handler: 5|
//   || (trailing empty line)
//   == C2 libverify (49)
//   #cp=2
//   threw java.lang.VerifyError
//   |(class: p/C2, method: run signature: ()V) catch_type not a subclass of Throwable|
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W103CatchTypeBeforeCode   (both modes).
public class L7W103CatchTypeBeforeCode {

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

        /** One `public static run()V`; handlers are {start, end, handler, catchType}. */
        byte[] build(String className, int maxStack, int maxLocals, byte[] code, int[][] handlers)
                throws java.io.IOException {
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

    static void run(String label, String name, int major) throws Exception {
        System.out.println("== " + label);
        Asm a = new Asm(major);
        int valueOf = a.methodref("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;");
        int string = a.cls("java/lang/String");
        System.out.println("#cp=" + string);
        // 0: iconst_0; 1: invokestatic String.valueOf(Object) (type error); 4: pop; 5: return
        byte[] code = {0x03, (byte) 0xb8, (byte) (valueOf >> 8), (byte) valueOf, 0x57, (byte) 0xb1};
        byte[] bytes = a.build(name.replace('.', '/'), 1, 0, code, new int[][] {{0, 4, 5, string}});
        Loader l = new Loader();
        l.classes.put(name, bytes);
        try {
            Class.forName(name, true, l);
            System.out.println("initialized");
        } catch (Throwable t) {
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
    }

    public static void main(String[] args) throws Exception {
        run("C1 split (52)", "p.C1", 52);
        run("C2 libverify (49)", "p.C2", 49);
    }
}
