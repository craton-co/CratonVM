// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L7 (page i107-L7-constructor-call-verifier-rules, open
// item 2b): HotSpot reads a method's whole StackMapTable before the type walk, and an
// `Uninitialized(offset)` entry whose offset is not the bci of a `new` is a ClassFormatError
// (`StackMapReader::parse_verification_type`): "StackMapTable format error: bad offset for
// Uninitialized in method '<external signature>'". CratonVM accepted the table before wave 108.
//   offset-not-new   0 iconst_0; 1 ifeq 4; 4 return; frame @4 full_frame{locals {}, stack
//                    {Uninitialized(0)}} -- the frame is never matched (the walk refuses first on
//                    HotSpot only because the table is read first).
//   offset-new       0 new Object; 3 iconst_0; 4 ifeq 7; 7 pop; 8 return; frame @7
//                    full_frame{locals {}, stack {Uninitialized(0)}}: ok
// The classes are hand-assembled (major 69) and initialised with Class.forName (link = verify).
//
// Run: javac -d out L7W108UninitializedOffset.java && java -cp out L7W108UninitializedOffset
//      cratonvm [--compatible] [--nojit] -cp out L7W108UninitializedOffset
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from stackMapTable.cpp)
// offset-not-new -> java.lang.ClassFormatError: StackMapTable format error: bad offset for Uninitialized in method 'void L7W108UoBad.m()'
// offset-new -> ok
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W108UninitializedOffset {

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
            super(L7W108UninitializedOffset.class.getClassLoader());
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

    static void row(String label, String binaryName, byte[] bytes) {
        Map<String, byte[]> all = new HashMap<>();
        all.put(binaryName, bytes);
        try {
            Class.forName(binaryName, true, new Loader(all));
            System.out.println(label + " -> ok");
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage());
            System.out.println(label + " -> " + t.getClass().getName() + ": " + msg.split("\n")[0]);
        }
    }

    public static void main(String[] args) throws Exception {
        Asm bad = new Asm("L7W108UoBad", "java/lang/Object", 69).ctor("java/lang/Object");
        // full_frame(255), offset_delta 4, 0 locals, 1 stack item: Uninitialized(8) offset 0
        bad.method(0x0009, "m", "()V", 1, 0, b(0x03, 0x99, 0x00, 0x03, 0xb1),
            b(0x00, 0x01, 0xff, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00), new int[0][]);
        row("offset-not-new", "L7W108UoBad", bad.build());

        Asm good = new Asm("L7W108UoGood", "java/lang/Object", 69).ctor("java/lang/Object");
        int obj = good.cls("java/lang/Object");
        good.method(0x0009, "m", "()V", 2, 0,
            b(0xbb, obj >> 8, obj & 0xff, 0x03, 0x99, 0x00, 0x03, 0x57, 0xb1),
            b(0x00, 0x01, 0xff, 0x00, 0x07, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00), new int[0][]);
        row("offset-new", "L7W108UoGood", good.build());
    }
}
