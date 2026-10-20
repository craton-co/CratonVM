// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 111, lane L7: how JDK 25's StackMapTable constructor bounds its
// reading (measured on w110: `number_of_entries = 1` with no frame bytes is "wrong attribute
// size", a count of 284 over a short table is "access beyond the end of attribute"). Frames are
// read while the attribute has bytes; a frame past the count is "wrong attribute size"
// (`check_size`), and so is a count not reached when the bytes run out (`check_end`); a count of
// 0 reads nothing. Each method is `nop; nop; return` (pcs 0, 1, 2), major 69.
//   count-1-no-frames      {0,1}                      (the w110 smt-truncated row)
//   count-2-one-frame      {0,2, 0}                   one same_frame(@0), count not reached
//   count-1-two-frames     {0,1, 0, 0}                same_frame(@0), same_frame(@1): past the count
//   count-2-cut-frame      {0,2, 0, 255,0}            second frame cut inside its offset_delta
//   count-0-trailing       {0,0, 0}                   count 0: nothing is read
//   count-2-two-frames     {0,2, 0, 0}                control: well formed
// Run: javac -d out L7W111StackMapCount.java && java -cp out L7W111StackMapCount
//      cratonvm [--compatible] [--nojit] -cp out L7W111StackMapCount
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from stackMapTable.cpp, the HotSpot run is the reference)
// count-1-no-frames -> java.lang.ClassFormatError: StackMapTable format error: wrong attribute size
// count-2-one-frame -> java.lang.ClassFormatError: StackMapTable format error: wrong attribute size
// count-1-two-frames -> java.lang.ClassFormatError: StackMapTable format error: wrong attribute size
// count-2-cut-frame -> java.lang.ClassFormatError: StackMapTable format error: access beyond the end of attribute
// count-0-trailing -> ok
// count-2-two-frames -> ok
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W111StackMapCount {

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

        int member(int tag, String owner, String mname, String desc) throws IOException {
            String key = "M" + tag + owner + "." + mname + desc;
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
            cp.writeByte(tag);
            cp.writeShort(c);
            cp.writeShort(nat);
            idx.put(key, next);
            return next++;
        }

        int methodref(String owner, String mname, String desc) throws IOException {
            return member(10, owner, mname, desc);
        }

        int fieldref(String owner, String fname, String desc) throws IOException {
            return member(9, owner, fname, desc);
        }

        /** A method; `stackMap` may be null. */
        Asm method(int flags, String mname, String desc, int maxStack, int maxLocals, byte[] code,
                byte[] stackMap) throws IOException {
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
            out.writeInt(12 + code.length + smtLen);
            out.writeShort(maxStack);
            out.writeShort(maxLocals);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(0);
            out.writeShort(stackMap == null ? 0 : 1);
            if (stackMap != null) {
                out.writeShort(smtU);
                out.writeInt(stackMap.length);
                out.write(stackMap);
            }
            methods.add(bytes.toByteArray());
            return this;
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
            super(L7W111StackMapCount.class.getClassLoader());
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

    static void row(String label, String internalName, byte[] bytes) {
        String binaryName = internalName.replace('/', '.');
        Map<String, byte[]> all = new HashMap<>();
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
                    System.out.println("  " + lines[i + 1].trim());
                }
            }
        }
    }

    interface Gen {
        byte[] code(Asm a) throws IOException;
    }

    static void tableRow(String label, String name, byte[] stackMap) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", 69);
        a.method(0x0009, "m", "()V", 0, 0, b(0x00, 0x00, 0xb1), stackMap);
        row(label, name, a.build());
    }

    public static void main(String[] args) throws Exception {
        tableRow("count-1-no-frames", "L7W111C1None", b(0, 1));
        tableRow("count-2-one-frame", "L7W111C2One", b(0, 2, 0));
        tableRow("count-1-two-frames", "L7W111C1Two", b(0, 1, 0, 0));
        tableRow("count-2-cut-frame", "L7W111C2Cut", b(0, 2, 0, 255, 0));
        tableRow("count-0-trailing", "L7W111C0Tail", b(0, 0, 0));
        tableRow("count-2-two-frames", "L7W111C2Ok", b(0, 2, 0, 0));
    }
}
