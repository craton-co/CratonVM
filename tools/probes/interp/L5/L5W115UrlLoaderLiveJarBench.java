// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L5: many live `URLClassLoader`s over the
// SAME jar, the test-suite shape `CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD`
// (default off) serves without reading the jar again per loader (proposal
// `i100-L5-proposal-loader-area-directions` item 1, "Wave 115"). Self-contained:
// the jar (one class plus 4000 filler entries) is written at run time into a
// temp directory. Every compared line must be the same with the switch off and
// on, both modes, JIT and --nojit. `TIME ` lines are the bench.
//
// Run: javac -d . L5W115UrlLoaderLiveJarBench.java
//      java -cp . L5W115UrlLoaderLiveJarBench                                   (HotSpot reference)
//      cratonvm --compatible -cp . L5W115UrlLoaderLiveJarBench
//      CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD=1 cratonvm --compatible -cp . L5W115UrlLoaderLiveJarBench
//      (and the same two with --jdk-only)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// first-defined-by-own true
// first-resource true
// first-filler-text filler-1234
// live-loaders 120
// live-defined-by-own 120
// live-distinct-classes 120
// live-resources 120
// live-filler-text 120
// closed-half 60
// after-close-defined-by-own 60
// after-close-resources 60
// second-jar-class-from-second true
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

public class L5W115UrlLoaderLiveJarBench {
    public static class Leaf {
        public static String hello() {
            return "leaf";
        }
    }

    static final String LEAF = "L5W115UrlLoaderLiveJarBench$Leaf";

    static byte[] bytesOf(String resource) throws Exception {
        try (InputStream in = L5W115UrlLoaderLiveJarBench.class.getResourceAsStream(resource)) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            in.transferTo(out);
            return out.toByteArray();
        }
    }

    static File writeJar(File dir, String name, int fillers) throws Exception {
        File jar = new File(dir, name);
        byte[] leaf = bytesOf(LEAF + ".class");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            out.putNextEntry(new JarEntry(LEAF + ".class"));
            out.write(leaf);
            out.closeEntry();
            for (int i = 0; i < fillers; i++) {
                out.putNextEntry(new JarEntry("filler/" + i + ".txt"));
                out.write(("filler-" + i).getBytes(StandardCharsets.US_ASCII));
                out.closeEntry();
            }
        }
        return jar;
    }

    static String read(URLClassLoader l, String name) throws Exception {
        try (InputStream in = l.getResourceAsStream(name)) {
            return in == null ? "null" : new String(in.readAllBytes(), StandardCharsets.US_ASCII);
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w115").toFile();
        File jar = writeJar(dir, "live.jar", 4000);
        URL[] urls = {jar.toURI().toURL()};
        ClassLoader parent = ClassLoader.getPlatformClassLoader();

        URLClassLoader first = new URLClassLoader(urls, parent);
        Class<?> c0 = first.loadClass(LEAF);
        System.out.println("first-defined-by-own " + (c0.getClassLoader() == first));
        System.out.println("first-resource " + (first.getResource("filler/7.txt") != null));
        System.out.println("first-filler-text " + read(first, "filler/1234.txt"));

        int n = 120;
        List<URLClassLoader> live = new ArrayList<>();
        Set<Class<?>> distinct = new HashSet<>();
        int own = 0;
        int resources = 0;
        int texts = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            URLClassLoader l = new URLClassLoader(urls, parent);
            live.add(l);
            Class<?> c = l.loadClass(LEAF);
            if (c.getClassLoader() == l) {
                own++;
            }
            distinct.add(c);
        }
        long t1 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            URLClassLoader l = live.get(i);
            if (l.getResource("filler/" + (i * 31 % 4000) + ".txt") != null) {
                resources++;
            }
            if (read(l, "filler/" + i + ".txt").equals("filler-" + i)) {
                texts++;
            }
        }
        long t2 = System.nanoTime();
        System.out.println("live-loaders " + live.size());
        System.out.println("live-defined-by-own " + own);
        System.out.println("live-distinct-classes " + distinct.size());
        System.out.println("live-resources " + resources);
        System.out.println("live-filler-text " + texts);

        // Close every other loader; the rest keep serving.
        int closed = 0;
        for (int i = 0; i < n; i += 2) {
            live.get(i).close();
            closed++;
        }
        System.out.println("closed-half " + closed);
        int afterOwn = 0;
        int afterRes = 0;
        for (int i = 1; i < n; i += 2) {
            URLClassLoader l = new URLClassLoader(urls, parent);
            live.set(i - 1, l);
            if (l.loadClass(LEAF).getClassLoader() == l) {
                afterOwn++;
            }
            if (l.getResource("filler/" + i + ".txt") != null) {
                afterRes++;
            }
        }
        System.out.println("after-close-defined-by-own " + afterOwn);
        System.out.println("after-close-resources " + afterRes);

        // A second, different jar is still read (the switch skips only a jar
        // already on the class path).
        File jar2 = writeJar(dir, "second.jar", 10);
        URLClassLoader second = new URLClassLoader(new URL[] {jar2.toURI().toURL()}, parent);
        System.out.println("second-jar-class-from-second " + (second.loadClass(LEAF).getClassLoader() == second));

        System.out.println("TIME live-loader-define ns/loader " + (t1 - t0) / n);
        System.out.println("TIME live-loader-resource ns/loader " + (t2 - t1) / n);
        for (URLClassLoader l : live) {
            l.close();
        }
        first.close();
        second.close();
        // Best effort, not compared.
        jar.delete();
        jar2.delete();
        dir.delete();
    }
}
