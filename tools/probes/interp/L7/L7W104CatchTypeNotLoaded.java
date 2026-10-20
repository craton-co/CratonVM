// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L7 (i101-L7-verifyerror-texts-are-not-hotspots, residue):
// HotSpot LOADS each catch type while checking the handler table, before the method's code.
// A catch type that is not loaded yet when the class links (a user-loader class, verified at
// link) used to be judged only after the walk refused the code's type error; the link step now
// asks the catch-type step first when the walk refuses (vm_util.rs, link_claimed_class).
// Each case: the class and its catch type come from a fresh user loader; the catch type is NOT
// loaded before Class.forName(n, true, l). The message is printed line by line between `|`.
// Expected HotSpot 25 output (the orchestrator records it; `#cp=` is the catch type's index):
//   == D1 missing catch type (52)
//   #cp=2
//   threw java.lang.NoClassDefFoundError
//   |p/Missing1|
//   == D2 missing catch type (49)
//   #cp=2
//   threw java.lang.NoClassDefFoundError
//   |p/Missing2|
//   == D3 unloaded non-Throwable catch type (52)
//   #cp=2
//   threw java.lang.VerifyError
//   |Catch type is not a subclass of Throwable in exception handler 5|
//   |Exception Details:|
//   |  Location:|
//   |    p/D3.run()V @5: return|
//   |  Reason:|
//   |    Type 'p/Plain3' (constant pool 2) is not assignable to 'java/lang/Throwable'|
//   |  Bytecode:|
//   |    0000000: 03b8 0008 57b1                         |
//   |  Exception Handler Table:|
//   |    bci [0, 4] => handler: 5|
//   ||
//   == D4 unloaded non-Throwable catch type (49)
//   #cp=2
//   threw java.lang.VerifyError
//   |(class: p/D4, method: run signature: ()V) catch_type not a subclass of Throwable|
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W104CatchTypeNotLoaded   (both modes).
public class L7W104CatchTypeNotLoaded {

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

    /** A plain `public class <name> extends Object` with no members. */
    static byte[] plain(String internal) throws java.io.IOException {
        Asm a = new Asm(49);
        int thisC = a.cls(internal);
        int superC = a.cls("java/lang/Object");
        java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(a.next);
        a.cp.flush();
        out.write(a.cpBytes.toByteArray());
        out.writeShort(0x21);
        out.writeShort(thisC);
        out.writeShort(superC);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        return bytes.toByteArray();
    }

    static void run(String label, String name, int major, String catchType, boolean define) throws Exception {
        System.out.println("== " + label);
        Asm a = new Asm(major);
        // The catch type first, so its index is 2 in every case.
        int catchC = a.cls(catchType);
        int valueOf = a.methodref("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;");
        System.out.println("#cp=" + catchC);
        // 0: iconst_0; 1: invokestatic String.valueOf(Object) (type error); 4: pop; 5: return
        byte[] code = {0x03, (byte) 0xb8, (byte) (valueOf >> 8), (byte) valueOf, 0x57, (byte) 0xb1};
        byte[] bytes = a.build(name.replace('.', '/'), 1, 0, code, new int[][] {{0, 4, 5, catchC}});
        Loader l = new Loader();
        l.classes.put(name, bytes);
        if (define) {
            l.classes.put(catchType.replace('/', '.'), plain(catchType));
        }
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
        run("D1 missing catch type (52)", "p.D1", 52, "p/Missing1", false);
        run("D2 missing catch type (49)", "p.D2", 49, "p/Missing2", false);
        run("D3 unloaded non-Throwable catch type (52)", "p.D3", 52, "p/Plain3", true);
        run("D4 unloaded non-Throwable catch type (49)", "p.D4", 49, "p/Plain4", true);
    }
}
