// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 101, lane L5 -- `ClassLoader.defineClass(String,
// ByteBuffer, ProtectionDomain)` on the SYNTHETIC JDK. A real JDK runs its own
// bytecode body for this method; the synthetic JDK runs the registered native
// `classloader::cl_define_class_bb`, which handed the ByteBuffer OBJECT to the
// byte[] define (its "array length" and "elements" were the buffer object's)
// and the ProtectionDomain slot as the offset. Now it copies the buffer's
// remaining bytes as the JDK body does. The class bytes are a member-less
// class assembled in `payloadBytes` (no resource read, no java.io, no
// java.lang.classfile), and each loader defines its own copy of the same name.
// Each row catches its own failure, so a synthetic-JDK gap shows as one row's
// exception class rather than ending the run (wave 103).
//
// Run (no setup; the binary must be built with the `synthetic-jdk` Cargo
// feature, which is not in the default set):
//   cargo build --release -j 5 -p cratonvm-cli --features synthetic-jdk
//   javac --release 8 -d out L5W101DefineClassByteBufferSynthetic.java
//   cratonvm --synthetic-jdk -cp out L5W101DefineClassByteBufferSynthetic
//   cratonvm --synthetic-jdk --nojit -cp out L5W101DefineClassByteBufferSynthetic
//   java -cp out L5W101DefineClassByteBufferSynthetic          (HotSpot 25)
// The probe battery runs it on the real JDK in both modes (control; the JDK's
// bytecode body, not this native, answers there).
//
// Expected HotSpot 25 output:
//   heap-array=L5W101Payload own-loader=true pos=3
//   heap-readonly=L5W101Payload own-loader=true pos-at-limit=true
//   direct=L5W101Payload own-loader=true pos=3
//   null-buffer=java.lang.NullPointerException
// (The JDK body keeps the position of a heap buffer with an accessible array
// and of a direct buffer, and advances a read-only heap buffer's with
// `b.get(tb)`.)

import java.nio.ByteBuffer;

public class L5W101DefineClassByteBufferSynthetic {
    static final String NAME = "L5W101Payload";

    static final class L extends ClassLoader {
        L() {
            super(null);
        }

        Class<?> def(String name, ByteBuffer b) {
            return defineClass(name, b, null);
        }
    }

    /**
     * A public class `L5W101Payload` extending Object with no members (class-file
     * version 52), assembled by hand: no resource lookup, no java.io and no
     * java.lang.classfile, so the synthetic JDK needs nothing beyond arrays.
     */
    static byte[] payloadBytes() {
        int[] head = {0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52, 0, 5};
        byte[] out = new byte[128];
        int n = 0;
        for (int v : head) out[n++] = (byte) v;
        n = classEntry(out, n, 2);
        n = utf8(out, n, NAME);
        n = classEntry(out, n, 4);
        n = utf8(out, n, "java/lang/Object");
        int[] tail = {0x00, 0x21, 0, 1, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0};
        for (int v : tail) out[n++] = (byte) v;
        byte[] cls = new byte[n];
        System.arraycopy(out, 0, cls, 0, n);
        return cls;
    }

    static int classEntry(byte[] out, int n, int nameIndex) {
        out[n++] = 7;
        out[n++] = (byte) (nameIndex >> 8);
        out[n++] = (byte) nameIndex;
        return n;
    }

    static int utf8(byte[] out, int n, String s) {
        out[n++] = 1;
        out[n++] = (byte) (s.length() >> 8);
        out[n++] = (byte) s.length();
        for (int i = 0; i < s.length(); i++) out[n++] = (byte) s.charAt(i);
        return n;
    }

    // The class bytes after a 3-byte prefix, position 3, limit at the end.
    static ByteBuffer heap(byte[] cls) {
        byte[] padded = new byte[cls.length + 3];
        System.arraycopy(cls, 0, padded, 3, cls.length);
        ByteBuffer b = ByteBuffer.wrap(padded);
        b.position(3);
        return b;
    }

    interface Row {
        String run();
    }

    // One row per shape: a failure prints the row's exception and the next row runs.
    static void row(String label, Row r) {
        String v;
        try {
            v = r.run();
        } catch (Throwable t) {
            v = t.getClass().getName();
        }
        System.out.println(label + "=" + v);
    }

    public static void main(String[] args) {
        final byte[] cls = payloadBytes();

        row("heap-array", new Row() {
            public String run() {
                ByteBuffer h = heap(cls);
                L l = new L();
                Class<?> c = l.def(NAME, h);
                return c.getName() + " own-loader=" + (c.getClassLoader() == l) + " pos=" + h.position();
            }
        });
        row("heap-readonly", new Row() {
            public String run() {
                ByteBuffer ro = heap(cls).asReadOnlyBuffer();
                L l = new L();
                Class<?> c = l.def(NAME, ro);
                return c.getName() + " own-loader=" + (c.getClassLoader() == l)
                        + " pos-at-limit=" + (ro.position() == ro.limit());
            }
        });
        row("direct", new Row() {
            public String run() {
                ByteBuffer d = ByteBuffer.allocateDirect(cls.length + 3);
                d.put(new byte[3]);
                d.put(cls);
                d.position(3);
                L l = new L();
                Class<?> c = l.def(NAME, d);
                return c.getName() + " own-loader=" + (c.getClassLoader() == l) + " pos=" + d.position();
            }
        });
        row("null-buffer", new Row() {
            public String run() {
                new L().def(NAME, null);
                return "defined";
            }
        });
    }
}
