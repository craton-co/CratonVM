// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 2, lane L4: putfield / putstatic to a `final` field
// outside its initializer. JVMS 6.5 (and HotSpot's LinkResolver::resolve_field)
// throw IllegalAccessError when the writer is not the declaring class (any class
// file version), or -- for class files of version 53 and later -- when the
// writing method is not <init> (instance field) / <clinit> (static field).
//
// The classes are assembled in memory (straight-line code, so no StackMapTable
// is needed) and defined by a private loader, so javac cannot refuse them.
// Messages are printed in [brackets] because HotSpot's initializer-method form
// ends in a space.
//
// Expected on HotSpot 25:
//   F.x after new = 1
//   F.S = 1
//   getX: returned 1
//   reset: java.lang.IllegalAccessError: [Update to non-static final field F.x attempted from a different method (reset) than the initializer method <init> ]
//   setS: java.lang.IllegalAccessError: [Update to static final field F.S attempted from a different method (setS) than the initializer method <clinit> ]
//   poke: java.lang.IllegalAccessError: [Update to non-static final field F.x attempted from a different class (Other) than the field's declaring class]
//   G.reset: returned null
//   G.y after reset = 5
//   hot reset: 20000 IllegalAccessError of 20000
//   hot poke: 20000 IllegalAccessError of 20000
//   F.x after all = 1
//
// CratonVM before wave 2 printed "returned null" for reset/setS/poke and let
// the stores through (F.x after all = 7). The "hot" lines check that neither
// the quickened putfield site (filled by 1000 legal <init> stores and by getX's
// getfield through the same constant-pool entry) nor a JIT compile of the hot
// method lets a later illegal store through: run with and without --nojit.

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class FinalFieldPutProbe {

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

        byte[] build(String name, String superName, int major) throws IOException {
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
            out.writeShort(major);
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
            super(FinalFieldPutProbe.class.getClassLoader());
            this.defs = defs;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = defs.get(name);
            if (b == null) throw new ClassNotFoundException(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static Method find(Class<?> c, String name) {
        for (Method m : c.getMethods()) {
            if (m.getName().equals(name)) return m;
        }
        throw new IllegalStateException(name);
    }

    static void run(String label, Method target, Object recv, Object... args) {
        try {
            Object r = target.invoke(recv, args);
            System.out.println(label + ": returned " + r);
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            System.out.println(label + ": " + c.getClass().getName() + ": [" + c.getMessage() + "]");
        } catch (IllegalAccessException e) {
            System.out.println(label + ": reflection failed: " + e);
        }
    }

    static int countIae(Method target, Object recv, Object... args) throws Exception {
        int n = 0;
        for (int k = 0; k < 20000; k++) {
            try {
                target.invoke(recv, args);
            } catch (InvocationTargetException e) {
                if (e.getCause() instanceof IllegalAccessError) n++;
            }
        }
        return n;
    }

    public static void main(String[] a) throws Exception {
        // F (version 61): public final int x; public static final int S;
        //   <init>: x = 1;  <clinit>: S = 1;  getX; reset: x = 5 (illegal);
        //   static setS: S = 5 (illegal)
        ClassWriter f = new ClassWriter();
        int objInit = f.ref(10, "java/lang/Object", "<init>", "()V");
        int fx = f.ref(9, "F", "x", "I");
        int fS = f.ref(9, "F", "S", "I");
        f.field(0x0011, "x", "I");
        f.field(0x0019, "S", "I");
        f.method(0x0001, "<init>", "()V", 2, 1,
                code(0x2a, 0xb7, objInit >> 8, objInit & 0xff,
                        0x2a, 0x04, 0xb5, fx >> 8, fx & 0xff, 0xb1));
        f.method(0x0008, "<clinit>", "()V", 1, 0,
                code(0x04, 0xb3, fS >> 8, fS & 0xff, 0xb1));
        f.method(0x0001, "getX", "()I", 1, 1,
                code(0x2a, 0xb4, fx >> 8, fx & 0xff, 0xac));
        f.method(0x0001, "reset", "()V", 2, 1,
                code(0x2a, 0x08, 0xb5, fx >> 8, fx & 0xff, 0xb1));
        f.method(0x0009, "setS", "()V", 1, 0,
                code(0x08, 0xb3, fS >> 8, fS & 0xff, 0xb1));
        f.method(0x0009, "readS", "()I", 1, 0,
                code(0xb2, fS >> 8, fS & 0xff, 0xac));
        byte[] fBytes = f.build("F", "java/lang/Object", 61);

        // G (version 52): public final int y; reset() writes y -- legal below 53.
        ClassWriter g = new ClassWriter();
        int gObjInit = g.ref(10, "java/lang/Object", "<init>", "()V");
        int gy = g.ref(9, "G", "y", "I");
        g.field(0x0011, "y", "I");
        g.method(0x0001, "<init>", "()V", 1, 1,
                code(0x2a, 0xb7, gObjInit >> 8, gObjInit & 0xff, 0xb1));
        g.method(0x0001, "reset", "()V", 2, 1,
                code(0x2a, 0x08, 0xb5, gy >> 8, gy & 0xff, 0xb1));
        g.method(0x0001, "getY", "()I", 1, 1,
                code(0x2a, 0xb4, gy >> 8, gy & 0xff, 0xac));
        byte[] gBytes = g.build("G", "java/lang/Object", 52);

        // Other (version 52): static poke(F) writes F.x -- illegal in any version.
        ClassWriter o = new ClassWriter();
        int ox = o.ref(9, "F", "x", "I");
        o.method(0x0009, "poke", "(LF;)V", 2, 1,
                code(0x2a, 0x10, 7, 0xb5, ox >> 8, ox & 0xff, 0xb1));
        byte[] oBytes = o.build("Other", "java/lang/Object", 52);

        Map<String, byte[]> defs = new HashMap<>();
        defs.put("F", fBytes);
        defs.put("G", gBytes);
        defs.put("Other", oBytes);
        Loader loader = new Loader(defs);
        Class<?> fClass = loader.loadClass("F");
        Class<?> gClass = loader.loadClass("G");
        Class<?> other = loader.loadClass("Other");

        Object fi = null;
        for (int k = 0; k < 1000; k++) {
            fi = fClass.getDeclaredConstructor().newInstance();
        }
        Method getX = find(fClass, "getX");
        System.out.println("F.x after new = " + getX.invoke(fi));
        System.out.println("F.S = " + find(fClass, "readS").invoke(null));
        run("getX", getX, fi);
        run("reset", find(fClass, "reset"), fi);
        run("setS", find(fClass, "setS"), null);
        run("poke", find(other, "poke"), null, fi);

        Object gi = gClass.getDeclaredConstructor().newInstance();
        run("G.reset", find(gClass, "reset"), gi);
        System.out.println("G.y after reset = " + find(gClass, "getY").invoke(gi));

        System.out.println("hot reset: " + countIae(find(fClass, "reset"), fi) + " IllegalAccessError of 20000");
        System.out.println("hot poke: " + countIae(find(other, "poke"), null, fi) + " IllegalAccessError of 20000");
        System.out.println("F.x after all = " + getX.invoke(fi));
    }
}
