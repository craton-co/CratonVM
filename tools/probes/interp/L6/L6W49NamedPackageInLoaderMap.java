// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6: a class a user loader defines with a
// plain `defineClass` (no `definePackage` first) defines its package in the
// loader (`postDefineClass` -> `getNamedPackage` puts a `NamedPackage` in the
// loader's `packages` map), in `--compatible` as on HotSpot and under
// `--jdk-only` (wave 42). `--compatible`'s `getNamedPackage` native answered a
// fresh synthetic `Package` and left the map empty, so the rows below printed
// `defined=null`, `define-after=package l6w49a`, `defined-packages=[]` and
// `module-packages=[]` there
// (`docs/internal/fixed-bugs/interpreter-L6-compatible-get-named-package-does-not-fill-the-loaders-package-map-FIXED-20261013.md`).
// Switch: `CRATONVM_COMPAT_NAMED_PACKAGES`. The `defined` row runs first
// (cold) and again last (for a second package).
//
// HotSpot 25.0.3 prints (measured; the same with -Xint):
//     defined=l6w49a
//     define-after=java.lang.IllegalArgumentException: l6w49a
//     defined-packages=[l6w49a, l6w49b]
//     module-packages=[l6w49a, l6w49b]
//     get-package=l6w49a true
//     defined-again=l6w49b
//
//     javac -d out L6W49NamedPackageInLoaderMap.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W49NamedPackageInLoaderMap

import java.io.*;
import java.util.*;

public class L6W49NamedPackageInLoaderMap {
    static final String P = "L6W49NamedPackageInLoaderMap$";

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

    static byte[] hello(String pkg) throws IOException {
        byte[] own;
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + "Hello.class")) {
            own = in.readAllBytes();
        }
        return rename(own, P + "Hello", pkg + "/Hello");
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(null);
        }

        Class<?> define(String pkg) throws IOException {
            byte[] b = hello(pkg);
            return defineClass(pkg + ".Hello", b, 0, b.length);
        }

        Package defined(String pn) {
            return getDefinedPackage(pn);
        }

        Object defineAfter(String pn) {
            try {
                return definePackage(pn, null, null, null, null, null, null, null);
            } catch (IllegalArgumentException e) {
                return e;
            }
        }

        List<String> definedPackages() {
            List<String> names = new ArrayList<>();
            for (Package p : getDefinedPackages()) names.add(p.getName());
            Collections.sort(names);
            return names;
        }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static String name(Package p) {
        return p == null ? "null" : p.getName();
    }

    public static void main(String[] args) throws Exception {
        Loader l = new Loader();
        Class<?> a = l.define("l6w49a");
        row("defined", () -> name(l.defined("l6w49a")));
        row("define-after", () -> l.defineAfter("l6w49a"));
        Class<?> b = l.define("l6w49b");
        row("defined-packages", l::definedPackages);
        row("module-packages", () -> new TreeSet<>(l.getUnnamedModule().getPackages()));
        row("get-package", () -> a.getPackage().getName() + " " + (a.getPackage() == l.defined("l6w49a")));
        row("defined-again", () -> name(l.defined("l6w49b")));
    }

    public static class Hello {
    }
}
