// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 2, lane L3: JVMS 6.5 linkage errors and 5.4.6
// method selection for shapes javac refuses to compile, so the classes are
// written by a tiny in-process class-file writer and defined by a private
// loader (the "separate compilation" the i1-L3 pages ask for).
//
// Every case runs three times (cold slow path, then the warm cached paths) and
// prints `case#round: result` or `case#round: ExceptionClass: message`.
// Compare with HotSpot 25; the RESULT and EXCEPTION CLASS columns must match
// exactly. HotSpot's expected outcomes (messages abbreviated):
//
//   iface-not-implemented   IncompatibleClassChangeError: Class L3i.C does
//                           not implement the requested interface L3i.I
//   static-as-virtual       IncompatibleClassChangeError: Expecting
//                           non-static method '... L3s.S.m()'
//   virtual-as-static       IncompatibleClassChangeError: Expected static
//                           method '... L3s.V.n()'
//   abstract-redeclared     AbstractMethodError (C extends abstract B, which
//                           re-declares A.m abstract): "Receiver class L3a.C
//                           ... of class L3a.A. Selected method is 'abstract
//                           java.lang.String L3a.B.m()'."
//   conflicting-defaults    IncompatibleClassChangeError: Conflicting default
//                           methods: L3d/J1.d L3d/J2.d
//   masked-default          AbstractMethodError (K extends J1 re-declares d
//                           abstract; E implements K): "Method
//                           L3d/E.d()Ljava/lang/String; is abstract"
//
// Wave 3 made the messages match too (selection.rs abstract_method_message,
// conflicting_defaults_message, superinterface_closure walk order).
//   package-private         p1.A.m    (p2.B.m does not override it, 5.4.5)
//   private-in-subclass     A.m       (a private B.m is not an override)
//   public-override         B.m       (control: an ordinary override)
//
// Before wave 2 CratonVM ran C.m, S.m, V.n, A.m (twice), J1.d or J2.d, J1.d,
// p2.B.m and B.m respectively.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.Map;

public class LinkageSelectionProbe {
    static final int PUBLIC = 0x0001, PRIVATE = 0x0002, STATIC = 0x0008, SUPER = 0x0020,
            INTERFACE = 0x0200, ABSTRACT = 0x0400;
    static final String S = "()Ljava/lang/String;";

    /** A class-file writer for straight-line methods (no StackMapTable needed). */
    static final class ClassWriter {
        private final ByteArrayOutputStream poolBytes = new ByteArrayOutputStream();
        private final DataOutputStream pool = new DataOutputStream(poolBytes);
        private final Map<String, Integer> cache = new HashMap<>();
        private int next = 1;
        private final ByteArrayOutputStream methodBytes = new ByteArrayOutputStream();
        private final DataOutputStream methods = new DataOutputStream(methodBytes);
        private int methodCount;
        private final String name;
        private final String superName;
        private final String[] interfaces;
        private final int access;

        ClassWriter(int access, String name, String superName, String... interfaces) {
            this.access = access;
            this.name = name;
            this.superName = superName;
            this.interfaces = interfaces;
        }

        private int entry(String key, int tag, int a, int b, boolean two) throws IOException {
            Integer found = cache.get(key);
            if (found != null) return found;
            pool.writeByte(tag);
            pool.writeShort(a);
            if (two) pool.writeShort(b);
            cache.put(key, next);
            return next++;
        }

        int utf8(String s) throws IOException {
            Integer found = cache.get("U" + s);
            if (found != null) return found;
            pool.writeByte(1);
            pool.writeUTF(s);
            cache.put("U" + s, next);
            return next++;
        }

        int cls(String n) throws IOException {
            int u = utf8(n);
            return entry("C" + n, 7, u, 0, false);
        }

        int str(String s) throws IOException {
            int u = utf8(s);
            return entry("S" + s, 8, u, 0, false);
        }

        int ref(int tag, String owner, String n, String d) throws IOException {
            int c = cls(owner);
            int un = utf8(n);
            int ud = utf8(d);
            int nat = entry("N" + n + ":" + d, 12, un, ud, true);
            return entry("R" + tag + owner + "." + n + d, tag, c, nat, true);
        }

        int methodref(String owner, String n, String d) throws IOException {
            return ref(10, owner, n, d);
        }

        int interfaceMethodref(String owner, String n, String d) throws IOException {
            return ref(11, owner, n, d);
        }

        void method(int acc, String n, String d, int maxStack, int maxLocals, byte[] code)
                throws IOException {
            methods.writeShort(acc);
            methods.writeShort(utf8(n));
            methods.writeShort(utf8(d));
            if (code == null) {
                methods.writeShort(0);
            } else {
                methods.writeShort(1);
                methods.writeShort(utf8("Code"));
                methods.writeInt(12 + code.length);
                methods.writeShort(maxStack);
                methods.writeShort(maxLocals);
                methods.writeInt(code.length);
                methods.write(code);
                methods.writeShort(0); // exception table
                methods.writeShort(0); // attributes
            }
            methodCount++;
        }

        /** `public <init>()V { super(); }` */
        void defaultConstructor() throws IOException {
            int superInit = methodref(superName, "<init>", "()V");
            method(PUBLIC, "<init>", "()V", 1, 1,
                    new byte[] {0x2a, (byte) 0xb7, hi(superInit), lo(superInit), (byte) 0xb1});
        }

        /** A method whose body is `return "<text>";`. */
        void returnsString(int acc, String n, String text) throws IOException {
            int s = str(text);
            method(acc, n, S, 1, (acc & STATIC) != 0 ? 0 : 1,
                    new byte[] {0x13, hi(s), lo(s), (byte) 0xb0});
        }

        byte[] toBytes() throws IOException {
            int thisIdx = cls(name);
            int superIdx = cls(superName);
            int[] ifaceIdx = new int[interfaces.length];
            for (int i = 0; i < interfaces.length; i++) ifaceIdx[i] = cls(interfaces[i]);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            DataOutputStream d = new DataOutputStream(out);
            d.writeInt(0xCAFEBABE);
            d.writeShort(0);
            d.writeShort(52);
            d.writeShort(next);
            d.write(poolBytes.toByteArray());
            d.writeShort(access);
            d.writeShort(thisIdx);
            d.writeShort(superIdx);
            d.writeShort(interfaces.length);
            for (int i : ifaceIdx) d.writeShort(i);
            d.writeShort(0); // fields
            d.writeShort(methodCount);
            d.write(methodBytes.toByteArray());
            d.writeShort(0); // attributes
            return out.toByteArray();
        }
    }

    static byte hi(int v) {
        return (byte) (v >> 8);
    }

    static byte lo(int v) {
        return (byte) v;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(LinkageSelectionProbe.class.getClassLoader());
        }

        Class<?> define(ClassWriter w) throws IOException {
            byte[] b = w.toBytes();
            return defineClass(w.name.replace('/', '.'), b, 0, b.length);
        }
    }

    /** `public static String run(<param>) { return ((param) p).<op> owner.m(); }` */
    static ClassWriter caller(String name, String param, int opcode, String owner, String m,
            boolean iface) throws IOException {
        ClassWriter w = new ClassWriter(PUBLIC | SUPER, name, "java/lang/Object");
        w.defaultConstructor();
        int ref = iface ? w.interfaceMethodref(owner, m, S) : w.methodref(owner, m, S);
        byte[] code;
        String desc;
        if (opcode == 0xb8) { // invokestatic, no argument
            code = new byte[] {(byte) 0xb8, hi(ref), lo(ref), (byte) 0xb0};
            desc = S;
            w.method(PUBLIC | STATIC, "run", desc, 1, 0, code);
        } else if (opcode == 0xb9) {
            code = new byte[] {0x2a, (byte) 0xb9, hi(ref), lo(ref), 1, 0, (byte) 0xb0};
            desc = "(" + param + ")Ljava/lang/String;";
            w.method(PUBLIC | STATIC, "run", desc, 1, 1, code);
        } else {
            code = new byte[] {0x2a, (byte) opcode, hi(ref), lo(ref), (byte) 0xb0};
            desc = "(" + param + ")Ljava/lang/String;";
            w.method(PUBLIC | STATIC, "run", desc, 1, 1, code);
        }
        return w;
    }

    static void report(String label, Method run, Object arg) {
        for (int round = 1; round <= 3; round++) {
            String out;
            try {
                out = String.valueOf(run.getParameterCount() == 0 ? run.invoke(null) : run.invoke(null, arg));
            } catch (InvocationTargetException e) {
                Throwable c = e.getCause();
                out = c.getClass().getName() + ": " + c.getMessage();
            } catch (Throwable t) {
                out = "harness " + t.getClass().getName() + ": " + t.getMessage();
            }
            System.out.println(label + "#" + round + ": " + out);
        }
    }

    static Object instance(Class<?> c) throws Exception {
        return c.getConstructor().newInstance();
    }

    public static void main(String[] args) throws Exception {
        Loader l = new Loader();

        // iface-not-implemented: C has a public m() but does not implement I.
        ClassWriter i = new ClassWriter(PUBLIC | INTERFACE | ABSTRACT, "L3i/I", "java/lang/Object");
        i.method(PUBLIC | ABSTRACT, "m", S, 0, 0, null);
        l.define(i);
        ClassWriter c = new ClassWriter(PUBLIC | SUPER, "L3i/C", "java/lang/Object");
        c.defaultConstructor();
        c.returnsString(PUBLIC, "m", "C.m");
        Class<?> cC = l.define(c);
        Class<?> ci = l.define(caller("L3i/Caller", "Ljava/lang/Object;", 0xb9, "L3i/I", "m", true));
        report("iface-not-implemented", ci.getMethod("run", Object.class), instance(cC));

        // static-as-virtual / virtual-as-static.
        ClassWriter s = new ClassWriter(PUBLIC | SUPER, "L3s/S", "java/lang/Object");
        s.defaultConstructor();
        s.returnsString(PUBLIC | STATIC, "m", "S.m");
        Class<?> cS = l.define(s);
        Class<?> cs = l.define(caller("L3s/Caller", "LL3s/S;", 0xb6, "L3s/S", "m", false));
        report("static-as-virtual", cs.getMethod("run", cS), instance(cS));
        ClassWriter v = new ClassWriter(PUBLIC | SUPER, "L3s/V", "java/lang/Object");
        v.defaultConstructor();
        v.returnsString(PUBLIC, "n", "V.n");
        l.define(v);
        Class<?> cv = l.define(caller("L3s/Caller2", null, 0xb8, "L3s/V", "n", false));
        report("virtual-as-static", cv.getMethod("run"), null);

        // abstract-redeclared: A.m concrete, abstract B re-declares m abstract, C extends B.
        ClassWriter a = new ClassWriter(PUBLIC | SUPER, "L3a/A", "java/lang/Object");
        a.defaultConstructor();
        a.returnsString(PUBLIC, "m", "A.m");
        Class<?> cA = l.define(a);
        ClassWriter b = new ClassWriter(PUBLIC | SUPER | ABSTRACT, "L3a/B", "L3a/A");
        b.defaultConstructor();
        b.method(PUBLIC | ABSTRACT, "m", S, 0, 0, null);
        l.define(b);
        ClassWriter cc = new ClassWriter(PUBLIC | SUPER, "L3a/C", "L3a/B");
        cc.defaultConstructor();
        Class<?> cAC = l.define(cc);
        Class<?> ca = l.define(caller("L3a/Caller", "LL3a/A;", 0xb6, "L3a/A", "m", false));
        report("abstract-redeclared", ca.getMethod("run", cA), instance(cAC));

        // conflicting-defaults: D implements J1, J2, both with a default d().
        ClassWriter j1 = new ClassWriter(PUBLIC | INTERFACE | ABSTRACT, "L3d/J1", "java/lang/Object");
        j1.returnsString(PUBLIC, "d", "J1.d");
        l.define(j1);
        ClassWriter j2 = new ClassWriter(PUBLIC | INTERFACE | ABSTRACT, "L3d/J2", "java/lang/Object");
        j2.returnsString(PUBLIC, "d", "J2.d");
        l.define(j2);
        ClassWriter dd = new ClassWriter(PUBLIC | SUPER, "L3d/D", "java/lang/Object", "L3d/J1", "L3d/J2");
        dd.defaultConstructor();
        Class<?> cD = l.define(dd);
        Class<?> cd = l.define(caller("L3d/Caller", "Ljava/lang/Object;", 0xb9, "L3d/J1", "d", true));
        report("conflicting-defaults", cd.getMethod("run", Object.class), instance(cD));

        // masked-default: K extends J1 and re-declares d abstract; E implements K.
        ClassWriter k = new ClassWriter(PUBLIC | INTERFACE | ABSTRACT, "L3d/K", "java/lang/Object", "L3d/J1");
        k.method(PUBLIC | ABSTRACT, "d", S, 0, 0, null);
        l.define(k);
        ClassWriter e = new ClassWriter(PUBLIC | SUPER, "L3d/E", "java/lang/Object", "L3d/K");
        e.defaultConstructor();
        Class<?> cE = l.define(e);
        report("masked-default", cd.getMethod("run", Object.class), instance(cE));

        // package-private: p2.B.m does not override the package-private p1.A.m.
        ClassWriter pa = new ClassWriter(PUBLIC | SUPER, "L3p/p1/A", "java/lang/Object");
        pa.defaultConstructor();
        pa.returnsString(0, "m", "p1.A.m");
        Class<?> cPA = l.define(pa);
        ClassWriter pb = new ClassWriter(PUBLIC | SUPER, "L3p/p2/B", "L3p/p1/A");
        pb.defaultConstructor();
        pb.returnsString(PUBLIC, "m", "p2.B.m");
        Class<?> cPB = l.define(pb);
        Class<?> cp = l.define(caller("L3p/p1/Caller", "LL3p/p1/A;", 0xb6, "L3p/p1/A", "m", false));
        report("package-private", cp.getMethod("run", cPA), instance(cPB));

        // private-in-subclass: a private B.m is not an override of A.m.
        ClassWriter va = new ClassWriter(PUBLIC | SUPER, "L3v/A", "java/lang/Object");
        va.defaultConstructor();
        va.returnsString(PUBLIC, "m", "A.m");
        Class<?> cVA = l.define(va);
        ClassWriter vb = new ClassWriter(PUBLIC | SUPER, "L3v/B", "L3v/A");
        vb.defaultConstructor();
        vb.returnsString(PRIVATE, "m", "B.m");
        Class<?> cVB = l.define(vb);
        Class<?> cvv = l.define(caller("L3v/Caller", "LL3v/A;", 0xb6, "L3v/A", "m", false));
        report("private-in-subclass", cvv.getMethod("run", cVA), instance(cVB));

        // public-override: the control.
        ClassWriter vc = new ClassWriter(PUBLIC | SUPER, "L3v/C", "L3v/A");
        vc.defaultConstructor();
        vc.returnsString(PUBLIC, "m", "B.m");
        Class<?> cVC = l.define(vc);
        report("public-override", cvv.getMethod("run", cVA), instance(cVC));
    }
}
