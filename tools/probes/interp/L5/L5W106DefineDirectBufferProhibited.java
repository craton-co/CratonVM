// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- `ClassLoader.defineClass(null,
// ByteBuffer, null)` of a class in a `java.*` package from a user loader.
// A heap buffer goes to `defineClass1` (wave 42 gave that native HotSpot's
// `ClassFileParser` text); a DIRECT buffer goes to `defineClass2`, which kept
// the VM's own text: `Prohibited package name: java.l5w106.Spoof (non-bootstrap
// loader ... cannot define a class in a protected platform package)`.
// A named define is refused by `preDefineClass` in Java first (control row).
// `@<hash>` is masked to `@H`.
//
// Run (no setup):
//   javac -d out L5W106DefineDirectBufferProhibited.java
//   cratonvm --jdk-only   [--nojit] -cp out L5W106DefineDirectBufferProhibited
//   cratonvm --compatible [--nojit] -cp out L5W106DefineDirectBufferProhibited
//   java -cp out L5W106DefineDirectBufferProhibited            (HotSpot 25)
// (--compatible prints HotSpot's text only where `nameAndId` is known to the
// VM and `CRATONVM_COMPAT_MODULE_MESSAGES` is on; compare with HotSpot anyway.)
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources (`SystemDictionary::
// check_prohibited_package` text, as L5W42DefineClassErrors measured for the
// heap row on 25.0.3), not run.
// HOTSPOT_EXPECTED_BEGIN
// heap-null-name-named=java.lang.SecurityException: Class loader (instance of): 'o' @H tried to load prohibited package name: java.l5w106
// direct-null-name-named=java.lang.SecurityException: Class loader (instance of): 'o' @H tried to load prohibited package name: java.l5w106
// direct-null-name-unnamed=java.lang.SecurityException: Class loader (instance of): L5W106DefineDirectBufferProhibited$Open @H tried to load prohibited package name: java.l5w106
// direct-named=java.lang.SecurityException: Prohibited package name: java.l5w106
// HOTSPOT_EXPECTED_END

import java.nio.ByteBuffer;

public class L5W106DefineDirectBufferProhibited {
    static final String NAME = "java/l5w106/Spoof";

    static class Open extends ClassLoader {
        Open() {
            super(null);
        }

        Open(String name) {
            super(name, null);
        }

        Class<?> def(String name, ByteBuffer b) {
            return defineClass(name, b, null);
        }
    }

    /** `public class java/l5w106/Spoof extends Object {}`, version 52, by hand. */
    static byte[] classBytes() {
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

    static ByteBuffer direct(byte[] cls) {
        ByteBuffer d = ByteBuffer.allocateDirect(cls.length);
        d.put(cls);
        d.flip();
        return d;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String label, Row r) {
        String v;
        try {
            v = "defined " + r.run();
        } catch (Throwable t) {
            v = t.getClass().getName() + ": " + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
        }
        System.out.println(label + "=" + v);
    }

    public static void main(String[] args) {
        final byte[] cls = classBytes();
        row("heap-null-name-named", () -> new Open("o").def(null, ByteBuffer.wrap(cls)));
        row("direct-null-name-named", () -> new Open("o").def(null, direct(cls)));
        row("direct-null-name-unnamed", () -> new Open().def(null, direct(cls)));
        row("direct-named", () -> new Open("o").def("java.l5w106.Spoof", direct(cls)));
    }
}
