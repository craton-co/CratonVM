// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L4 -- a defect probe (no switch), pages
// `docs/internal/fixed-bugs/interpreter-i73-L4-mhn-resolve-judges-a-held-mirror-by-a-by-name-load-FIXED-20261111.md`
// and `i65-orch-jdk-only-findgetter-on-a-renamed-public-nested-class-is-package-private-20261029.md`.
//
// `MethodHandleNatives.resolve` (the `--jdk-only` path of every `Lookup`
// finder) holds the `MemberName.clazz` mirror but merged the member's `ACC_*`
// flags from a SECOND, by-name load of the class from the native caller's
// loader. A class that name does not reach -- a hidden class (`p/C/0x..` is no
// loadable name), a class only a child loader defines (`DupN` below, `DupA`'s
// bytes renamed) -- then kept flags without `ACC_PUBLIC` / `ACC_STATIC`:
// `findGetter` on the child class's public field threw "member is private to
// package", a static member read as an instance one. Wave 79 judges by the
// mirror's own class. Every row must match HotSpot in both modes.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// hidden: true
// hidden-static-getter: 3
// hidden-static-method: 10
// hidden-static-info: invokeStatic static=true public=true
// hidden-virtual: 5
// hidden-getter: 4
// hidden-setter: 8
// hidden-static-setter: 11
// child-name-unreachable: java.lang.ClassNotFoundException: L4W79MhnResolveMirror$DupN
// child-static-getter: 5
// child-getter: 6
// child-static-method: 42
// child-info: getStatic static=true public=true
// child-wrong-kind: java.lang.IllegalAccessException: expected a static field: L4W79MhnResolveMirror$DupN.v/int/getField, from class L4W79MhnResolveMirror (unnamed module @UNNAMED)
// HOTSPOT_EXPECTED_END
//
// The `child-wrong-kind` row's message names the unnamed module with its
// identity hash; the probe rewrites `@<hex>` to `@UNNAMED`.
//
// SETUP: none; `java|cratonvm [--compatible|--jdk-only] [--nojit] -cp . L4W79MhnResolveMirror`.
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Modifier;
import java.nio.charset.StandardCharsets;

public class L4W79MhnResolveMirror {
    public static class Hid {
        public static int S = 3;
        public int i = 4;

        public static int twice(int x) {
            return 2 * x;
        }

        public int plus(int x) {
            return i + x;
        }
    }

    public static class DupA {
        public static int X = 5;
        public int v = 6;

        public static int v() {
            return 42;
        }
    }

    static final String DUP_A = "L4W79MhnResolveMirror$DupA";
    /** As long as `DUP_A`: the bytes are renamed in place. */
    static final String DUP_N = "L4W79MhnResolveMirror$DupN";

    static final class Definer extends ClassLoader {
        Definer(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(byte[] bytes) {
            return defineClass(DUP_N, bytes, 0, bytes.length);
        }
    }

    static byte[] bytesOf(String name) throws IOException {
        try (InputStream in = L4W79MhnResolveMirror.class.getResourceAsStream(name + ".class")) {
            if (in == null) {
                throw new IOException("no class file for " + name);
            }
            return in.readAllBytes();
        }
    }

    /** `from`'s class bytes with every occurrence of its name renamed to `DUP_N`. */
    static byte[] renamed(String from) throws IOException {
        byte[] bytes = bytesOf(from);
        byte[] needle = from.getBytes(StandardCharsets.UTF_8);
        byte[] with = DUP_N.getBytes(StandardCharsets.UTF_8);
        for (int i = 0; i + needle.length <= bytes.length; i++) {
            boolean hit = true;
            for (int j = 0; j < needle.length && hit; j++) {
                hit = bytes[i + j] == needle[j];
            }
            if (hit) {
                System.arraycopy(with, 0, bytes, i, with.length);
            }
        }
        return bytes;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        out = out.replaceAll("unnamed module @[0-9a-f]+", "unnamed module @UNNAMED");
        System.out.println(name + ": " + out);
    }

    static String info(MethodHandleInfo info) {
        return MethodHandleInfo.referenceKindToString(info.getReferenceKind())
                + " static=" + Modifier.isStatic(info.getModifiers())
                + " public=" + Modifier.isPublic(info.getModifiers());
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandles.Lookup hl = lk.defineHiddenClass(bytesOf("L4W79MhnResolveMirror$Hid"), true);
        Class<?> hc = hl.lookupClass();
        Object ho = hc.getConstructor().newInstance();
        MethodType intInt = MethodType.methodType(int.class, int.class);

        row("hidden", hc::isHidden);
        row("hidden-static-getter", () -> (int) hl.findStaticGetter(hc, "S", int.class).invoke());
        row("hidden-static-method", () -> (int) hl.findStatic(hc, "twice", intInt).invoke(5));
        row("hidden-static-info", () -> info(hl.revealDirect(hl.findStatic(hc, "twice", intInt))));
        row("hidden-virtual", () -> (int) hl.findVirtual(hc, "plus", intInt).invoke(ho, 1));
        row("hidden-getter", () -> (int) hl.findGetter(hc, "i", int.class).invoke(ho));
        row("hidden-setter", () -> {
            hl.findSetter(hc, "i", int.class).invoke(ho, 8);
            return (int) hl.findGetter(hc, "i", int.class).invoke(ho);
        });
        row("hidden-static-setter", () -> {
            hl.findStaticSetter(hc, "S", int.class).invoke(11);
            return (int) hl.findStaticGetter(hc, "S", int.class).invoke();
        });

        Class<?> dup = new Definer(L4W79MhnResolveMirror.class.getClassLoader()).define(renamed(DUP_A));
        Object d = dup.getConstructor().newInstance();
        row("child-name-unreachable", () -> Class.forName(DUP_N));
        row("child-static-getter", () -> (int) lk.findStaticGetter(dup, "X", int.class).invoke());
        row("child-getter", () -> (int) lk.findGetter(dup, "v", int.class).invoke(d));
        row("child-static-method", () -> (int) lk.findStatic(dup, "v", MethodType.methodType(int.class)).invoke());
        row("child-info", () -> info(lk.revealDirect(lk.findStaticGetter(dup, "X", int.class))));
        row("child-wrong-kind", () -> lk.findStaticGetter(dup, "v", int.class));
    }
}
