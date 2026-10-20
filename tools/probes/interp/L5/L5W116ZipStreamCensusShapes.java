// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L5: the four inflater shapes the
// `CRATONVM_DBG=zip-stream-census` census tells apart (proposal
// `i64-L5-proposal-serve-zipfile-entry-streams-from-the-vm-archive`, "Wave 116"):
//   * a class read from a jar (`Resource.getBytes` under the JDK's loader),
//   * a resource stream of a jar entry read by `Properties.load` (a ZipFile
//     entry stream OUTSIDE getBytes, what the proposal would serve),
//   * a GZIP stream, * an `Inflater` driven directly.
// Self-contained: the jar is written at run time. stdout is deterministic;
// with the census on, stderr carries `[zip-stream-census]` lines whose
// `kind=` / `via=` split is what to read (counts are not compared).
//
// Run: javac -d . L5W116ZipStreamCensusShapes.java
//      java -cp . L5W116ZipStreamCensusShapes                     (HotSpot reference)
//      cratonvm --jdk-only -cp . L5W116ZipStreamCensusShapes
//      CRATONVM_DBG=zip-stream-census cratonvm --jdk-only -cp . L5W116ZipStreamCensusShapes 2>census.txt
//      (and --compatible). Expected in census.txt: a `kind=zipfile-entry via=Resource.getBytes` row,
//      a `kind=zipfile-entry via=other` row whose consumer is a `Properties` frame,
//      a `kind=gzip` row and a `kind=direct` row naming this class's `main`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// class-from-jar leaf
// properties-from-jar 200 v-123
// gzip-round-trip 20000
// direct-inflate 20000
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.Properties;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.zip.Deflater;
import java.util.zip.GZIPInputStream;
import java.util.zip.GZIPOutputStream;
import java.util.zip.Inflater;

public class L5W116ZipStreamCensusShapes {
    public static class Leaf {
        @Override
        public String toString() {
            return "leaf";
        }
    }

    static final String LEAF = "L5W116ZipStreamCensusShapes$Leaf";

    static byte[] bytesOf(String resource) throws Exception {
        try (InputStream in = L5W116ZipStreamCensusShapes.class.getResourceAsStream(resource)) {
            return in.readAllBytes();
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w116").toFile();
        File jar = new File(dir, "census.jar");
        StringBuilder props = new StringBuilder();
        for (int i = 0; i < 200; i++) {
            props.append("k").append(i).append("=v-").append(i).append('\n');
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            out.putNextEntry(new JarEntry(LEAF + ".class"));
            out.write(bytesOf(LEAF + ".class"));
            out.closeEntry();
            out.putNextEntry(new JarEntry("app.properties"));
            out.write(props.toString().getBytes(StandardCharsets.ISO_8859_1));
            out.closeEntry();
        }
        try (URLClassLoader l = new URLClassLoader(new URL[] {jar.toURI().toURL()},
                ClassLoader.getPlatformClassLoader())) {
            Object leaf = l.loadClass(LEAF).getConstructor().newInstance();
            System.out.println("class-from-jar " + leaf);
            Properties p = new Properties();
            try (InputStream in = l.getResourceAsStream("app.properties")) {
                p.load(in);
            }
            System.out.println("properties-from-jar " + p.size() + " " + p.getProperty("k123"));
        }

        byte[] data = new byte[20000];
        for (int i = 0; i < data.length; i++) {
            data[i] = (byte) ("census".charAt(i % 6) + i % 7);
        }
        ByteArrayOutputStream gz = new ByteArrayOutputStream();
        try (GZIPOutputStream out = new GZIPOutputStream(gz)) {
            out.write(data);
        }
        try (GZIPInputStream in = new GZIPInputStream(new ByteArrayInputStream(gz.toByteArray()))) {
            System.out.println("gzip-round-trip " + in.readAllBytes().length);
        }

        Deflater d = new Deflater();
        d.setInput(data);
        d.finish();
        byte[] packed = new byte[40000];
        int n = d.deflate(packed);
        d.end();
        Inflater inf = new Inflater();
        inf.setInput(packed, 0, n);
        byte[] back = new byte[data.length];
        int got = 0;
        while (!inf.finished() && got < back.length) {
            got += inf.inflate(back, got, back.length - got);
        }
        inf.end();
        System.out.println("direct-inflate " + got);

        jar.delete();
        dir.delete();
    }
}
