// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 3, lane L4: the MESSAGE of NoSuchFieldError and
// NoSuchMethodError raised by field/method resolution (JVMS 5.4.3.2/5.4.3.3).
//
// The two classes are assembled in memory (class file version 52, straight-line
// code, no StackMapTable needed) and defined by a private loader, so the
// references to absent members cannot be caught by javac.
//
// Expected on HotSpot 25 (JDK-8298065 wording for fields, JDK 21+):
//   getstaticMissingInt: java.lang.NoSuchFieldError: Class Holder does not have member field 'int nope'
//   getfieldMissingArray: java.lang.NoSuchFieldError: Class Holder does not have member field 'java.lang.String[] names'
//   putfieldMissingRef: java.lang.NoSuchFieldError: Class Holder does not have member field 'java.lang.Object o'
//   putstaticMissingLong: java.lang.NoSuchFieldError: Class Holder does not have member field 'long[][] g'
//   getfieldWrongType: java.lang.NoSuchFieldError: Class Holder does not have member field 'long i'
//   invokestaticMissing: java.lang.NoSuchMethodError: 'long Holder.calc(int, java.lang.String[])'
//   invokestaticMissingVoid: java.lang.NoSuchMethodError: 'void Holder.plain()'
//   readI: returned 0
//
// CratonVM before wave 3 printed `Holder.nope` (and so on) for every field
// line. `getfieldWrongType` names a field that exists with another type
// (`int i`): resolution is by name AND descriptor, so it is absent.

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class NoSuchMemberMessageProbe {

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
            super(NoSuchMemberMessageProbe.class.getClassLoader());
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
        // Holder { public int i; public Holder() {} }
        ClassWriter h = new ClassWriter();
        int objInit = h.ref(10, "java/lang/Object", "<init>", "()V");
        h.field(0x0001, "i", "I");
        h.method(0x0001, "<init>", "()V", 1, 1,
                code(0x2a, 0xb7, objInit >> 8, objInit & 0xff, 0xb1));
        byte[] holder = h.build("Holder", "java/lang/Object");

        ClassWriter u = new ClassWriter();
        int nope = u.ref(9, "Holder", "nope", "I");
        int names = u.ref(9, "Holder", "names", "[Ljava/lang/String;");
        int o = u.ref(9, "Holder", "o", "Ljava/lang/Object;");
        int g = u.ref(9, "Holder", "g", "[[J");
        int iLong = u.ref(9, "Holder", "i", "J"); // exists, but as `int i`
        int iInt = u.ref(9, "Holder", "i", "I");
        int calc = u.ref(10, "Holder", "calc", "(I[Ljava/lang/String;)J");
        int plain = u.ref(10, "Holder", "plain", "()V");
        // getstatic Holder.nope:I
        u.method(0x0009, "getstaticMissingInt", "()I", 1, 0,
                code(0xb2, nope >> 8, nope & 0xff, 0xac));
        // getfield Holder.names:[Ljava/lang/String;
        u.method(0x0009, "getfieldMissingArray", "(LHolder;)Ljava/lang/Object;", 1, 1,
                code(0x2a, 0xb4, names >> 8, names & 0xff, 0xb0));
        // putfield Holder.o:Ljava/lang/Object; (aconst_null)
        u.method(0x0009, "putfieldMissingRef", "(LHolder;)V", 2, 1,
                code(0x2a, 0x01, 0xb5, o >> 8, o & 0xff, 0xb1));
        // putstatic Holder.g:[[J (aconst_null)
        u.method(0x0009, "putstaticMissingLong", "()V", 1, 0,
                code(0x01, 0xb3, g >> 8, g & 0xff, 0xb1));
        // getfield Holder.i:J against a class declaring `int i`
        u.method(0x0009, "getfieldWrongType", "(LHolder;)J", 2, 1,
                code(0x2a, 0xb4, iLong >> 8, iLong & 0xff, 0xad));
        // invokestatic Holder.calc(I[Ljava/lang/String;)J (iconst_0, aconst_null)
        u.method(0x0009, "invokestaticMissing", "()J", 2, 0,
                code(0x03, 0x01, 0xb8, calc >> 8, calc & 0xff, 0xad));
        // invokestatic Holder.plain()V
        u.method(0x0009, "invokestaticMissingVoid", "()V", 0, 0,
                code(0xb8, plain >> 8, plain & 0xff, 0xb1));
        // well-formed read, to show the class itself links
        u.method(0x0009, "readI", "(LHolder;)I", 1, 1,
                code(0x2a, 0xb4, iInt >> 8, iInt & 0xff, 0xac));
        byte[] userBytes = u.build("User", "java/lang/Object");

        Map<String, byte[]> defs = new HashMap<>();
        defs.put("Holder", holder);
        defs.put("User", userBytes);
        Loader loader = new Loader(defs);
        Class<?> holderClass = loader.loadClass("Holder");
        Class<?> user = loader.loadClass("User");
        Object instance = holderClass.getDeclaredConstructor().newInstance();

        run(user, "getstaticMissingInt");
        run(user, "getfieldMissingArray", instance);
        run(user, "putfieldMissingRef", instance);
        run(user, "putstaticMissingLong");
        run(user, "getfieldWrongType", instance);
        run(user, "invokestaticMissing");
        run(user, "invokestaticMissingVoid");
        run(user, "readI", instance);
    }
}
