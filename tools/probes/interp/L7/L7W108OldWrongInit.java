// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L7 (page i107-L7-constructor-call-verifier-rules, open
// item 1): libverify's text (class files below major 50, `check_code.c`) for a constructor call
// on the wrong class. HotSpot verifies these classes with the old type-inferencing verifier only,
// whose message is "(class: C, method: m signature: D) Call to wrong initialization method" for
// both shapes:
//   new-child-init-parent   new Child; dup; invokespecial Parent.<init>   (ITEM_NewObject)
//   grandchild-super        aload_0; invokespecial <grandparent>.<init>    (ITEM_InitObject)
//   jsr-new-child-init      the first shape in a method that also has a jsr / ret subroutine
//                           (CratonVM's subroutine walk, verify_subroutines.rs)
//   new-child-init-child    the well-formed control row
// CratonVM before wave 108 printed its own reason in libverify's format ("Call to wrong <init>
// method: the new at 0 creates ...").
//
// The classes are hand-assembled (major 49, no StackMapTable) by the small writer below, so the
// bytes are exactly what each row says; every class is defined by one loader and initialised with
// Class.forName (link = verify).
//
// Run: javac -d out L7W108OldWrongInit.java && java -cp out L7W108OldWrongInit
//      cratonvm [--compatible] [--nojit] -cp out L7W108OldWrongInit
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from check_code.c)
// new-child-init-parent -> java.lang.VerifyError: (class: L7W108OwWrong, method: m signature: ()V) Call to wrong initialization method
// grandchild-super -> java.lang.VerifyError: (class: L7W108OwGrand, method: <init> signature: ()V) Call to wrong initialization method
// jsr-new-child-init -> java.lang.VerifyError: (class: L7W108OwJsr, method: m signature: ()V) Call to wrong initialization method
// new-child-init-child -> ok
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W108OldWrongInit {

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
            super(L7W108OldWrongInit.class.getClassLoader());
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

    static final int MAJOR = 49;

    static Map<String, byte[]> hierarchy() throws IOException {
        Map<String, byte[]> m = new HashMap<>();
        m.put("L7W108OwParent", new Asm("L7W108OwParent", "java/lang/Object", MAJOR)
            .ctor("java/lang/Object").build());
        m.put("L7W108OwChild", new Asm("L7W108OwChild", "L7W108OwParent", MAJOR)
            .ctor("L7W108OwParent").build());
        return m;
    }

    static void row(String label, String binaryName, byte[] bytes) throws IOException {
        Map<String, byte[]> all = hierarchy();
        all.put(binaryName, bytes);
        try {
            Class.forName(binaryName, true, new Loader(all));
            System.out.println(label + " -> ok");
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage());
            System.out.println(label + " -> " + t.getClass().getName() + ": " + msg.split("\n")[0]);
        }
    }

    /** A class with `<init>` and a static `m()V` of the given code. */
    static byte[] withM(String name, int maxStack, int maxLocals, java.util.function.Function<Asm, byte[]> code)
            throws IOException {
        Asm a = new Asm(name, "java/lang/Object", MAJOR).ctor("java/lang/Object");
        a.method(0x0009, "m", "()V", maxStack, maxLocals, code.apply(a), null, new int[0][]);
        return a.build();
    }

    static byte[] newInit(Asm a, String created, String ctorOwner, int... tail) {
        try {
            int c = a.cls(created);
            int m = a.methodref(ctorOwner, "<init>", "()V");
            int[] head = {0xbb, c >> 8, c & 0xff, 0x59, 0xb7, m >> 8, m & 0xff, 0x57};
            int[] all = new int[head.length + tail.length];
            System.arraycopy(head, 0, all, 0, head.length);
            System.arraycopy(tail, 0, all, head.length, tail.length);
            return b(all);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    public static void main(String[] args) throws Exception {
        row("new-child-init-parent", "L7W108OwWrong", withM("L7W108OwWrong", 2, 0,
            a -> newInit(a, "L7W108OwChild", "L7W108OwParent", 0xb1)));
        row("grandchild-super", "L7W108OwGrand", new Asm("L7W108OwGrand", "L7W108OwChild", MAJOR)
            .ctor("L7W108OwParent").build());
        // 0 new; 3 dup; 4 invokespecial; 7 pop; 8 jsr 12; 11 return; 12 astore_0; 13 ret 0
        row("jsr-new-child-init", "L7W108OwJsr", withM("L7W108OwJsr", 2, 1,
            a -> newInit(a, "L7W108OwChild", "L7W108OwParent", 0xa8, 0x00, 0x04, 0xb1, 0x4b, 0xa9, 0x00)));
        row("new-child-init-child", "L7W108OwRight", withM("L7W108OwRight", 2, 0,
            a -> newInit(a, "L7W108OwChild", "L7W108OwChild", 0xb1)));
    }
}
