// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 64, lane L5 -- the jar-loader bytes index
// (`CRATONVM_JAR_LOADER_BYTES_INDEX`, default off;
// `docs/internal/fixed-bugs/interpreter-i63-L5-proposal-serve-jarloader-resource-bytes-from-the-class-path-archive-KEPT-OFF-20261019.md`).
// A `URLClassLoader` over plain `file:` jars gets one `URLClassPath$JarLoader`
// per jar, and defining a class it found reads the class file through
// `Resource.getBytes()` -- the JDK's read loop over the `ZipFile` entry
// stream. With the switch on, CratonVM answers that call from its own copy of
// the jar when the jar is plain (not Multi-Release, unsigned) and the
// `JarEntry`'s CRC and size match the VM's central directory.
//
// What must not change (and what the rows print): which loader a class comes
// from, its code source location and signers, its size, the defined package
// and its manifest attributes (sealing included), the resource URL
// (`jar:file:...!/<name>`, the versioned real name for a Multi-Release jar),
// a resource's bytes, an empty class file's `ClassFormatError`, and a jar
// rewritten between two loaders (the second loader sees the new class).
// `a.jar` holds DEFLATED entries and a sealing manifest, `b.jar` STORED ones;
// `s.jar` (a signature file) and `c.jar` (Multi-Release, `pc.V` only under
// `META-INF/versions/9/`) are controls the intrinsic leaves to the body;
// `e.jar` holds a zero-byte class file. Two rounds on fresh loaders; `b.jar`
// is rewritten (another length) between them.
//
// Run (no setup; writes temporary jars and deletes them):
//   javac -d out L5W64JarLoaderBytesIndex.java
//   CRATONVM_JAR_LOADER_BYTES_INDEX=1 cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W64JarLoaderBytesIndex
//
// Positive control: with `CRATONVM_DBG=access` the LOADER census line reads
// `jar-loader-bytes-served=7 jar-loader-bytes-body=5` with the switch on
// (round 1: A, A2, B, E served, S and V body; round 2: A, A2, E served, the
// rewritten B, S and V body -- the VM drops a jar whose length or mtime
// changed) and `0` / `0` with it off (not registered); the output is the
// same either way.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// class-A-1=loader=true source=file:<dir>/a.jar signers=null size=3000
// class-A2-1=loader=true source=file:<dir>/a.jar signers=null size=2000
// class-B-1=loader=true source=file:<dir>/b.jar signers=null size=2500
// class-S-1=loader=true source=file:<dir>/s.jar signers=null size=1500
// class-V-1=loader=true source=file:<dir>/c.jar signers=null size=1200
// empty-1=java.lang.ClassFormatError
// pkg-pa-1=title=ta vendor=va version=1.0 sealed=true
// pkg-pb-1=title=null vendor=null version=null sealed=false
// url-A-1=jar:file:<dir>/a.jar!/pa/A.class
// url-B-1=jar:file:<dir>/b.jar!/pb/B.class
// url-V-1=jar:file:<dir>/c.jar!/META-INF/versions/9/pc/V.class
// data-1=hello from b
// class-A-2=loader=true source=file:<dir>/a.jar signers=null size=3000
// class-A2-2=loader=true source=file:<dir>/a.jar signers=null size=2000
// class-B-2=loader=true source=file:<dir>/b.jar signers=null size=2600
// class-S-2=loader=true source=file:<dir>/s.jar signers=null size=1500
// class-V-2=loader=true source=file:<dir>/c.jar signers=null size=1200
// empty-2=java.lang.ClassFormatError
// pkg-pa-2=title=ta vendor=va version=1.0 sealed=true
// pkg-pb-2=title=null vendor=null version=null sealed=false
// url-A-2=jar:file:<dir>/a.jar!/pa/A.class
// url-B-2=jar:file:<dir>/b.jar!/pb/B.class
// url-V-2=jar:file:<dir>/c.jar!/META-INF/versions/9/pc/V.class
// data-2=hello again from b
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W64JarLoaderBytesIndex {
    static String dirPrefix;

    /**
     * `public class <name> extends Object {}` (version 49), padded with an
     * unused `CONSTANT_Utf8` so the class file is exactly `size` bytes (the
     * body's read loop then takes several steps).
     */
    static byte[] paddedClass(String name, int size) throws IOException {
        // header 10, #1 3+n, #2 3, #3 3+16, #4 3, #5's header 3, the 7 u2 tail 14.
        int fixed = 10 + (3 + name.length()) + 3 + (3 + "java/lang/Object".length()) + 3 + 3 + 14;
        int pad = size - fixed;
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < pad; i++) {
            text.append((char) ('a' + i % 26));
        }
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(6);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF("java/lang/Object");
        out.writeByte(7); out.writeShort(3);
        out.writeByte(1); out.writeUTF(text.toString());
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        byte[] b = bytes.toByteArray();
        if (b.length != size) {
            throw new IllegalStateException("padded class is " + b.length + " bytes, not " + size);
        }
        return b;
    }

    static void stored(ZipOutputStream zip, String name, byte[] data) throws IOException {
        ZipEntry entry = new ZipEntry(name);
        entry.setMethod(ZipEntry.STORED);
        entry.setSize(data.length);
        entry.setCompressedSize(data.length);
        CRC32 crc = new CRC32();
        crc.update(data);
        entry.setCrc(crc.getValue());
        zip.putNextEntry(entry);
        zip.write(data);
        zip.closeEntry();
    }

    /** A jar with an optional manifest; entries DEFLATED unless `store`. */
    static void writeJar(Path path, Manifest manifest, Map<String, byte[]> entries, boolean store)
            throws IOException {
        try (OutputStream file = Files.newOutputStream(path);
                ZipOutputStream zip = manifest == null ? new ZipOutputStream(file)
                        : new JarOutputStream(file, manifest)) {
            for (Map.Entry<String, byte[]> e : entries.entrySet()) {
                if (store) {
                    stored(zip, e.getKey(), e.getValue());
                } else {
                    zip.putNextEntry(new ZipEntry(e.getKey()));
                    zip.write(e.getValue());
                    zip.closeEntry();
                }
            }
        }
    }

    static void writeB(Path path, int size, String data) throws IOException {
        Map<String, byte[]> b = new LinkedHashMap<>();
        b.put("pb/", new byte[0]);
        b.put("pb/B.class", paddedClass("pb/B", size));
        b.put("pb/data.txt", data.getBytes(StandardCharsets.UTF_8));
        writeJar(path, null, b, true);
    }

    static String rel(Object o) {
        return String.valueOf(o).replace(dirPrefix, "file:<dir>/");
    }

    static int size(ClassLoader cl, String resource) throws IOException {
        try (InputStream in = cl.getResourceAsStream(resource)) {
            return in == null ? -1 : in.readAllBytes().length;
        }
    }

    static void classRow(URLClassLoader ucl, int n, String label, String cn) {
        try {
            Class<?> c = ucl.loadClass(cn);
            Object signers = c.getProtectionDomain().getCodeSource().getCodeSigners();
            System.out.println("class-" + label + "-" + n + "=loader=" + (c.getClassLoader() == ucl)
                    + " source=" + rel(c.getProtectionDomain().getCodeSource().getLocation())
                    + " signers=" + signers + " size=" + size(ucl, cn.replace('.', '/') + ".class"));
        } catch (Throwable t) {
            System.out.println("class-" + label + "-" + n + "=" + t);
        }
    }

    static void pkgRow(URLClassLoader ucl, int n, String cn) throws Exception {
        Package p = ucl.loadClass(cn).getPackage();
        System.out.println("pkg-" + p.getName() + "-" + n + "=title=" + p.getImplementationTitle()
                + " vendor=" + p.getImplementationVendor() + " version=" + p.getImplementationVersion()
                + " sealed=" + p.isSealed());
    }

    static void round(int n, URL[] urls) throws Exception {
        try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
            classRow(ucl, n, "A", "pa.A");
            classRow(ucl, n, "A2", "pa.A2");
            classRow(ucl, n, "B", "pb.B");
            classRow(ucl, n, "S", "ps.S");
            classRow(ucl, n, "V", "pc.V");
            try {
                ucl.loadClass("pe.E");
                System.out.println("empty-" + n + "=loaded");
            } catch (Throwable t) {
                System.out.println("empty-" + n + "=" + t.getClass().getName());
            }
            pkgRow(ucl, n, "pa.A");
            pkgRow(ucl, n, "pb.B");
            System.out.println("url-A-" + n + "=" + rel(ucl.getResource("pa/A.class")));
            System.out.println("url-B-" + n + "=" + rel(ucl.getResource("pb/B.class")));
            System.out.println("url-V-" + n + "=" + rel(ucl.getResource("pc/V.class")));
            try (InputStream in = ucl.getResourceAsStream("pb/data.txt")) {
                System.out.println("data-" + n + "="
                        + (in == null ? "null" : new String(in.readAllBytes(), StandardCharsets.UTF_8)));
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w64-jars");
        dirPrefix = dir.toUri().toURL().toString();
        if (!dirPrefix.endsWith("/")) {
            dirPrefix += "/";
        }
        Path a = dir.resolve("a.jar");
        Path b = dir.resolve("b.jar");
        Path s = dir.resolve("s.jar");
        Path c = dir.resolve("c.jar");
        Path e = dir.resolve("e.jar");
        try {
            Manifest am = new Manifest();
            Attributes main = am.getMainAttributes();
            main.put(Attributes.Name.MANIFEST_VERSION, "1.0");
            main.put(Attributes.Name.IMPLEMENTATION_TITLE, "ta");
            main.put(Attributes.Name.IMPLEMENTATION_VENDOR, "va");
            main.put(Attributes.Name.IMPLEMENTATION_VERSION, "1.0");
            main.put(Attributes.Name.SEALED, "true");
            Map<String, byte[]> ae = new LinkedHashMap<>();
            ae.put("pa/", new byte[0]);
            ae.put("pa/A.class", paddedClass("pa/A", 3000));
            ae.put("pa/A2.class", paddedClass("pa/A2", 2000));
            writeJar(a, am, ae, false);

            writeB(b, 2500, "hello from b");

            Map<String, byte[]> se = new LinkedHashMap<>();
            se.put("META-INF/", new byte[0]);
            se.put("META-INF/SIGNER.SF", "Signature-Version: 1.0\r\n\r\n".getBytes(StandardCharsets.UTF_8));
            se.put("ps/", new byte[0]);
            se.put("ps/S.class", paddedClass("ps/S", 1500));
            writeJar(s, null, se, false);

            Manifest cm = new Manifest();
            cm.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
            cm.getMainAttributes().put(Attributes.Name.MULTI_RELEASE, "true");
            Map<String, byte[]> ce = new LinkedHashMap<>();
            ce.put("META-INF/versions/9/pc/V.class", paddedClass("pc/V", 1200));
            writeJar(c, cm, ce, false);

            Map<String, byte[]> ee = new LinkedHashMap<>();
            ee.put("pe/E.class", new byte[0]);
            writeJar(e, null, ee, false);

            URL[] urls = {
                a.toUri().toURL(), b.toUri().toURL(), s.toUri().toURL(), c.toUri().toURL(), e.toUri().toURL()
            };
            round(1, urls);
            writeB(b, 2600, "hello again from b");
            round(2, urls);
        } finally {
            for (Path p : new Path[] {a, b, s, c, e}) {
                Files.deleteIfExists(p);
            }
            Files.deleteIfExists(dir);
        }
    }
}
