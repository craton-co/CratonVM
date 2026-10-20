// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L7: HotSpot verifies a class when it is
// LINKED (InstanceKlass::link_class_impl), not inside defineClass. A class
// with a type error, defined by a user-defined loader and never linked, raises
// nothing; `Class.forName(n, false, l)` does not link either; the first link
// (here getDeclaredMethods, which links, and Class.forName(n, true, l)) throws
// VerifyError, and every later link attempt throws it again (a verification
// failure leaves the class unlinked, retryable).
// We gave the verdict inside defineClass since wave 101
// (CRATONVM_VERIFY_USER_LOADER_CLASSES); wave 102 moves it to the link
// (CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK, default on, =0 restores the
// define-time verdict).
// Only the first line of each message is printed (the text itself is
// L7W101VerifyBadTypeOnStack's subject).
// Expected HotSpot 25 output:
//   == define
//   defined p.AtLink1
//   == forName-no-init
//   loaded p.AtLink1
//   == getDeclaredMethods
//   threw java.lang.VerifyError: Bad type on operand stack
//   == forName-init
//   threw java.lang.VerifyError: Bad type on operand stack
//   == forName-init-again
//   threw java.lang.VerifyError: Bad type on operand stack
//   == good-class
//   initialized p.AtLink2
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W102VerifyAtLink   (both modes).
public class L7W102VerifyAtLink {

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

    static String firstLine(Throwable t) {
        String m = t.getMessage();
        if (m == null) {
            return t.getClass().getName();
        }
        int nl = m.indexOf('\n');
        return t.getClass().getName() + ": " + (nl < 0 ? m : m.substring(0, nl));
    }

    interface Step {
        String run() throws Throwable;
    }

    static void step(String label, Step s) {
        System.out.println("== " + label);
        try {
            System.out.println(s.run());
        } catch (Throwable t) {
            System.out.println("threw " + firstLine(t));
        }
    }

    public static void main(String[] args) throws Exception {
        Asm a = new Asm();
        int valueOf = a.methodref("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;");
        // iconst_0; invokestatic String.valueOf(Object); pop; return
        byte[] bad = a.build("p/AtLink1", "run", "()V", 1, 0,
                b(0x03, 0xb8, valueOf >> 8, valueOf & 0xff, 0x57, 0xb1), null);
        // return
        byte[] good = new Asm().build("p/AtLink2", "run", "()V", 0, 0, b(0xb1), null);

        Loader l = new Loader();
        l.classes.put("p.AtLink1", bad);
        l.classes.put("p.AtLink2", good);
        step("define", () -> "defined " + l.loadClass("p.AtLink1").getName());
        step("forName-no-init", () -> "loaded " + Class.forName("p.AtLink1", false, l).getName());
        step("getDeclaredMethods", () -> "methods " + Class.forName("p.AtLink1", false, l).getDeclaredMethods().length);
        step("forName-init", () -> "initialized " + Class.forName("p.AtLink1", true, l).getName());
        step("forName-init-again", () -> "initialized " + Class.forName("p.AtLink1", true, l).getName());
        step("good-class", () -> "initialized " + Class.forName("p.AtLink2", true, l).getName());
    }
}
