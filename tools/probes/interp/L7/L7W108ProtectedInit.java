// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L7 (page i107-L7-constructor-call-verifier-rules, open
// item 2a): HotSpot's `verify_invoke_init` protected rule. A `protected` constructor of a proper
// superclass declared in another run-time package may only initialise an object of the current
// class (or a subclass): `new Base; dup; invokespecial Base.<init>` inside `Sub extends Base`
// from another package is "Bad access to protected <init> method" (javac refuses the source
// form, "Base() has protected access"). CratonVM verified the class before wave 108.
//   other-package     l7w108b.Sub extends l7w108a.Base: refused
//   same-package      l7w108a.Peer extends l7w108a.Base: same run-time package, ok
//   own-class         Sub.m2: new Sub; invokespecial Sub.<init>: ok
// The classes are hand-assembled (major 69, straight-line code, no StackMapTable needed), defined
// by one loader and initialised with Class.forName (link = verify; nothing runs `m`).
//
// Run: javac -d out L7W108ProtectedInit.java && java -cp out L7W108ProtectedInit
//      cratonvm [--compatible] [--nojit] -cp out L7W108ProtectedInit
// Each row prints the header line, then the Location and Reason lines ("(constant pool N)"
// printed as "(constant pool #)").
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from verifier.cpp)
// other-package -> java.lang.VerifyError: Bad access to protected <init> method
//   l7w108b/Sub.m()V @4: invokespecial
//   Type 'l7w108a/Base' (constant pool #) is not assignable to 'l7w108b/Sub'
// same-package -> ok
// own-class -> ok
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W108ProtectedInit {

    /** A minimal class-file writer: one class, any number of methods. */
    static final class Asm {
        final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        final DataOutputStream cp = new DataOutputStream(cpBytes);
        final Map<String, Integer> idx = new HashMap<>();
        final List<byte[]> methods = new ArrayList<>();
        int next = 1;
        final String name;
        final String superName;
        final int major;

        Asm(String name, String superName, int major) {
            this.name = name;
            this.superName = superName;
            this.major = major;
        }

        int utf8(String s) throws IOException {
            Integer i = idx.get("U" + s);
            if (i != null) {
                return i;
            }
            cp.writeByte(1);
            cp.writeUTF(s);
            idx.put("U" + s, next);
            return next++;
        }

        int cls(String n) throws IOException {
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

        int methodref(String owner, String mname, String desc) throws IOException {
            String key = "M" + owner + "." + mname + desc;
            Integer i = idx.get(key);
            if (i != null) {
                return i;
            }
            int c = cls(owner);
            int n = utf8(mname);
            int d = utf8(desc);
            cp.writeByte(12);
            cp.writeShort(n);
            cp.writeShort(d);
            int nat = next++;
            cp.writeByte(10);
            cp.writeShort(c);
            cp.writeShort(nat);
            idx.put(key, next);
            return next++;
        }

        /** A method; `stackMap` may be null; handlers are {start, end, handler, catchType}. */
        Asm method(int flags, String mname, String desc, int maxStack, int maxLocals, byte[] code,
                byte[] stackMap, int[][] handlers) throws IOException {
            int nameU = utf8(mname);
            int descU = utf8(desc);
            int codeU = utf8("Code");
            int smtU = stackMap == null ? 0 : utf8("StackMapTable");
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
            out.writeShort(flags);
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
            out.writeShort(handlers.length);
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
            methods.add(bytes.toByteArray());
            return this;
        }

        /** `<init>()V` calling `target.<init>()V` on `this`. */
        Asm ctor(String target) throws IOException {
            int m = methodref(target, "<init>", "()V");
            return method(0x0001, "<init>", "()V", 1, 1, b(0x2a, 0xb7, m >> 8, m & 0xff, 0xb1), null,
                new int[0][]);
        }

        byte[] build() throws IOException {
            int thisC = cls(name);
            int superC = cls(superName);
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
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
            out.writeShort(methods.size());
            for (byte[] m : methods) {
                out.write(m);
            }
            out.writeShort(0);
            return bytes.toByteArray();
        }
    }

    static byte[] b(int... v) {
        byte[] r = new byte[v.length];
        for (int i = 0; i < v.length; i++) {
            r[i] = (byte) v[i];
        }
        return r;
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> classes;

        Loader(Map<String, byte[]> classes) {
            super(L7W108ProtectedInit.class.getClassLoader());
            this.classes = classes;
        }

        @Override
        protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] bytes = classes.get(n);
            if (bytes == null) {
                throw new ClassNotFoundException(n);
            }
            return defineClass(n, bytes, 0, bytes.length);
        }
    }

    static final int MAJOR = 69;

    static Map<String, byte[]> base() throws IOException {
        Map<String, byte[]> m = new HashMap<>();
        Asm base = new Asm("l7w108a/Base", "java/lang/Object", MAJOR);
        int oi = base.methodref("java/lang/Object", "<init>", "()V");
        // protected <init>()V
        base.method(0x0004, "<init>", "()V", 1, 1, b(0x2a, 0xb7, oi >> 8, oi & 0xff, 0xb1), null,
            new int[0][]);
        m.put("l7w108a.Base", base.build());
        return m;
    }

    static void row(String label, String binaryName, byte[] bytes) throws IOException {
        Map<String, byte[]> all = base();
        all.put(binaryName, bytes);
        try {
            Class.forName(binaryName, true, new Loader(all));
            System.out.println(label + " -> ok");
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage());
            String[] lines = msg.split("\n");
            System.out.println(label + " -> " + t.getClass().getName() + ": " + lines[0]);
            for (int i = 0; i + 1 < lines.length; i++) {
                String l = lines[i].trim();
                if (l.equals("Location:") || l.equals("Reason:")) {
                    System.out.println("  " + lines[i + 1].trim()
                        .replaceAll("\\(constant pool \\d+\\)", "(constant pool #)"));
                }
            }
        }
    }

    /** `name extends l7w108a/Base` with a public `<init>` and a static `m()V` creating `created`. */
    static byte[] sub(String name, String created) throws IOException {
        Asm a = new Asm(name, "l7w108a/Base", MAJOR).ctor("l7w108a/Base");
        int c = a.cls(created);
        int m = a.methodref(created, "<init>", "()V");
        // 0 new; 3 dup; 4 invokespecial; 7 pop; 8 return
        a.method(0x0009, "m", "()V", 2, 0,
            b(0xbb, c >> 8, c & 0xff, 0x59, 0xb7, m >> 8, m & 0xff, 0x57, 0xb1), null, new int[0][]);
        return a.build();
    }

    public static void main(String[] args) throws Exception {
        row("other-package", "l7w108b.Sub", sub("l7w108b/Sub", "l7w108a/Base"));
        row("same-package", "l7w108a.Peer", sub("l7w108a/Peer", "l7w108a/Base"));
        row("own-class", "l7w108b.Own", sub("l7w108b/Own", "l7w108b/Own"));
    }
}
