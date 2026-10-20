// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L5 -- an APPLICATION subclass of
// `java.net.JarURLConnection` that does not override `getJarEntry()`,
// `getContentLength()` or `getContentLengthLong()` runs the JDK's bodies:
// `getJarFile().getJarEntry(entryName)` and `URLConnection`'s header read
// (no `content-length` header: -1). The VM's stand-ins on
// `java.net.JarURLConnection` (written for its never-constructed carrier)
// answered such a receiver from its URL instead (the `JarURLConnection` rows
// of `docs/internal/fixed-bugs/interpreter-L5-bridges-still-run-over-jdk-bytecode-under-jdk-only-FIXED-20261007.md`).
//
// Rows (the first call, and again last):
//   entry-*    `getJarEntry()`: its name and class, and how many times the
//              subclass's `getJarFile()` was asked
//   length-*   `getContentLength()` / `getContentLengthLong()`
//   names-*    `getEntryName()` / `getJarFileURL()` protocol (constructed
//              fields, already the JDK's since wave 57)
//
// Run (no setup):
//   javac -d out L5W58JarConnectionSubclass.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W58JarConnectionSubclass
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   entry-1=a.txt java.util.jar.JarFile$JarFileEntry asked=1
//   length-1=-1 -1
//   names-1=a.txt file
//   entry-2=a.txt java.util.jar.JarFile$JarFileEntry asked=1
//   length-2=-1 -1
//   names-2=a.txt file
//
// CratonVM before wave 58 (debug build, Windows, measured, `--jdk-only` and
// `--compatible` with the `jdk-jar-file-bodies` row): `entry-N=a.txt
// java.util.jar.JarEntry asked=0` and `length-N=5 5` (the carrier's
// stand-ins read the entry from the URL). `--compatible` without the row
// still prints `java.util.jar.JarEntry` for `entry-N` (the `JarFile`
// stand-in's entry class), the rest as HotSpot.

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.net.JarURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.concurrent.Callable;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;

public class L5W58JarConnectionSubclass {
    static File jar;

    static class MyConn extends JarURLConnection {
        int asked;

        MyConn(URL u) throws IOException {
            super(u);
        }

        @Override
        public JarFile getJarFile() throws IOException {
            asked++;
            return new JarFile(jar);
        }

        @Override
        public void connect() {
        }
    }

    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w58conn").toFile();
        jar = new File(dir, "c.jar");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            out.putNextEntry(new ZipEntry("a.txt"));
            out.write("hello".getBytes(StandardCharsets.UTF_8));
            out.closeEntry();
        }
        URL u = new URL("jar:" + jar.toURI().toURL() + "!/a.txt");
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("entry-" + tag + "=" + attempt(() -> {
                MyConn c = new MyConn(u);
                var e = c.getJarEntry();
                return (e == null ? "null" : e.getName() + " " + e.getClass().getName()) + " asked=" + c.asked;
            }));
            System.out.println("length-" + tag + "=" + attempt(() -> {
                MyConn c = new MyConn(u);
                return c.getContentLength() + " " + c.getContentLengthLong();
            }));
            System.out.println("names-" + tag + "=" + attempt(() -> {
                MyConn c = new MyConn(u);
                return c.getEntryName() + " " + c.getJarFileURL().getProtocol();
            }));
        }
    }
}
