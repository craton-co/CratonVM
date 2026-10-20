// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L7 (hunt; page
// interpreter-i108-L7-code-attribute-parse-refusals-use-the-readers-words-FIXED-20261016): HotSpot's
// ClassFileParser refuses a Code attribute's length and exception-table ranges with its own
// ClassFormatError texts (`parse_method`, `parse_exception_table`), at define time:
//   code-length-0      a method whose code_length is 0
//                      "Invalid method Code length 0 in class file C"
//   handler-range      exception table [0, 9) in a 2-byte method
//                      "Illegal exception table range in class file C"
//   handler-pc         handler_pc 9 in a 2-byte method
//                      "Illegal exception table handler in class file C"
// CratonVM's reader refuses the same bytes with its own words ("Code attribute code_length 0
// outside valid range 1..=65535", "Code exception_table[0]: end_pc 9 exceeds code_length 2 ...").
// Each class is hand-assembled (major 69) and defined + initialised with Class.forName.
//
// Run: javac -d out L7W108ParseCodeTexts.java && java -cp out L7W108ParseCodeTexts
//      cratonvm [--compatible] [--nojit] -cp out L7W108ParseCodeTexts
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from classFileParser.cpp)
// code-length-0 -> java.lang.ClassFormatError: Invalid method Code length 0 in class file L7W108PcZero
// handler-range -> java.lang.ClassFormatError: Illegal exception table range in class file L7W108PcRange
// handler-pc -> java.lang.ClassFormatError: Illegal exception table handler in class file L7W108PcHandler
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W108ParseCodeTexts {

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
            super(L7W108ParseCodeTexts.class.getClassLoader());
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

    static byte[] withM(String name, byte[] code, int[][] handlers) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", 69).ctor("java/lang/Object");
        a.method(0x0009, "m", "()V", 1, 0, code, null, handlers);
        return a.build();
    }

    public static void main(String[] args) throws Exception {
        row("code-length-0", "L7W108PcZero", withM("L7W108PcZero", new byte[0], new int[0][]));
        // 0 aconst_null; 1 athrow
        row("handler-range", "L7W108PcRange", withM("L7W108PcRange", b(0x01, 0xbf), new int[][] {{0, 9, 0, 0}}));
        row("handler-pc", "L7W108PcHandler", withM("L7W108PcHandler", b(0x01, 0xbf), new int[][] {{0, 1, 9, 0}}));
    }
}
