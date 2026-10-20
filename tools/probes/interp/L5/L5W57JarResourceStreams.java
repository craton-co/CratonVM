// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- a JAR entry's resource stream and
// connection are the JDK's own (`sun.net.www.protocol.jar.JarURLConnection`)
// (`docs/internal/fixed-bugs/interpreter-L5-classloader-resource-streams-are-bytearrayinputstreams-FIXED-20261021.md`, the JAR
// half). The probe writes a jar into a fresh temporary directory, then asks:
//
//   conn-*     `new URL("jar:file:...!/a.txt").openConnection()`: its class,
//              `getEntryName()`, `getJarEntry().getName()`,
//              `getContentLengthLong()`, `getJarFile()`'s class
//   jarfileurl-* `getJarFileURL()`: the same URL each call, equal to the jar's
//   open-*     `url.openStream()`: the first bytes and the stream's class
//   ucl-*      a `URLClassLoader` over the jar: `getResourceAsStream("a.txt")`
//   class-*    `Inner.class.getResourceAsStream("/a.txt")`, `Inner` defined by
//              that loader from the jar (unnamed module, user loader)
//   missing-*  `jar:...!/nope.txt` `openStream()`: the exception (HotSpot 25 leaves the path out)
//   nocache-*  `setUseCaches(false)`, read, close: is the `JarFile` closed?
//
// Run (no setup):
//   javac -d out L5W57JarResourceStreams.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W57JarResourceStreams
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   conn-1=sun.net.www.protocol.jar.JarURLConnection a.txt a.txt 5 sun.net.www.protocol.jar.URLJarFile
//   jarfileurl-1=true true file
//   open-1=hello sun.net.www.protocol.jar.JarURLConnection$JarURLInputStream
//   ucl-1=hello sun.net.www.protocol.jar.JarURLConnection$JarURLInputStream
//   class-1=hello sun.net.www.protocol.jar.JarURLConnection$JarURLInputStream loader=java.net.URLClassLoader
//   missing-1=java.io.FileNotFoundException msg=JAR entry not found in jar file
//   nocache-1=hello closed=java.lang.IllegalStateException msg=zip file closed
//   conn-2=sun.net.www.protocol.jar.JarURLConnection a.txt a.txt 5 sun.net.www.protocol.jar.URLJarFile
//   jarfileurl-2=true true file
//   open-2=hello sun.net.www.protocol.jar.JarURLConnection$JarURLInputStream
//   ucl-2=hello sun.net.www.protocol.jar.JarURLConnection$JarURLInputStream
//   class-2=hello sun.net.www.protocol.jar.JarURLConnection$JarURLInputStream loader=java.net.URLClassLoader
//   missing-2=java.io.FileNotFoundException msg=JAR entry not found in jar file
//   nocache-2=hello closed=java.lang.IllegalStateException msg=zip file closed
//
// CratonVM before wave 57 (debug build, Windows, measured): `--jdk-only`
// matched but for `jarfileurl-N=false true file` (the `getJarFileURL`
// native answered a fresh URL per call), and so did `--compatible` once
// `Path.toFile()` was fixed, except the rows the `jdk-jar-url-connections`
// row changes: `conn-N=... java.util.jar.JarFile`, `open-N` / `ucl-N` /
// `class-N` `java.io.ByteArrayInputStream`, `missing-N=java.io.FileNotFoundException
// msg=entry nope.txt in <path>: specified file not found in archive`,
// `nocache-N=hello closed=a.txt`. With the row (`CRATONVM_COMPAT_ALIGN=
// +jdk-jar-url-connections`) only `nocache-N=hello closed=null` still
// differs (`--compatible`'s `JarFile` stand-ins answer a closed archive's
// `getEntry` with null; `interpreter-L5-compatible-jarfile-accessors-after-close-answer-null-FIXED-20261022.md`).
// Since wave 58 the `jdk-jar-file-bodies` row (on from lane L5's switch-on
// commit) retires those stand-ins: every line matches HotSpot in
// `--compatible` too (debug build, Windows, measured).

import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.net.JarURLConnection;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.concurrent.Callable;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;

public class L5W57JarResourceStreams {
    public static class Inner {
        public static InputStream open() {
            return Inner.class.getResourceAsStream("/a.txt");
        }
    }

    static String jarPath;

    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            String m = String.valueOf(t.getMessage());
            if (jarPath != null) {
                m = m.replace(jarPath, "<jar>");
            }
            return t.getClass().getName() + " msg=" + m;
        }
    }

    static String head(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (in) {
            byte[] b = in.readNBytes(5);
            return new String(b, StandardCharsets.US_ASCII) + " " + in.getClass().getName();
        }
    }

    static void rows(String tag, File jar, URLClassLoader ucl, Class<?> inner) throws Exception {
        String base = "jar:" + jar.toURI().toURL() + "!/";
        URL a = new URL(base + "a.txt");
        System.out.println("conn-" + tag + "=" + attempt(() -> {
            URLConnection c = a.openConnection();
            JarURLConnection j = (JarURLConnection) c;
            return c.getClass().getName() + " " + j.getEntryName() + " " + j.getJarEntry().getName()
                    + " " + j.getContentLengthLong() + " " + j.getJarFile().getClass().getName();
        }));
        System.out.println("jarfileurl-" + tag + "=" + attempt(() -> {
            JarURLConnection j = (JarURLConnection) a.openConnection();
            URL u = j.getJarFileURL();
            return (u == j.getJarFileURL()) + " " + u.equals(jar.toURI().toURL()) + " " + u.getProtocol();
        }));        System.out.println("open-" + tag + "=" + attempt(() -> head(a.openStream())));
        System.out.println("ucl-" + tag + "=" + attempt(() -> head(ucl.getResourceAsStream("a.txt"))));
        System.out.println("class-" + tag + "=" + attempt(() -> {
            InputStream in = (InputStream) inner.getMethod("open").invoke(null);
            return head(in) + " loader=" + inner.getClassLoader().getClass().getName();
        }));
        System.out.println("missing-" + tag + "=" + attempt(() -> head(new URL(base + "nope.txt").openStream())));
        System.out.println("nocache-" + tag + "=" + attempt(() -> {
            JarURLConnection j = (JarURLConnection) a.openConnection();
            j.setUseCaches(false);
            String h;
            try (InputStream in = j.getInputStream()) {
                h = new String(in.readNBytes(5), StandardCharsets.US_ASCII);
            }
            JarFile jf = j.getJarFile();
            String closed;
            try {
                closed = String.valueOf(jf.getEntry("a.txt"));
            } catch (Throwable t) {
                closed = t.getClass().getName() + " msg=" + t.getMessage();
            }
            return h + " closed=" + closed;
        }));
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w57jar").toFile();
        File jar = new File(dir, "res.jar");
        jarPath = jar.getPath();
        String innerName = Inner.class.getName().replace('.', '/') + ".class";
        byte[] innerBytes;
        try (InputStream in = L5W57JarResourceStreams.class.getClassLoader().getResourceAsStream(innerName)) {
            innerBytes = in.readAllBytes();
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            out.putNextEntry(new ZipEntry("a.txt"));
            out.write("hello".getBytes(StandardCharsets.US_ASCII));
            out.closeEntry();
            out.putNextEntry(new ZipEntry(innerName));
            out.write(innerBytes);
            out.closeEntry();
        }
        URLClassLoader ucl = new URLClassLoader(new URL[] {jar.toURI().toURL()}, null);
        Class<?> inner = Class.forName(Inner.class.getName(), true, ucl);
        rows("1", jar, ucl, inner);
        rows("2", jar, ucl, inner);
    }
}
