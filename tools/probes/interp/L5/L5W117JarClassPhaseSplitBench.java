// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L5: where a class loaded from a jar
// spends its time, phase by phase, measured the same way on HotSpot and on
// CratonVM (`i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot`,
// "Wave 117"). Self-contained: a jar of 400 generated classes (each with a
// `<clinit>`, a field and two methods) is written at run time.
//
// Per class, medians over 7 rounds (each round fresh loaders, so every define
// is a first define):
//   open-cen       `new JarFile(f)` + `close()` (the central directory read), per jar / classes
//   get-entry      `JarFile.getEntry(name)`
//   read-inflate   `getInputStream(entry).readAllBytes()` (the inflater)
//   define         `defineClass(name, bytes, ..)` on a plain loader
//   link-init      `Class.forName(name, true, loader)` right after the define (link, verify, `<clinit>`)
//   e2e            a fresh `URLClassLoader` over the jar: `Class.forName(name, true, loader)`
//   remainder      e2e - (get-entry + read-inflate + define + link-init): the loader's own Java
//                  (`URLClassPath` lookup, `CodeSource` / `ProtectionDomain`, package definition)
// Every TIME row is the A/B; the non-TIME lines are deterministic (compared).
//
// With CratonVM add `CRATONVM_DBG_DEFINE_CENSUS=1` to split `define` / `link-init` further (the
// define census's `native-total`, `verify-*`, `link-*`, `init-*` rows) and
// `CRATONVM_DBG=zip-stream-census` to count the inflate calls behind `read-inflate` / `e2e`.
//
// Run: javac -d . L5W117JarClassPhaseSplitBench.java
//      java -cp . L5W117JarClassPhaseSplitBench                 (HotSpot; also -Xint)
//      cratonvm --jdk-only [--nojit] -cp . L5W117JarClassPhaseSplitBench
//      cratonvm --compatible [--nojit] -cp . L5W117JarClassPhaseSplitBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// classes 400
// read-bytes-ok 400
// define-checksum 80200
// e2e-checksum 80200
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.util.Arrays;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;

public class L5W117JarClassPhaseSplitBench {
    static final int N = 400;
    static final int ROUNDS = 7;

    static String name(int i) {
        return "gen.C" + i;
    }

    /** `gen.C<i>`: `public static int V = i + 1;` set in `<clinit>`, `value()`, `twice()`. */
    static byte[] classBytes(int i) {
        ClassDesc self = ClassDesc.of(name(i));
        return ClassFile.of().build(self, cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withField("V", ConstantDescs.CD_int, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC)
                .withMethodBody("<clinit>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_STATIC, c -> c
                        .ldc(i + 1)
                        .putstatic(self, "V", ConstantDescs.CD_int)
                        .return_())
                .withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_PUBLIC, c -> c
                        .aload(0)
                        .invokespecial(ConstantDescs.CD_Object, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void))
                        .return_())
                .withMethodBody("value", MethodTypeDesc.of(ConstantDescs.CD_int), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c
                        .getstatic(self, "V", ConstantDescs.CD_int)
                        .ireturn())
                .withMethodBody("twice", MethodTypeDesc.of(ConstantDescs.CD_int), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c
                        .invokestatic(self, "value", MethodTypeDesc.of(ConstantDescs.CD_int))
                        .iconst_2()
                        .imul()
                        .ireturn()));
    }

    static final class Plain extends ClassLoader {
        Plain() {
            super(ClassLoader.getPlatformClassLoader());
        }

        Class<?> define(String n, byte[] b) {
            return defineClass(n, b, 0, b.length);
        }
    }

    static long median(long[] v) {
        long[] c = v.clone();
        Arrays.sort(c);
        return c[c.length / 2];
    }

    static int value(Class<?> c) throws Exception {
        return c.getField("V").getInt(null);
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w117split").toFile();
        File jar = new File(dir, "gen.jar");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            for (int i = 0; i < N; i++) {
                out.putNextEntry(new JarEntry(name(i).replace('.', '/') + ".class"));
                out.write(classBytes(i));
                out.closeEntry();
            }
        }
        URL[] urls = {jar.toURI().toURL()};
        String[] entries = new String[N];
        for (int i = 0; i < N; i++) {
            entries[i] = name(i).replace('.', '/') + ".class";
        }

        long[] openCen = new long[ROUNDS];
        long[] getEntry = new long[ROUNDS];
        long[] readInflate = new long[ROUNDS];
        long[] define = new long[ROUNDS];
        long[] linkInit = new long[ROUNDS];
        long[] e2e = new long[ROUNDS];
        int readOk = 0;
        long defineSum = 0;
        long e2eSum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            new JarFile(jar).close();
            openCen[r] = (System.nanoTime() - t0) / N;

            byte[][] bytes = new byte[N][];
            try (JarFile jf = new JarFile(jar)) {
                ZipEntry[] es = new ZipEntry[N];
                long a = System.nanoTime();
                for (int i = 0; i < N; i++) {
                    es[i] = jf.getEntry(entries[i]);
                }
                long b = System.nanoTime();
                for (int i = 0; i < N; i++) {
                    try (InputStream in = jf.getInputStream(es[i])) {
                        bytes[i] = in.readAllBytes();
                    }
                }
                long c = System.nanoTime();
                getEntry[r] = (b - a) / N;
                readInflate[r] = (c - b) / N;
            }
            if (r == 0) {
                for (int i = 0; i < N; i++) {
                    if (Arrays.equals(bytes[i], classBytes(i))) {
                        readOk++;
                    }
                }
            }

            Plain plain = new Plain();
            Class<?>[] defined = new Class<?>[N];
            long d0 = System.nanoTime();
            for (int i = 0; i < N; i++) {
                defined[i] = plain.define(name(i), bytes[i]);
            }
            long d1 = System.nanoTime();
            for (int i = 0; i < N; i++) {
                Class.forName(name(i), true, plain);
            }
            long d2 = System.nanoTime();
            define[r] = (d1 - d0) / N;
            linkInit[r] = (d2 - d1) / N;
            if (r == 0) {
                for (Class<?> k : defined) {
                    defineSum += value(k);
                }
            }

            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
                Class<?>[] loaded = new Class<?>[N];
                long u0 = System.nanoTime();
                for (int i = 0; i < N; i++) {
                    loaded[i] = Class.forName(name(i), true, ucl);
                }
                e2e[r] = (System.nanoTime() - u0) / N;
                if (r == 0) {
                    for (Class<?> k : loaded) {
                        e2eSum += value(k);
                    }
                }
            }
        }
        System.out.println("classes " + N);
        System.out.println("read-bytes-ok " + readOk);
        System.out.println("define-checksum " + defineSum);
        System.out.println("e2e-checksum " + e2eSum);
        long ge = median(getEntry);
        long ri = median(readInflate);
        long de = median(define);
        long li = median(linkInit);
        long ee = median(e2e);
        System.out.println("TIME open-cen ns/class " + median(openCen));
        System.out.println("TIME get-entry ns/class " + ge);
        System.out.println("TIME read-inflate ns/class " + ri);
        System.out.println("TIME define ns/class " + de);
        System.out.println("TIME link-init ns/class " + li);
        System.out.println("TIME e2e ns/class " + ee);
        System.out.println("TIME remainder ns/class " + (ee - ge - ri - de - li));
        jar.delete();
        dir.delete();
    }
}
