// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- the residue of
// `interpreter-i101-L5-synthetic-define-class-bytebuffer-reads-the-buffer-object`
// (docs/internal/fixed-bugs/..., FIXED 20261016): on the SYNTHETIC JDK a heap
// ByteBuffer view at a non-zero array base (`slice()` of a positioned buffer,
// then `asReadOnlyBuffer()`, or a slice of a slice) had no backing storage, so
// its first `get` raised `internal error: ByteBuffer missing backing storage`.
// The fabricated 10-slot `java/nio/ByteBuffer` has no `offset` field; wave 106
// keeps the base in `native_io::BB_STUB_HEAP_OFFSET_SLOT` (servlet.rs
// `s2_bb_new_heap_view` / `s2_bb_heap_base`, native-io
// `bb_resolve_heap_offset`). Each row catches its own failure.
//
// Run (no setup; the binary must be built with the `synthetic-jdk` Cargo
// feature, in its own target dir):
//   cargo build --release -j 5 -p cratonvm-cli --features synthetic-jdk
//   javac --release 8 -d out L5W106SyntheticSliceReadOnly.java
//   cratonvm --synthetic-jdk -cp out L5W106SyntheticSliceReadOnly
//   cratonvm --synthetic-jdk --nojit -cp out L5W106SyntheticSliceReadOnly
//   java -cp out L5W106SyntheticSliceReadOnly                 (HotSpot 25)
// The battery runs it on the real JDK in both modes as the control.
//
// HOTSPOT_EXPECTED_BEGIN
// slice-first=cafebabe remaining-ok=true
// slice-of-slice=febabe00
// slice-readonly-first=cafebabe readonly=true
// slice-readonly-define=L5W106Payload own-loader=true pos-at-limit=true
// HOTSPOT_EXPECTED_END

import java.nio.ByteBuffer;

public class L5W106SyntheticSliceReadOnly {
    static final String NAME = "L5W106Payload";

    static final class L extends ClassLoader {
        L() {
            super(null);
        }

        Class<?> def(String name, ByteBuffer b) {
            return defineClass(name, b, null);
        }
    }

    /** `public class L5W106Payload extends Object {}`, version 52, by hand. */
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

    /** The class bytes after a 3-byte prefix, position 3. */
    static ByteBuffer positioned(byte[] cls) {
        byte[] padded = new byte[cls.length + 3];
        System.arraycopy(cls, 0, padded, 3, cls.length);
        ByteBuffer b = ByteBuffer.wrap(padded);
        b.position(3);
        return b;
    }

    static String hex(int b) {
        String s = Integer.toHexString(b & 0xff);
        return s.length() == 1 ? "0" + s : s;
    }

    /** Four relative gets, as hex. */
    static String first4(ByteBuffer b) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 4; i++) sb.append(hex(b.get()));
        return sb.toString();
    }

    interface Row {
        String run();
    }

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

        row("slice-first", new Row() {
            public String run() {
                ByteBuffer s = positioned(cls).slice();
                boolean remainingOk = s.remaining() == cls.length;
                return first4(s) + " remaining-ok=" + remainingOk;
            }
        });
        row("slice-of-slice", new Row() {
            public String run() {
                ByteBuffer s = positioned(cls).slice();
                s.position(1);
                ByteBuffer t = s.slice();
                StringBuilder sb = new StringBuilder();
                for (int i = 0; i < 4; i++) sb.append(hex(t.get(i)));
                return sb.toString();
            }
        });
        row("slice-readonly-first", new Row() {
            public String run() {
                ByteBuffer ro = positioned(cls).slice().asReadOnlyBuffer();
                return first4(ro) + " readonly=" + ro.isReadOnly();
            }
        });
        row("slice-readonly-define", new Row() {
            public String run() {
                ByteBuffer ro = positioned(cls).slice().asReadOnlyBuffer();
                L l = new L();
                Class<?> c = l.def(NAME, ro);
                return c.getName() + " own-loader=" + (c.getClassLoader() == l)
                        + " pos-at-limit=" + (ro.position() == ro.limit());
            }
        });
    }
}
