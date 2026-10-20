// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L7 (page i107-L7-constructor-call-verifier-rules, open
// item 3, and the flag rules it needs): HotSpot's split verifier keeps `flagThisUninit` per frame
// (set on a constructor's entry, cleared only by the `<init>` call on `uninitializedThis`, taken
// from every declared frame) and compares it at every frame match: a frame that has the flag is
// not assignable to a declared frame without it ("Current frame's flags are not assignable to
// stack map frame's."). An `invokespecial <init>` on `uninitializedThis` inside a `try` range is
// checked against its handlers twice: with the incoming locals and the flag, then after the call
// with the flag still set. CratonVM compared locals only, so before wave 108 it ACCEPTED rows 2,
// 3 and 4 (each a constructor that can return without running super()):
//   1 super-in-try-uninit-handler   handler frame {uninitializedThis}: the post-call check sees
//                                   'C' in locals[0] (refused on both VMs; the printed Current
//                                   Frame differs: HotSpot prints flagThisUninit)
//   2 super-in-try-bare-handler     handler frame {} (pop; return): the pre-call check's flags
//   3 overwrite-this-then-return    aconst_null; astore_0; return: the flag survives the store
//   4 branch-around-super           iconst_0; ifeq L; aload_0; invokespecial; L: return with
//                                   frame L {}: the branch's flags
//   5 try-before-super              a try range before super() with a handler frame
//                                   {uninitializedThis}: ok (no false refusal)
//   6 plain-ctor                    ok
// Kill switch: CRATONVM_VERIFY_THIS_UNINIT_FLAGS=0 (rows 2-4 load again).
// The classes are hand-assembled (major 69) and initialised with Class.forName (link = verify).
//
// Run: javac -d out L7W108ThisUninitFlags.java && java -cp out L7W108ThisUninitFlags
//      cratonvm [--compatible] [--nojit] -cp out L7W108ThisUninitFlags
// Each row prints the header line, then the Location and Reason lines.
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from verifier.cpp / stackMapFrame.cpp)
// super-in-try-uninit-handler -> java.lang.VerifyError: Stack map does not match the one at exception handler 5
//   L7W108TuA.<init>()V @5: pop
//   Type 'L7W108TuA' (current frame, locals[0]) is not assignable to uninitializedThis (stack map, locals[0])
// super-in-try-bare-handler -> java.lang.VerifyError: Stack map does not match the one at exception handler 5
//   L7W108TuB.<init>()V @5: pop
//   Current frame's flags are not assignable to stack map frame's.
// overwrite-this-then-return -> java.lang.VerifyError: Constructor must call super() or this() before return
//   L7W108TuC.<init>()V @2: return
//   Error exists in the bytecode
// branch-around-super -> java.lang.VerifyError: Inconsistent stackmap frames at branch target 8
//   L7W108TuD.<init>()V @8: return
//   Current frame's flags are not assignable to stack map frame's.
// try-before-super -> ok
// plain-ctor -> ok
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W108ThisUninitFlags {

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
            super(L7W108ThisUninitFlags.class.getClassLoader());
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

    /** A class `name` whose only method is `<init>()V`; `handler` is {start, end, handler, catch} or null. */
    static byte[] ctorClass(String name, int maxStack, int maxLocals, int[] handler, CodeGen gen)
            throws IOException {
        Asm a = new Asm(name, "java/lang/Object", 69);
        int oi = a.methodref("java/lang/Object", "<init>", "()V");
        int thr = a.cls("java/lang/Throwable");
        byte[][] parts = gen.make(oi, thr);
        int[][] handlers = handler == null ? new int[0][] : new int[][] {handler};
        a.method(0x0001, "<init>", "()V", maxStack, maxLocals, parts[0], parts[1], handlers);
        return a.build();
    }

    interface CodeGen {
        /** {code, stackMap or null}. */
        byte[][] make(int objectInit, int throwable);
    }

    public static void main(String[] args) throws Exception {
        // 1. 0 aload_0; 1 invokespecial Object.<init>; 4 return; 5 pop; 6 aconst_null; 7 athrow.
        //    try [1, 4) -> 5, any. Frame @5: full_frame{locals {uninitializedThis}, stack {Throwable}}.
        row("super-in-try-uninit-handler", "L7W108TuA", ctorClass("L7W108TuA", 1, 1, new int[] {1, 4, 5, 0}, (oi, thr) -> new byte[][] {
            b(0x2a, 0xb7, oi >> 8, oi & 0xff, 0xb1, 0x57, 0x01, 0xbf),
            b(0x00, 0x01, 0xff, 0x00, 0x05, 0x00, 0x01, 0x06, 0x00, 0x01, 0x07, thr >> 8, thr & 0xff)}));
        // 2. As 1, frame @5: full_frame{locals {}, stack {Throwable}}; handler 5 pop; 6 return.
        row("super-in-try-bare-handler", "L7W108TuB", ctorClass("L7W108TuB", 1, 1, new int[] {1, 4, 5, 0}, (oi, thr) -> new byte[][] {
            b(0x2a, 0xb7, oi >> 8, oi & 0xff, 0xb1, 0x57, 0xb1),
            b(0x00, 0x01, 0xff, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x07, thr >> 8, thr & 0xff)}));
        // 3. 0 aconst_null; 1 astore_0; 2 return.
        row("overwrite-this-then-return", "L7W108TuC", ctorClass("L7W108TuC", 1, 1, null, (oi, thr) -> new byte[][] {
            b(0x01, 0x4b, 0xb1), null}));
        // 4. 0 iconst_0; 1 ifeq 8; 4 aload_0; 5 invokespecial Object.<init>; 8 return.
        //    Frame @8: full_frame{locals {}, stack {}}.
        row("branch-around-super", "L7W108TuD", ctorClass("L7W108TuD", 1, 1, null, (oi, thr) -> new byte[][] {
            b(0x03, 0x99, 0x00, 0x07, 0x2a, 0xb7, oi >> 8, oi & 0xff, 0xb1),
            b(0x00, 0x01, 0xff, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00)}));
        // 5. 0 iconst_1; 1 istore_1; 2 aload_0; 3 invokespecial; 6 return; 7 athrow.
        //    try [0, 2) -> 7, any. Frame @7: full_frame{locals {uninitializedThis}, stack {Throwable}}.
        row("try-before-super", "L7W108TuE", ctorClass("L7W108TuE", 1, 2, new int[] {0, 2, 7, 0}, (oi, thr) -> new byte[][] {
            b(0x04, 0x3c, 0x2a, 0xb7, oi >> 8, oi & 0xff, 0xb1, 0xbf),
            b(0x00, 0x01, 0xff, 0x00, 0x07, 0x00, 0x01, 0x06, 0x00, 0x01, 0x07, thr >> 8, thr & 0xff)}));
        // 6. 0 aload_0; 1 invokespecial; 4 return.
        row("plain-ctor", "L7W108TuF", ctorClass("L7W108TuF", 1, 1, null, (oi, thr) -> new byte[][] {
            b(0x2a, 0xb7, oi >> 8, oi & 0xff, 0xb1), null}));
    }
}
