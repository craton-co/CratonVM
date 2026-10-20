// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- what a `URLClassLoader` answers
// after `close()`. JDK 25: `URLClassPath.closeLoaders()` sets `closed`, so
// `getLoader(i)` answers null from then on: `getResource` /
// `getResourceAsStream` answer null, `getResources` is empty, a class not yet
// loaded is `ClassNotFoundException: <name>` (from `findClass`), a class
// already loaded still comes back (`findLoadedClass`), `getURLs()` is
// unchanged, and the jar is no longer held open (deletable on Windows too).
// CratonVM keeps a per-VM `ClassPath` per URL list (classloader.rs
// `cached_class_path_for_paths`) and a closed-loader table that only the
// `close()` natives/bridges write; any door that serves from the cached
// `ClassPath` without asking that table answers as if the loader were open.
// No row of the battery covered close() before this probe.
//
// Run (no setup; writes a temp dir and a temp jar and deletes them):
//   javac -d out L5W106UrlLoaderAfterClose.java
//   java -cp out L5W106UrlLoaderAfterClose                         (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W106UrlLoaderAfterClose
//   cratonvm --compatible [--nojit] -cp out L5W106UrlLoaderAfterClose
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources, not run.
// HOTSPOT_EXPECTED_BEGIN
// dir-before-resource=true
// dir-before-class=Pay1
// dir-after-resource=null
// dir-after-stream=null
// dir-after-resources-empty=true
// dir-after-loaded-class=Pay1 same=true
// dir-after-new-class=java.lang.ClassNotFoundException: Pay2
// dir-after-urls=1
// dir-close-twice=ok
// jar-before-resource=r-jar
// jar-before-class=Pay1
// jar-after-resource=null
// jar-after-new-class=java.lang.ClassNotFoundException: Pay2
// jar-deleted-after-close=true
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.io.OutputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

public class L5W106UrlLoaderAfterClose {
    /** `public class <name> extends Object {}`, version 52, by hand. */
    static byte[] classBytes(String name) {
        int[] head = {0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52, 0, 5};
        byte[] out = new byte[128];
        int n = 0;
        for (int v : head) out[n++] = (byte) v;
        n = classEntry(out, n, 2);
        n = utf8(out, n, name);
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

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String label, Row r) {
        String v;
        try {
            v = String.valueOf(r.run());
        } catch (Throwable t) {
            v = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + "=" + v);
    }

    static String read(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (InputStream s = in) {
            return new String(s.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w106ucl");
        Files.write(dir.resolve("Pay1.class"), classBytes("Pay1"));
        Files.write(dir.resolve("Pay2.class"), classBytes("Pay2"));
        Files.write(dir.resolve("r.txt"), "r-dir".getBytes(StandardCharsets.UTF_8));

        URLClassLoader d = new URLClassLoader(new URL[] {dir.toUri().toURL()}, null);
        row("dir-before-resource", () -> d.getResource("r.txt") != null);
        Class<?> pay1 = d.loadClass("Pay1");
        row("dir-before-class", () -> pay1.getName());
        d.close();
        row("dir-after-resource", () -> d.getResource("r.txt"));
        row("dir-after-stream", () -> read(d.getResourceAsStream("r.txt")));
        row("dir-after-resources-empty", () -> !d.getResources("r.txt").hasMoreElements());
        row("dir-after-loaded-class", () -> {
            Class<?> c = d.loadClass("Pay1");
            return c.getName() + " same=" + (c == pay1);
        });
        row("dir-after-new-class", () -> d.loadClass("Pay2").getName());
        row("dir-after-urls", () -> d.getURLs().length);
        row("dir-close-twice", () -> {
            d.close();
            return "ok";
        });

        Path jar = Files.createTempFile("l5w106ucl", ".jar");
        try (OutputStream fo = Files.newOutputStream(jar);
                JarOutputStream jo = new JarOutputStream(fo)) {
            for (String n : new String[] {"Pay1", "Pay2"}) {
                jo.putNextEntry(new JarEntry(n + ".class"));
                jo.write(classBytes(n));
                jo.closeEntry();
            }
            jo.putNextEntry(new JarEntry("r.txt"));
            jo.write("r-jar".getBytes(StandardCharsets.UTF_8));
            jo.closeEntry();
        }
        URLClassLoader j = new URLClassLoader(new URL[] {jar.toUri().toURL()}, null);
        row("jar-before-resource", () -> read(j.getResourceAsStream("r.txt")));
        row("jar-before-class", () -> j.loadClass("Pay1").getName());
        j.close();
        row("jar-after-resource", () -> j.getResource("r.txt"));
        row("jar-after-new-class", () -> j.loadClass("Pay2").getName());
        row("jar-deleted-after-close", () -> {
            Files.delete(jar);
            return !Files.exists(jar);
        });

        for (String n : new String[] {"Pay1.class", "Pay2.class", "r.txt"}) {
            Files.deleteIfExists(dir.resolve(n));
        }
        Files.deleteIfExists(dir);
    }
}
