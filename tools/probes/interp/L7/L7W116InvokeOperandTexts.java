// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe
//
// Interpreter round i6 wave 116, lane L7 (verifier review): the invokeinterface operand
// checks of HotSpot's `verify_invoke_instructions` (split verifier) and libverify's
// `verify_opcode_operands` (major < 50). Hand-built classes, each one static method:
//
//   count-ok-69        aload_0; invokeinterface Runnable.run()V count 1: loads (control)
//   count-high-69      the same with count 2: "Inconsistent args count operand in invokeinterface"
//                      (CratonVM before wave 116 accepted it)
//   count-zero-69      count 0: the same refusal
//   long-ok-69         (LongConsumer, J): invokeinterface accept(J)V count 3: loads
//   long-low-69        the same with count 2 (one slot per argument): refused
//   count-high-49      major 49: "Inconsistent args_size for invokeinterface"
//   fourth-69          count 1, fourth operand byte 1: "Fourth operand byte of invokeinterface
//                      must be zero" (CratonVM said "Bad instruction")
//   fourth-49          the same, major 49 (CratonVM gave its own decoder text)
//
// Kill switch for the count rule: CRATONVM_VERIFY_CONTROL_RULES=0 (count-* load again).
// Run: javac -d out L7W116InvokeOperandTexts.java && java -cp out L7W116InvokeOperandTexts
//      cratonvm [--compatible] [--nojit] -cp out L7W116InvokeOperandTexts
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// count-ok-69 -> ok
// count-high-69 -> java.lang.VerifyError: Inconsistent args count operand in invokeinterface
//   L7W116CountHigh.m(Ljava/lang/Runnable;)V @1: invokeinterface
//   Error exists in the bytecode
// count-zero-69 -> java.lang.VerifyError: Inconsistent args count operand in invokeinterface
//   L7W116CountZero.m(Ljava/lang/Runnable;)V @1: invokeinterface
//   Error exists in the bytecode
// long-ok-69 -> ok
// long-low-69 -> java.lang.VerifyError: Inconsistent args count operand in invokeinterface
//   L7W116LongLow.m(Ljava/util/function/LongConsumer;J)V @2: invokeinterface
//   Error exists in the bytecode
// count-high-49 -> java.lang.VerifyError: (class: L7W116CountHigh49, method: m signature: (Ljava/lang/Runnable;)V) Inconsistent args_size for invokeinterface
// fourth-69 -> java.lang.VerifyError: Fourth operand byte of invokeinterface must be zero
//   L7W116Fourth.m(Ljava/lang/Runnable;)V @1: invokeinterface
//   Error exists in the bytecode
// fourth-49 -> java.lang.VerifyError: (class: L7W116Fourth49, method: m signature: (Ljava/lang/Runnable;)V) Fourth operand byte of invokeinterface must be zero
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W116InvokeOperandTexts {

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

        int ifaceMethodref(String owner, String mname, String desc) throws IOException {
            return member(11, owner, mname, desc);
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
            super(L7W116InvokeOperandTexts.class.getClassLoader());
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
        all.putAll(EXTRA);
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

    static final Map<String, byte[]> EXTRA = new HashMap<>();

    static void staticRow(String label, String name, int major, String desc, int maxStack,
            int maxLocals, Gen gen) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", major);
        a.method(0x0009, "m", desc, maxStack, maxLocals, gen.code(a), null);
        row(label, name, a.build());
    }

    interface Gen {
        byte[] code(Asm a) throws IOException;
    }

    /** aload_0; invokeinterface Runnable.run()V count, fourth; return */
    static Gen run(int count, int fourth) {
        return a -> {
            int m = a.ifaceMethodref("java/lang/Runnable", "run", "()V");
            return b(0x2a, 0xb9, m >> 8, m & 0xff, count, fourth, 0xb1);
        };
    }

    /** aload_0; lload_1; invokeinterface LongConsumer.accept(J)V count, 0; return */
    static Gen accept(int count) {
        return a -> {
            int m = a.ifaceMethodref("java/util/function/LongConsumer", "accept", "(J)V");
            return b(0x2a, 0x1f, 0xb9, m >> 8, m & 0xff, count, 0, 0xb1);
        };
    }

    public static void main(String[] args) throws Exception {
        String r = "(Ljava/lang/Runnable;)V";
        String l = "(Ljava/util/function/LongConsumer;J)V";
        staticRow("count-ok-69", "L7W116CountOk", 69, r, 1, 1, run(1, 0));
        staticRow("count-high-69", "L7W116CountHigh", 69, r, 1, 1, run(2, 0));
        staticRow("count-zero-69", "L7W116CountZero", 69, r, 1, 1, run(0, 0));
        staticRow("long-ok-69", "L7W116LongOk", 69, l, 3, 3, accept(3));
        staticRow("long-low-69", "L7W116LongLow", 69, l, 3, 3, accept(2));
        staticRow("count-high-49", "L7W116CountHigh49", 49, r, 1, 1, run(2, 0));
        staticRow("fourth-69", "L7W116Fourth", 69, r, 1, 1, run(1, 1));
        staticRow("fourth-49", "L7W116Fourth49", 49, r, 1, 1, run(1, 1));
    }
}
