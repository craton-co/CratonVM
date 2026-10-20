// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 100, lane L5 --
// `docs/internal/fixed-bugs/interpreter-i99-orch-truncated-jar-sigbus-on-linux-FIXED-20261015.md`.
// A PLAIN jar (a `Class-Path:` manifest, a class, a resource) rewritten and
// truncated IN PLACE by another thread while `URLClassLoader`s over it read
// its manifest (`getURLs()`, whose `--compatible` native expands the
// manifest `Class-Path:` by reading the jar), load its class and read its
// resource. HotSpot's `ZipFile` reads with positioned reads into its own
// buffers: every failure is a Java exception and the VM survives. Before
// wave 100 the `--compatible` manifest reader (`ClassPath::read_jar_manifest`)
// and the nested walk-index fallback (`nested_archive_entry_names`) MAPPED
// the jar on Linux / macOS, so a read past a truncated end was SIGBUS.
// (`L5W67NestedJarOuterTruncate` covers the fat-jar shape.)
//
// Linux / macOS are the interesting hosts; on Windows the rewrite may be
// refused while a handle is open, which the writer retries.
//
// Run (no setup):
//   javac -d out L5W100TruncatedPlainJar.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W100TruncatedPlainJar
//
// Expected HotSpot 25 output:
//   before=loader=true res=hello
//   stress-rounds=300
//   stress-survived=true
//   after=loader=true res=hello

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L5W100TruncatedPlainJar {
    static final int STRESS = 300;

    static byte[] jar(String text, int padding) throws IOException {
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        m.getMainAttributes().put(Attributes.Name.CLASS_PATH, "other-does-not-exist.jar");
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (JarOutputStream jos = new JarOutputStream(bytes, m)) {
            jos.putNextEntry(new JarEntry("w100t/T.class"));
            jos.write(ClassFile.of().build(ClassDesc.of("w100t.T"), cb -> cb
                    .withFlags(ClassFile.ACC_PUBLIC)
                    .withSuperclass(ConstantDescs.CD_Object)));
            jos.closeEntry();
            jos.putNextEntry(new JarEntry("w100t/res.txt"));
            jos.write(text.getBytes("UTF-8"));
            jos.closeEntry();
            // Padding so the central directory sits pages past the start.
            jos.putNextEntry(new JarEntry("w100t/pad.bin"));
            byte[] pad = new byte[padding];
            for (int i = 0; i < pad.length; i++) {
                pad[i] = (byte) (i * 31 + 7);
            }
            jos.write(pad);
            jos.closeEntry();
        }
        return bytes.toByteArray();
    }

    static void rewrite(Path p, byte[] b) throws IOException {
        Files.write(p, b, StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING,
                StandardOpenOption.WRITE);
    }

    static void truncate(Path p, long size) throws IOException {
        try (FileChannel ch = FileChannel.open(p, StandardOpenOption.WRITE)) {
            ch.truncate(size);
        }
    }

    static String row(URLClassLoader ucl) {
        String loader;
        try {
            Class<?> c = Class.forName("w100t.T", false, ucl);
            loader = String.valueOf(c.getClassLoader() == ucl);
        } catch (Throwable t) {
            loader = "fail";
        }
        String res;
        try {
            URL u = ucl.getResource("w100t/res.txt");
            if (u == null) {
                res = "null";
            } else {
                // No `JarFileFactory` caching: a cached `JarFile` would
                // outlive the loader and see a later rewrite.
                URLConnection conn = u.openConnection();
                conn.setUseCaches(false);
                try (InputStream in = conn.getInputStream()) {
                    res = new String(in.readAllBytes(), "UTF-8");
                }
            }
        } catch (Throwable t) {
            res = "fail";
        }
        return "loader=" + loader + " res=" + res;
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w100t");
        Path jar = dir.resolve("plain.jar");
        byte[] first = jar("hello", 64 * 1024);
        byte[] second = jar("hello", 96 * 1024);
        rewrite(jar, first);
        URL[] urls = {jar.toUri().toURL()};
        try (URLClassLoader ucl = new URLClassLoader(urls, null)) {
            System.out.println("before=" + row(ucl));
        }
        AtomicBoolean stop = new AtomicBoolean();
        Thread writer = new Thread(() -> {
            long[] sizes = {first.length / 2, 22, 0, first.length - 1, 4096};
            for (int i = 0; !stop.get(); i++) {
                try {
                    rewrite(jar, (i & 1) == 0 ? first : second);
                    truncate(jar, sizes[i % sizes.length]);
                } catch (IOException ignored) {
                    // A host that refuses the rewrite while a handle is open.
                }
            }
        }, "l5w100t-writer");
        writer.start();
        int rounds = 0;
        try {
            for (; rounds < STRESS; rounds++) {
                try (URLClassLoader ucl = new URLClassLoader(urls, null)) {
                    ucl.getURLs();
                    row(ucl);
                } catch (Throwable ignored) {
                    // Any Java-level failure is fine; only the VM's survival is checked.
                }
            }
        } finally {
            stop.set(true);
            writer.join();
        }
        System.out.println("stress-rounds=" + rounds);
        System.out.println("stress-survived=true");
        rewrite(jar, first);
        try (URLClassLoader ucl = new URLClassLoader(urls, null)) {
            System.out.println("after=" + row(ucl));
        }
    }
}
