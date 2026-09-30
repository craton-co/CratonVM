// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, lane L4: getfield/putfield/getstatic/putstatic against a
// field whose static-ness disagrees with the opcode (a binary-incompatible class
// change). JVMS 6.5 requires IncompatibleClassChangeError at link time.
//
// The two classes are assembled in memory (class file version 52, straight-line
// code, so no StackMapTable is needed) and defined by a private loader, so the
// mismatch cannot be caught by javac.
//
// Expected on HotSpot 25:
//   setS: returned null
//   getfieldOnStatic: java.lang.IncompatibleClassChangeError: Expected non-static field Holder.s
//   getstaticOnInstance: java.lang.IncompatibleClassChangeError: Expected static field Holder.i
//   putfieldOnStatic: java.lang.IncompatibleClassChangeError: Expected non-static field Holder.s
//   putstaticOnInstance: java.lang.IncompatibleClassChangeError: Expected static field Holder.i
//   getfieldOnStaticNullReceiver: java.lang.IncompatibleClassChangeError: Expected non-static field Holder.s
//   Holder.s after = 7
//   Holder.i after = 0
//
// CratonVM before round i1 returned VALUES instead (the static slot at the
// instance index and vice versa) and silently wrote the wrong slot. The
// null-receiver line is a separate ordering check: HotSpot resolves (and so
// throws ICCE) before it looks at the receiver; an implementation that pops and
// null-checks the receiver first reports a NullPointerException instead (the
// pre-i1 `op_getfield` did). Since wave 2 the JIT compile doors refuse such a
// site, leaving the method to the interpreter: a JIT run (with User.* warmed
// past the compile threshold) must print the same lines as --nojit.

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class FieldKindMismatchProbe {

    /** A minimal class-file writer: constant pool, fields, methods with Code. */
    static final class ClassWriter {
        private final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        private final DataOutputStream cp = new DataOutputStream(cpBytes);
        private final Map<String, Integer> pool = new HashMap<>();
        private int cpCount = 1;
        private final List<Object[]> fields = new ArrayList<>();
        private final List<Object[]> methods = new ArrayList<>();

        int utf8(String s) throws IOException {
            Integer i = pool.get("1:" + s);
            if (i != null) return i;
            cp.writeByte(1);
            cp.writeUTF(s);
            pool.put("1:" + s, cpCount);
            return cpCount++;
        }

        int cls(String name) throws IOException {
            int n = utf8(name);
            Integer i = pool.get("7:" + name);
            if (i != null) return i;
            cp.writeByte(7);
            cp.writeShort(n);
            pool.put("7:" + name, cpCount);
            return cpCount++;
        }

        int nat(String name, String desc) throws IOException {
            int n = utf8(name);
            int d = utf8(desc);
            String key = "12:" + name + ":" + desc;
            Integer i = pool.get(key);
            if (i != null) return i;
            cp.writeByte(12);
            cp.writeShort(n);
            cp.writeShort(d);
            pool.put(key, cpCount);
            return cpCount++;
        }

        /** tag 9 = Fieldref, 10 = Methodref. */
        int ref(int tag, String owner, String name, String desc) throws IOException {
            int c = cls(owner);
            int nt = nat(name, desc);
            String key = tag + ":" + owner + "." + name + ":" + desc;
            Integer i = pool.get(key);
            if (i != null) return i;
            cp.writeByte(tag);
            cp.writeShort(c);
            cp.writeShort(nt);
            pool.put(key, cpCount);
            return cpCount++;
        }

        void field(int access, String name, String desc) {
            fields.add(new Object[] {access, name, desc});
        }

        void method(int access, String name, String desc, int maxStack, int maxLocals, byte[] code) {
            methods.add(new Object[] {access, name, desc, maxStack, maxLocals, code});
        }

        byte[] build(String name, String superName) throws IOException {
            int thisIdx = cls(name);
            int superIdx = cls(superName);
            int codeIdx = utf8("Code");
            int[][] fieldIdx = new int[fields.size()][];
            for (int k = 0; k < fields.size(); k++) {
                Object[] f = fields.get(k);
                fieldIdx[k] = new int[] {utf8((String) f[1]), utf8((String) f[2])};
            }
            int[][] methodIdx = new int[methods.size()][];
            for (int k = 0; k < methods.size(); k++) {
                Object[] m = methods.get(k);
                methodIdx[k] = new int[] {utf8((String) m[1]), utf8((String) m[2])};
            }
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(52);
            out.writeShort(cpCount);
            out.write(cpBytes.toByteArray());
            out.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
            out.writeShort(thisIdx);
            out.writeShort(superIdx);
            out.writeShort(0); // interfaces
            out.writeShort(fields.size());
            for (int k = 0; k < fields.size(); k++) {
                out.writeShort((Integer) fields.get(k)[0]);
                out.writeShort(fieldIdx[k][0]);
                out.writeShort(fieldIdx[k][1]);
                out.writeShort(0);
            }
            out.writeShort(methods.size());
            for (int k = 0; k < methods.size(); k++) {
                Object[] m = methods.get(k);
                byte[] code = (byte[]) m[5];
                out.writeShort((Integer) m[0]);
                out.writeShort(methodIdx[k][0]);
                out.writeShort(methodIdx[k][1]);
                out.writeShort(1); // one attribute: Code
                out.writeShort(codeIdx);
                out.writeInt(12 + code.length);
                out.writeShort((Integer) m[3]);
                out.writeShort((Integer) m[4]);
                out.writeInt(code.length);
                out.write(code);
                out.writeShort(0); // exception table
                out.writeShort(0); // attributes
            }
            out.writeShort(0); // class attributes
            return bytes.toByteArray();
        }
    }

    static byte[] code(int... b) {
        byte[] out = new byte[b.length];
        for (int k = 0; k < b.length; k++) out[k] = (byte) b[k];
        return out;
    }

    static final class Loader extends ClassLoader {
        private final Map<String, byte[]> defs;

        Loader(Map<String, byte[]> defs) {
            super(FieldKindMismatchProbe.class.getClassLoader());
            this.defs = defs;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = defs.get(name);
            if (b == null) throw new ClassNotFoundException(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static void run(Class<?> user, String name, Object... args) {
        Method target = null;
        for (Method m : user.getMethods()) {
            if (m.getName().equals(name)) target = m;
        }
        try {
            Object r = target.invoke(null, args);
            System.out.println(name + ": returned " + r);
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            System.out.println(name + ": " + c.getClass().getName() + ": " + c.getMessage());
        } catch (IllegalAccessException e) {
            System.out.println(name + ": reflection failed: " + e);
        }
    }

    public static void main(String[] a) throws Exception {
        // Holder { public static int s; public int i; public Holder() {} }
        ClassWriter h = new ClassWriter();
        int objInit = h.ref(10, "java/lang/Object", "<init>", "()V");
        h.field(0x0009, "s", "I");
        h.field(0x0001, "i", "I");
        h.method(0x0001, "<init>", "()V", 1, 1,
                code(0x2a, 0xb7, objInit >> 8, objInit & 0xff, 0xb1));
        byte[] holder = h.build("Holder", "java/lang/Object");

        ClassWriter u = new ClassWriter();
        int fs = u.ref(9, "Holder", "s", "I"); // declared static in Holder
        int fi = u.ref(9, "Holder", "i", "I"); // declared instance in Holder
        // getfield on the STATIC field
        u.method(0x0009, "getfieldOnStatic", "(LHolder;)I", 1, 1,
                code(0x2a, 0xb4, fs >> 8, fs & 0xff, 0xac));
        // getstatic on the INSTANCE field
        u.method(0x0009, "getstaticOnInstance", "()I", 1, 0,
                code(0xb2, fi >> 8, fi & 0xff, 0xac));
        // putfield on the STATIC field (iconst_5)
        u.method(0x0009, "putfieldOnStatic", "(LHolder;)V", 2, 1,
                code(0x2a, 0x08, 0xb5, fs >> 8, fs & 0xff, 0xb1));
        // putstatic on the INSTANCE field (iconst_5)
        u.method(0x0009, "putstaticOnInstance", "()V", 1, 0,
                code(0x08, 0xb3, fi >> 8, fi & 0xff, 0xb1));
        // getfield on the STATIC field with a null receiver
        u.method(0x0009, "getfieldOnStaticNullReceiver", "()I", 1, 0,
                code(0x01, 0xb4, fs >> 8, fs & 0xff, 0xac));
        // well-formed accessors, to show nothing was written by the above
        u.method(0x0009, "setS", "()V", 1, 0,
                code(0x10, 7, 0xb3, fs >> 8, fs & 0xff, 0xb1));
        u.method(0x0009, "readS", "()I", 1, 0,
                code(0xb2, fs >> 8, fs & 0xff, 0xac));
        u.method(0x0009, "readI", "(LHolder;)I", 1, 1,
                code(0x2a, 0xb4, fi >> 8, fi & 0xff, 0xac));
        byte[] userBytes = u.build("User", "java/lang/Object");

        Map<String, byte[]> defs = new HashMap<>();
        defs.put("Holder", holder);
        defs.put("User", userBytes);
        Loader loader = new Loader(defs);
        Class<?> holderClass = loader.loadClass("Holder");
        Class<?> user = loader.loadClass("User");
        Object instance = holderClass.getDeclaredConstructor().newInstance();

        run(user, "setS");
        run(user, "getfieldOnStatic", instance);
        run(user, "getstaticOnInstance");
        run(user, "putfieldOnStatic", instance);
        run(user, "putstaticOnInstance");
        run(user, "getfieldOnStaticNullReceiver");
        Method readS = user.getMethod("readS");
        Method readI = user.getMethod("readI", holderClass);
        System.out.println("Holder.s after = " + readS.invoke(null));
        System.out.println("Holder.i after = " + readI.invoke(null, instance));
    }
}
