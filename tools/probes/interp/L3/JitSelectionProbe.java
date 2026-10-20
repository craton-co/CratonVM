// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 8, lane L3: JVMS 5.4.6 selection on the COMPILED
// dispatch paths. `LinkageSelectionProbe` checks the interpreter (three calls
// per shape); this one runs each shape's `invokevirtual` inside a hot loop, so
// the caller is OSR- or invocation-compiled and the call goes through the JIT
// dispatch helpers (MIC / PIC, the by-name tail, the interpreted-callee
// template) and the receiver-guarded inliner.
//
// The classes are written in-process (javac refuses the shapes) at class-file
// version 49, so the loop needs no StackMapTable, and defined by a private
// loader. `Caller.loop(A a, int n)` runs `r = a.m()` n times and returns r.
//
// HotSpot 25 prints exactly:
//   private-in-subclass round 1: A.m
//   private-in-subclass round 2: A.m
//   private-in-subclass round 3: A.m
//   private-in-subclass round 4: A.m
//   package-private round 1: p1.A.m
//   package-private round 2: p1.A.m
//   package-private round 3: p1.A.m
//   package-private round 4: p1.A.m
//   public-override round 1: C.m
//   public-override round 2: C.m
//   public-override round 3: C.m
//   public-override round 4: C.m
//
// Before wave 8 a compiled caller could print B.m / p2.B.m for the first two
// shapes once the loop left the interpreter. Compare `--compatible` with and
// without `--nojit`, and with CRATONVM_BG_COMPILE=0; stdout must be identical.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.Map;

public class JitSelectionProbe {
    static final int PUBLIC = 0x0001, PRIVATE = 0x0002, STATIC = 0x0008, SUPER = 0x0020;
    static final String S = "()Ljava/lang/String;";
    static final int ROUNDS = 4, CALLS = 60000;

    static final class ClassWriter {
        private final ByteArrayOutputStream poolBytes = new ByteArrayOutputStream();
        private final DataOutputStream pool = new DataOutputStream(poolBytes);
        private final Map<String, Integer> cache = new HashMap<>();
        private int next = 1;
        private final ByteArrayOutputStream methodBytes = new ByteArrayOutputStream();
        private final DataOutputStream methods = new DataOutputStream(methodBytes);
        private int methodCount;
        final String name;
        private final String superName;

        ClassWriter(String name, String superName) {
            this.name = name;
            this.superName = superName;
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
            return entry("C" + n, 7, utf8(n), 0, false);
        }

        int str(String s) throws IOException {
            return entry("S" + s, 8, utf8(s), 0, false);
        }

        int methodref(String owner, String n, String d) throws IOException {
            int c = cls(owner);
            int nat = entry("N" + n + ":" + d, 12, utf8(n), utf8(d), true);
            return entry("R" + owner + "." + n + d, 10, c, nat, true);
        }

        void method(int acc, String n, String d, int maxStack, int maxLocals, byte[] code)
                throws IOException {
            methods.writeShort(acc);
            methods.writeShort(utf8(n));
            methods.writeShort(utf8(d));
            methods.writeShort(1);
            methods.writeShort(utf8("Code"));
            methods.writeInt(12 + code.length);
            methods.writeShort(maxStack);
            methods.writeShort(maxLocals);
            methods.writeInt(code.length);
            methods.write(code);
            methods.writeShort(0); // exception table
            methods.writeShort(0); // attributes
            methodCount++;
        }

        void defaultConstructor() throws IOException {
            int superInit = methodref(superName, "<init>", "()V");
            method(PUBLIC, "<init>", "()V", 1, 1,
                    new byte[] {0x2a, (byte) 0xb7, hi(superInit), lo(superInit), (byte) 0xb1});
        }

        void returnsString(int acc, String n, String text) throws IOException {
            int s = str(text);
            method(acc, n, S, 1, 1, new byte[] {0x13, hi(s), lo(s), (byte) 0xb0});
        }

        byte[] toBytes() throws IOException {
            int thisIdx = cls(name);
            int superIdx = cls(superName);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            DataOutputStream d = new DataOutputStream(out);
            d.writeInt(0xCAFEBABE);
            d.writeShort(0);
            d.writeShort(49); // no StackMapTable needed for the loop
            d.writeShort(next);
            d.write(poolBytes.toByteArray());
            d.writeShort(PUBLIC | SUPER);
            d.writeShort(thisIdx);
            d.writeShort(superIdx);
            d.writeShort(0); // interfaces
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
            super(JitSelectionProbe.class.getClassLoader());
        }

        Class<?> define(ClassWriter w) throws IOException {
            byte[] b = w.toBytes();
            return defineClass(w.name.replace('/', '.'), b, 0, b.length);
        }
    }

    /**
     * `public static String loop(owner a, int n)`:
     * `String r = null; for (int i = 0; i < n; i++) r = a.m(); return r;`
     */
    static ClassWriter caller(String name, String owner) throws IOException {
        ClassWriter w = new ClassWriter(name, "java/lang/Object");
        w.defaultConstructor();
        int ref = w.methodref(owner, "m", S);
        byte[] code = {
            0x01, // 0: aconst_null
            0x4d, // 1: astore_2
            0x03, // 2: iconst_0
            0x3e, // 3: istore_3
            0x1d, // 4: iload_3
            0x1b, // 5: iload_1
            (byte) 0xa2, 0x00, 0x0e, // 6: if_icmpge 20
            0x2a, // 9: aload_0
            (byte) 0xb6, hi(ref), lo(ref), // 10: invokevirtual owner.m
            0x4d, // 13: astore_2
            (byte) 0x84, 0x03, 0x01, // 14: iinc 3, 1
            (byte) 0xa7, (byte) 0xff, (byte) 0xf3, // 17: goto 4
            0x2c, // 20: aload_2
            (byte) 0xb0, // 21: areturn
        };
        w.method(PUBLIC | STATIC, "loop", "(L" + owner + ";I)Ljava/lang/String;", 2, 4, code);
        return w;
    }

    static void run(String label, Class<?> callerClass, Class<?> ownerClass, Object receiver)
            throws Exception {
        Method loop = callerClass.getMethod("loop", ownerClass, int.class);
        for (int round = 1; round <= ROUNDS; round++) {
            Object r;
            try {
                r = loop.invoke(null, receiver, CALLS);
            } catch (java.lang.reflect.InvocationTargetException e) {
                r = e.getCause().getClass().getName() + ": " + e.getCause().getMessage();
            }
            System.out.println(label + " round " + round + ": " + r);
        }
    }

    public static void main(String[] args) throws Exception {
        Loader l = new Loader();

        // private-in-subclass: B.m is private, so `a.m()` on a B runs A.m.
        ClassWriter a = new ClassWriter("L3j/A", "java/lang/Object");
        a.defaultConstructor();
        a.returnsString(PUBLIC, "m", "A.m");
        Class<?> cA = l.define(a);
        ClassWriter b = new ClassWriter("L3j/B", "L3j/A");
        b.defaultConstructor();
        b.returnsString(PRIVATE, "m", "B.m");
        Class<?> cB = l.define(b);
        Class<?> callA = l.define(caller("L3j/Caller", "L3j/A"));
        run("private-in-subclass", callA, cA, cB.getConstructor().newInstance());

        // package-private: p2.B.m (public) does not override p1.A.m (package-private).
        ClassWriter pa = new ClassWriter("L3j/p1/A", "java/lang/Object");
        pa.defaultConstructor();
        pa.returnsString(0, "m", "p1.A.m");
        Class<?> cPA = l.define(pa);
        ClassWriter pb = new ClassWriter("L3j/p2/B", "L3j/p1/A");
        pb.defaultConstructor();
        pb.returnsString(PUBLIC, "m", "p2.B.m");
        Class<?> cPB = l.define(pb);
        Class<?> callP = l.define(caller("L3j/p1/Caller", "L3j/p1/A"));
        run("package-private", callP, cPA, cPB.getConstructor().newInstance());

        // public-override: the control, an ordinary override.
        ClassWriter c = new ClassWriter("L3j/C", "L3j/A");
        c.defaultConstructor();
        c.returnsString(PUBLIC, "m", "C.m");
        Class<?> cC = l.define(c);
        run("public-override", callA, cA, cC.getConstructor().newInstance());
    }
}
