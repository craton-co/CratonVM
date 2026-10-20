// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L5 -- `--compatible`'s `JarFile` is the
// JDK's (the `jdk-jar-file-bodies` HotSpot-alignment row; the defect
// `docs/internal/fixed-bugs/interpreter-L5-compatible-jarfile-accessors-after-close-answer-null-FIXED-20261022.md`).
// The probe writes two jars into a fresh temporary directory (a plain one with
// a manifest, a comment and a stored entry; a Multi-Release one with a
// `META-INF/versions/9` override), then asks:
//
//   closed-*   every accessor of a CLOSED `JarFile` (HotSpot: `ensureOpen`'s
//              `IllegalStateException: zip file closed`), first and again last
//   sub-*      a `JarFile` subclass whose `getEntry` override calls
//              `super.getEntry` (the `UrlJarFile` shape), and its close
//   entry-*    a `JarEntry`'s name, size, compressed size, method, comment,
//              its class, and `getJarEntry` of a missing name
//   man-*      the manifest's main attributes, its identity across calls,
//              a per-entry section, a value with a leading and two trailing
//              spaces (kept verbatim)
//   mr-*       `isMultiRelease`, a versioned entry's content and real name
//              under `Runtime.version()` and under base version 8
//   zip-*      a plain `ZipFile` over the same jar: entries in order, close
//   stream-*   `JarFile.stream()` names and `size()`
//
// Run (no setup):
//   javac -d out L5W58CompatJarFileBodies.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W58CompatJarFileBodies
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   closed-1=getEntry:java.lang.IllegalStateException:zip file closed getJarEntry:java.lang.IllegalStateException:zip file closed getInputStream:java.lang.IllegalStateException:zip file closed entries:java.lang.IllegalStateException:zip file closed stream:java.lang.IllegalStateException:zip file closed size:java.lang.IllegalStateException:zip file closed getComment:java.lang.IllegalStateException:zip file closed getManifest:java.lang.IllegalStateException:zip file closed getName:a.jar
//   sub-1=L5W58CompatJarFileBodies$SubJar sub:a.txt null closed=java.lang.IllegalStateException
//   entry-1=a.txt 5 5 0 note java.util.jar.JarFile$JarFileEntry missing=null
//   man-1=1.0 L5Probe 4.2 same=true section=yes spec=[ padded  ]
//   mr-1=true ver=nine real=META-INF/versions/9/v.txt base=base realBase=v.txt
//   zip-1=[META-INF/MANIFEST.MF, a.txt, b/c.txt] n=3 comment=jar-comment closed=java.lang.IllegalStateException
//   stream-1=[META-INF/MANIFEST.MF, a.txt, b/c.txt] 3
//   closed-2=getEntry:java.lang.IllegalStateException:zip file closed getJarEntry:java.lang.IllegalStateException:zip file closed getInputStream:java.lang.IllegalStateException:zip file closed entries:java.lang.IllegalStateException:zip file closed stream:java.lang.IllegalStateException:zip file closed size:java.lang.IllegalStateException:zip file closed getComment:java.lang.IllegalStateException:zip file closed getManifest:java.lang.IllegalStateException:zip file closed getName:a.jar
//   sub-2=L5W58CompatJarFileBodies$SubJar sub:a.txt null closed=java.lang.IllegalStateException
//   entry-2=a.txt 5 5 0 note java.util.jar.JarFile$JarFileEntry missing=null
//   man-2=1.0 L5Probe 4.2 same=true section=yes spec=[ padded  ]
//   mr-2=true ver=nine real=META-INF/versions/9/v.txt base=base realBase=v.txt
//   zip-2=[META-INF/MANIFEST.MF, a.txt, b/c.txt] n=3 comment=jar-comment closed=java.lang.IllegalStateException
//   stream-2=[META-INF/MANIFEST.MF, a.txt, b/c.txt] 3
//
// CratonVM `--compatible` before wave 58 (the stand-ins; code read and the
// wave-57 measurement of `getEntry`): the `closed-N` accessors answer `null`
// / an empty listing where HotSpot throws, and `entry-N`'s class is
// `java.util.jar.JarEntry`. With the row (`CRATONVM_COMPAT_ALIGN=
// +jdk-jar-file-bodies`) the JDK's bodies run, as under `--jdk-only`.

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

public class L5W58CompatJarFileBodies {
    static File plain;
    static File multi;

    public static class SubJar extends JarFile {
        public SubJar(File f) throws IOException {
            super(f);
        }

        @Override
        public ZipEntry getEntry(String name) {
            ZipEntry e = super.getEntry(name);
            return e;
        }
    }

    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + ":" + t.getMessage();
        }
    }

    static String cls(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static String read(InputStream in) throws IOException {
        try (InputStream i = in) {
            return new String(i.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

    static void put(JarOutputStream out, String name, String body, boolean stored, String comment)
            throws IOException {
        byte[] b = body.getBytes(StandardCharsets.UTF_8);
        JarEntry e = new JarEntry(name);
        if (stored) {
            CRC32 crc = new CRC32();
            crc.update(b);
            e.setMethod(ZipEntry.STORED);
            e.setSize(b.length);
            e.setCompressedSize(b.length);
            e.setCrc(crc.getValue());
        }
        if (comment != null) {
            e.setComment(comment);
        }
        out.putNextEntry(e);
        out.write(b);
        out.closeEntry();
    }

    static void writeJars(File dir) throws IOException {
        plain = new File(dir, "a.jar");
        Manifest m = new Manifest();
        Attributes main = m.getMainAttributes();
        main.put(Attributes.Name.MANIFEST_VERSION, "1.0");
        main.put(Attributes.Name.IMPLEMENTATION_TITLE, "L5Probe");
        main.put(Attributes.Name.IMPLEMENTATION_VERSION, "4.2");
        main.put(Attributes.Name.SPECIFICATION_TITLE, " padded  ");
        Attributes sec = new Attributes();
        sec.putValue("Note", "yes");
        m.getEntries().put("a.txt", sec);
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(plain), m)) {
            out.setComment("jar-comment");
            put(out, "a.txt", "hello", true, "note");
            put(out, "b/c.txt", "inner", false, null);
        }
        multi = new File(dir, "mr.jar");
        Manifest mm = new Manifest();
        mm.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        mm.getMainAttributes().putValue("Multi-Release", "true");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(multi), mm)) {
            put(out, "v.txt", "base", false, null);
            put(out, "META-INF/versions/9/v.txt", "nine", false, null);
        }
    }

    static String closedRow() throws Exception {
        JarFile jf = new JarFile(plain);
        JarEntry a = jf.getJarEntry("a.txt");
        jf.close();
        StringBuilder sb = new StringBuilder();
        sb.append("getEntry:").append(attempt(() -> jf.getEntry("a.txt")));
        sb.append(" getJarEntry:").append(attempt(() -> jf.getJarEntry("a.txt")));
        sb.append(" getInputStream:").append(attempt(() -> jf.getInputStream(a)));
        sb.append(" entries:").append(attempt(() -> jf.entries()));
        sb.append(" stream:").append(attempt(() -> jf.stream()));
        sb.append(" size:").append(attempt(() -> jf.size()));
        sb.append(" getComment:").append(attempt(() -> jf.getComment()));
        sb.append(" getManifest:").append(attempt(() -> jf.getManifest()));
        sb.append(" getName:").append(new File(jf.getName()).getName());
        return sb.toString();
    }

    static String subRow() throws Exception {
        SubJar s = new SubJar(plain);
        ZipEntry e = s.getEntry("a.txt");
        ZipEntry missing = s.getEntry("zz");
        s.close();
        return s.getClass().getName() + " sub:" + e.getName() + " " + missing
                + " closed=" + cls(() -> s.getEntry("a.txt"));
    }

    static String entryRow() throws Exception {
        try (JarFile jf = new JarFile(plain)) {
            JarEntry e = jf.getJarEntry("a.txt");
            return e.getName() + " " + e.getSize() + " " + e.getCompressedSize() + " "
                    + e.getMethod() + " " + e.getComment() + " " + e.getClass().getName()
                    + " missing=" + jf.getJarEntry("nope");
        }
    }

    static String manRow() throws Exception {
        try (JarFile jf = new JarFile(plain)) {
            Manifest m = jf.getManifest();
            Attributes a = m.getMainAttributes();
            Attributes sec = m.getAttributes("a.txt");
            return a.getValue("Manifest-Version") + " "
                    + a.getValue(Attributes.Name.IMPLEMENTATION_TITLE) + " "
                    + a.getValue("implementation-version") + " same="
                    + (m == jf.getManifest()) + " section="
                    + (sec == null ? null : sec.getValue("Note")) + " spec=["
                    + a.getValue("Specification-Title") + "]";
        }
    }

    static String mrRow() throws Exception {
        String head;
        try (JarFile jf = new JarFile(multi, true, ZipFile.OPEN_READ, Runtime.version())) {
            JarEntry e = jf.getJarEntry("v.txt");
            head = jf.isMultiRelease() + " ver=" + read(jf.getInputStream(e)) + " real="
                    + e.getRealName();
        }
        try (JarFile jf = new JarFile(multi, true, ZipFile.OPEN_READ, JarFile.baseVersion())) {
            JarEntry e = jf.getJarEntry("v.txt");
            return head + " base=" + read(jf.getInputStream(e)) + " realBase=" + e.getRealName();
        }
    }

    static String zipRow() throws Exception {
        ZipFile z = new ZipFile(plain);
        List<String> names = new ArrayList<>();
        for (ZipEntry e : Collections.list(z.entries())) {
            names.add(e.getName());
        }
        String out = names + " n=" + z.size() + " comment=" + z.getComment();
        z.close();
        return out + " closed=" + cls(() -> z.getEntry("a.txt"));
    }

    static String streamRow() throws Exception {
        try (JarFile jf = new JarFile(plain)) {
            List<String> names = new ArrayList<>();
            jf.stream().forEach(e -> names.add(e.getName()));
            return names + " " + jf.size();
        }
    }

    static void round(int n) {
        System.out.println("closed-" + n + "=" + attempt(L5W58CompatJarFileBodies::closedRow));
        System.out.println("sub-" + n + "=" + attempt(L5W58CompatJarFileBodies::subRow));
        System.out.println("entry-" + n + "=" + attempt(L5W58CompatJarFileBodies::entryRow));
        System.out.println("man-" + n + "=" + attempt(L5W58CompatJarFileBodies::manRow));
        System.out.println("mr-" + n + "=" + attempt(L5W58CompatJarFileBodies::mrRow));
        System.out.println("zip-" + n + "=" + attempt(L5W58CompatJarFileBodies::zipRow));
        System.out.println("stream-" + n + "=" + attempt(L5W58CompatJarFileBodies::streamRow));
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w58jar").toFile();
        writeJars(dir);
        round(1);
        round(2);
    }
}
