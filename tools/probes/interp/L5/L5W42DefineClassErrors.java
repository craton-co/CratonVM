// Interpreter round i1, wave 42, lane L5 -- `ClassLoader.defineClass` error
// cases against HotSpot 25: the name checks (`preDefineClass`, Java), the
// native's argument checks (`ClassLoader.defineClass1` / `defineClass2`), the
// class-file header, a `java.*` class defined with a NULL name, and a
// duplicate definition. One row per case; loaders are `ClassLoader`s with a
// null parent that call `defineClass` directly.
//
// CratonVM before wave 42 (`--jdk-only`, from the code:
// `native-builtins/src/lang_system.rs` `native_classloader_define_class1` /
// `_class2`, `validate_classfile_header`, `read_define_class_nonnegative_int`,
// `read_byte_array_define_class_slice`; `classloading/src/class_manager.rs`
// for the prohibited package):
//   java-null-name(-unnamed)=java.lang.SecurityException: Prohibited package
//     name: java.l5w42.Hello (non-bootstrap loader ... cannot define a class in
//     a protected platform package)
//   garbage, garbage-null-name, garbage-high, bad-magic-len, short-3,
//   magic-only-6, buffer-garbage=java.lang.ClassFormatError: <name>:
//     defineClass1: not a valid class file (bad magic)   (defineClass2 for the
//     buffer row)
//   magic-version-only, cut-in-pool=java.lang.ClassFormatError: l5w42/Hello:
//     unexpected end of data at position <n>
//   bad-offset / neg-offset / neg-len=java.lang.ArrayIndexOutOfBoundsException:
//     Array index out of range: <n>
//   null-bytes=java.lang.NullPointerException: defineClass1: byte[] is null
// `--compatible` keeps those (unchanged by design); the other rows as below.
//
// Run (no setup):
//   javac -d out L5W42DefineClassErrors.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W42DefineClassErrors
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   wrong-name=java.lang.NoClassDefFoundError: l5w42/Other (wrong name: l5w42/Hello)
//   slash-name=java.lang.NoClassDefFoundError: IllegalName: l5w42/Hello
//   java-name=java.lang.SecurityException: Prohibited package name: java.l5w42
//   java-null-name=java.lang.SecurityException: Class loader (instance of): 'o' @H tried to load prohibited package name: java.l5w42
//   java-null-name-unnamed=java.lang.SecurityException: Class loader (instance of): L5W42DefineClassErrors$Open @H tried to load prohibited package name: java.l5w42
//   null-name=l5w42.Hello
//   garbage=java.lang.ClassFormatError: Incompatible magic value 16909060 in class file l5w42/Hello
//   garbage-null-name=java.lang.ClassFormatError: Incompatible magic value 16909060 in class file <Unknown>
//   garbage-high=java.lang.ClassFormatError: Incompatible magic value 4278321924 in class file l5w42/Hello
//   bad-magic-len=java.lang.ClassFormatError: Truncated class file
//   short-3=java.lang.ClassFormatError: Truncated class file
//   magic-only-6=java.lang.ClassFormatError: Truncated class file
//   magic-version-only=java.lang.ClassFormatError: Truncated class file
//   cut-in-pool=java.lang.ClassFormatError: Truncated class file
//   bad-offset=java.lang.ArrayIndexOutOfBoundsException: Array region 5..1000005 out of bounds for length 16
//   neg-offset=java.lang.ArrayIndexOutOfBoundsException: Array region -1..9 out of bounds for length 16
//   neg-len=java.lang.ArrayIndexOutOfBoundsException: null
//   null-bytes=java.lang.NullPointerException: null
//   duplicate=java.lang.LinkageError: loader 'o' @H attempted duplicate class definition for l5w42.Hello. (l5w42.Hello is in unnamed module of loader 'o' @H, parent loader 'bootstrap')
//   buffer-define=l5w42.Hello pos=0
//   buffer-garbage=java.lang.ClassFormatError: Incompatible magic value 16909060 in class file l5w42/Hello

import java.io.*;
import java.lang.module.*;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W42DefineClassErrors {
    static final String P = "L5W42DefineClassErrors$";

    static byte[] rename(byte[] bytes, Map<String, String> renames) throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(in.readInt());
        out.writeShort(in.readUnsignedShort());
        out.writeShort(in.readUnsignedShort());
        int count = in.readUnsignedShort();
        out.writeShort(count);
        for (int i = 1; i < count; i++) {
            int tag = in.readUnsignedByte();
            out.writeByte(tag);
            switch (tag) {
                case 1 -> {
                    String s = in.readUTF();
                    for (Map.Entry<String, String> e : renames.entrySet()) s = s.replace(e.getKey(), e.getValue());
                    out.writeUTF(s);
                }
                case 3, 4 -> out.writeInt(in.readInt());
                case 5, 6 -> { out.writeLong(in.readLong()); i++; }
                case 7, 8, 16, 19, 20 -> out.writeShort(in.readUnsignedShort());
                case 9, 10, 11, 12, 17, 18 -> out.writeInt(in.readInt());
                case 15 -> { out.writeByte(in.readUnsignedByte()); out.writeShort(in.readUnsignedShort()); }
                default -> throw new IOException("tag " + tag);
            }
        }
        in.transferTo(out);
        out.flush();
        return buf.toByteArray();
    }

    static byte[] own(String n) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(n + ".class")) { return in.readAllBytes(); }
    }

    static byte[] hello(String as) throws IOException {
        return rename(own(P + "Hello"), Map.of(P + "Hello", as));
    }

    static String norm(String s) { return s == null ? "null" : s.replaceAll("@[0-9a-f]+", "@H"); }

    static class Open extends ClassLoader {
        Open(String name, ClassLoader parent) { super(name, parent); }
        Class<?> def(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
        Class<?> def(String name, byte[] b, int off, int len) { return defineClass(name, b, off, len); }
        Class<?> defBuf(String name, ByteBuffer bb) { return defineClass(name, bb, null); }
        Class<?> loaded(String name) { return findLoadedClass(name); }
        Package defPkg(String name) { return definePackage(name, null, null, null, null, null, null, null); }
        Package pkg(String name) { return getPackage(name); }
        Object lock(String name) { return getClassLoadingLock(name); }
    }

    static class Parallel extends ClassLoader {
        static { registerAsParallelCapable(); }
        Parallel() { super(null); }
        Object lock(String name) { return getClassLoadingLock(name); }
    }

    static class NotParallel extends Parallel {
        Object lock2(String name) { return getClassLoadingLock(name); }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName() + ": " + norm(t.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        row("wrong-name", () -> new Open("o", null).def("l5w42.Other", hello("l5w42/Hello")).getName());
        row("slash-name", () -> new Open("o", null).def("l5w42/Hello", hello("l5w42/Hello")).getName());
        row("java-name", () -> new Open("o", null).def("java.l5w42.Hello", hello("java/l5w42/Hello")).getName());
        row("java-null-name", () -> new Open("o", null).def(null, hello("java/l5w42/Hello")).getName());
        row("java-null-name-unnamed", () -> new Open(null, null).def(null, hello("java/l5w42/Hello")).getName());
        row("null-name", () -> new Open("o", null).def(null, hello("l5w42/Hello")).getName());
        row("garbage", () -> new Open("o", null).def("l5w42.Hello", new byte[] {1, 2, 3, 4, 5, 6, 7, 8}).getName());
        row("garbage-null-name", () -> new Open("o", null).def(null, new byte[] {1, 2, 3, 4, 5, 6, 7, 8}).getName());
        row("garbage-high", () -> new Open("o", null).def("l5w42.Hello", new byte[] {(byte) 0xFF, 2, 3, 4, 5, 6, 7, 8}).getName());
        row("bad-magic-len", () -> new Open("o", null).def("l5w42.Hello", new byte[0]).getName());
        row("short-3", () -> new Open("o", null).def("l5w42.Hello", new byte[] {(byte) 0xCA, (byte) 0xFE, (byte) 0xBA}).getName());
        row("magic-only-6", () -> new Open("o", null).def("l5w42.Hello", new byte[] {(byte) 0xCA, (byte) 0xFE, (byte) 0xBA, (byte) 0xBE, 0, 0}).getName());
        row("magic-version-only", () -> new Open("o", null).def("l5w42.Hello", new byte[] {(byte) 0xCA, (byte) 0xFE, (byte) 0xBA, (byte) 0xBE, 0, 0, 0, 65}).getName());
        row("cut-in-pool", () -> new Open("o", null).def("l5w42.Hello", java.util.Arrays.copyOf(hello("l5w42/Hello"), 20)).getName());
        row("bad-offset", () -> new Open("o", null).def("l5w42.Hello", new byte[16], 5, 1000000).getName());
        row("neg-offset", () -> new Open("o", null).def("l5w42.Hello", new byte[16], -1, 10).getName());
        row("neg-len",() -> new Open("o", null).def("l5w42.Hello", hello("l5w42/Hello"), 0, -1).getName());
        row("null-bytes", () -> new Open("o", null).def("l5w42.Hello", null, 0, 0).getName());
        row("duplicate", () -> {
            Open o = new Open("o", null);
            o.def("l5w42.Hello", hello("l5w42/Hello"));
            return o.def("l5w42.Hello", hello("l5w42/Hello")).getName();
        });
        row("buffer-define", () -> {
            byte[] b = hello("l5w42/Hello");
            ByteBuffer bb = ByteBuffer.allocateDirect(b.length);
            bb.put(b).flip();
            Class<?> c = new Open("o", null).defBuf("l5w42.Hello", bb);
            return c.getName() + " pos=" + bb.position();
        });
        row("buffer-garbage", () -> {
            ByteBuffer bb = ByteBuffer.allocateDirect(8);
            bb.put(new byte[] {1, 2, 3, 4, 5, 6, 7, 8}).flip();
            return new Open("o", null).defBuf("l5w42.Hello", bb).getName();
        });
    }

    public static class Hello {
        public static String hi() { return "hi"; }
    }
}
