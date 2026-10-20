// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6: the VM-raised `ClassCastException`
// and `Lookup.defineClass` duplicate-definition messages name each class's
// module and loader, a user-defined loader by its `nameAndId`, in
// `--compatible` as on HotSpot and under `--jdk-only`
// (`docs/internal/fixed-bugs/interpreter-L6-compatible-cast-and-duplicate-define-messages-omit-module-and-loader-FIXED-20261013.md`).
// Switch: `CRATONVM_COMPAT_MODULE_MESSAGES`. `--compatible` printed the bare
// `l6w49m.Hello cannot be cast to java.lang.String` before, and the
// duplicate row's loader as `UserDefined(n)`. The identity hash HotSpot
// appends to a loader's name (`@1b6d3586`) is masked as `@H`.
//
// HotSpot 25.0.3 prints (measured; the same with -Xint):
//     cast=class l6w49m.Hello cannot be cast to class java.lang.String (l6w49m.Hello is in unnamed module of loader 'l6w49' @H; java.lang.String is in module java.base of loader 'bootstrap')
//     cast-array=class [Ll6w49m.Hello; cannot be cast to class [Ljava.lang.String; ([Ll6w49m.Hello; is in unnamed module of loader 'l6w49' @H; [Ljava.lang.String; is in module java.base of loader 'bootstrap')
//     duplicate=java.lang.LinkageError: loader 'l6w49' @H attempted duplicate class definition for l6w49m.Mate. (l6w49m.Mate is in unnamed module of loader 'l6w49' @H, parent loader 'bootstrap')
//     cast-again=class l6w49m.Hello cannot be cast to class java.lang.String (l6w49m.Hello is in unnamed module of loader 'l6w49' @H; java.lang.String is in module java.base of loader 'bootstrap')
//
//     javac -d out L6W49CompatModuleMessages.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W49CompatModuleMessages

import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;

public class L6W49CompatModuleMessages {
    static final String P = "L6W49CompatModuleMessages$";

    static byte[] rename(byte[] bytes, String from, String to) throws IOException {
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
                case 1 -> out.writeUTF(in.readUTF().replace(from, to));
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

    static byte[] member(String simple) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + simple + ".class")) {
            return rename(in.readAllBytes(), P + simple, "l6w49m/" + simple);
        }
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super("l6w49", null);
        }

        Class<?> define(String simple) throws IOException {
            byte[] b = member(simple);
            return defineClass("l6w49m." + simple, b, 0, b.length);
        }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            String m = String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
            System.out.println(label + "=" + (t instanceof ClassCastException ? m : t.getClass().getName() + ": " + m));
        }
    }

    static Object cast(Object o) {
        return (String) o;
    }

    static Object castArray(Object o) {
        return (String[]) o;
    }

    public static void main(String[] args) throws Exception {
        Loader l = new Loader();
        Class<?> hello = l.define("Hello");
        Object h = hello.getConstructor().newInstance();
        row("cast", () -> cast(h));
        row("cast-array", () -> castArray(Array.newInstance(hello, 1)));
        MethodHandles.Lookup lk = (MethodHandles.Lookup) hello.getMethod("lk").invoke(null);
        byte[] mate = member("Mate");
        row("duplicate", () -> {
            lk.defineClass(mate);
            return lk.defineClass(mate);
        });
        row("cast-again", () -> cast(h));
    }

    public static class Hello {
        public static MethodHandles.Lookup lk() {
            return MethodHandles.lookup();
        }
    }

    public static class Mate {
    }
}
