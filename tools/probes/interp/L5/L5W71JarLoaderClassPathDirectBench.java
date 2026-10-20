// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 71, lane L5 -- what
// `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT` (default off) saves: the
// jar-loader bytes (`CRATONVM_JAR_LOADER_BYTES_INDEX`) of a jar that is ON
// THE VM'S APPLICATION CLASS PATH read straight from the class path's own
// entry, with no row in the jar cache -- no second map + parse (the default),
// and no clone of the directory, copy of the entry index or memo slot (the
// wave-70 lend, `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`)
// (`docs/internal/fixed-bugs/interpreter-i70-L5-proposal-serve-class-path-jar-bytes-without-a-row-KEPT-OFF-20261104.md`).
// The shape of `L5W70JarLoaderClassPathArchiveBench` (a `URLClassLoader`
// over the `-cp` jars' `file:` URLs, parent the platform loader, stands in
// for the `--jdk-only` `AppClassLoader` so every round defines every class
// afresh), plus a PARALLEL round: THREADS threads, each with its own loader
// over all the jars, at once -- the direct path reads under the class path
// entry's own `ZipArchive` lock (shared with the VM's class lookups) where a
// row reads under its clone's, so this is where contention would show.
//
// Two steps: `--make <dir>` writes JARS jars `<dir>/l5w71j<k>.jar` of
// CLASSES deflated ~4 KB classes each (any JVM, deterministic bytes) and
// prints the class-path suffix; then the bench runs with them on -cp.
//
// Run (interleave the three cratonvm rows, take medians; Linux; $B the
// cratonvm binary, $J the JDK 25 bin directory):
//   $J/javac -d out L5W71JarLoaderClassPathDirectBench.java
//   CP=$($J/java -cp out L5W71JarLoaderClassPathDirectBench --make /tmp/l5w71b | tail -1)
//   I="CRATONVM_JAR_LOADER_BYTES_INDEX=1"
//   env $I CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT=1 $B --java-home $J/.. -cp out:$CP L5W71JarLoaderClassPathDirectBench   # direct
//   env $I CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE=1 $B --java-home $J/.. -cp out:$CP L5W71JarLoaderClassPathDirectBench  # wave-70 lend
//   env $I $B --java-home $J/.. -cp out:$CP L5W71JarLoaderClassPathDirectBench                                           # rows (control)
//   $J/java -cp out:$CP L5W71JarLoaderClassPathDirectBench                                                               # HotSpot
// System calls (`strace -f -c -e trace=openat,mmap,newfstatat,statx`): with
// the switch no `openat`/`mmap` per jar and one `stat` per class (the stamp
// check), as a row hit has.
//
// Prints to stderr: `round=<k> ns/class=<n>`, `first-round ns/class=<n>`,
// `later-rounds median ns/class=<n>`, `parallel-<THREADS> ns/class=<n>`
// (wall time over THREADS x JARS x CLASSES), and after round 0
// `jar-mappings=<n> jar-mapped-kib=<n> VmRSS-kib=<n>` (`maps=unavailable`
// off Linux): JARS mappings with the switch or the lend, 2 x JARS with
// neither. stdout is the same on every VM. Positive control:
// `CRATONVM_DBG=access` LOADER census `jar-loader-class-path-direct=<JARS *
// CLASSES * (ROUNDS + THREADS)>`, `jar-loader-class-path-direct-fallback=0`
// and `nested-jar-rows-built=0` with the switch.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// on-class-path=true
// loaded=1600 per round
// parallel-loaded=6400
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W71JarLoaderClassPathDirectBench {
    static final int JARS = 32;
    static final int CLASSES = 50;
    static final int ROUNDS = 7;
    static final int THREADS = 4;
    static final long ENTRY_TIME = 1577836800000L;

    /** `public class <name> extends Object {}` padded with an unused, compressible `CONSTANT_Utf8`. */
    static byte[] paddedClass(String name, int pad) throws IOException {
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < pad; i++) {
            text.append((char) ('a' + (i * 7 + name.length()) % 26));
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
        return bytes.toByteArray();
    }

    static String className(int jar, int i) {
        return "p" + jar + ".C" + i;
    }

    static void make(Path dir) throws IOException {
        Files.createDirectories(dir);
        StringBuilder cp = new StringBuilder();
        for (int j = 0; j < JARS; j++) {
            Path jar = dir.resolve("l5w71j" + j + ".jar").toAbsolutePath();
            try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
                ZipEntry pkg = new ZipEntry("p" + j + "/");
                pkg.setTime(ENTRY_TIME);
                zip.putNextEntry(pkg);
                zip.closeEntry();
                for (int i = 0; i < CLASSES; i++) {
                    String internal = className(j, i).replace('.', '/');
                    ZipEntry entry = new ZipEntry(internal + ".class");
                    entry.setTime(ENTRY_TIME);
                    zip.putNextEntry(entry);
                    zip.write(paddedClass(internal, 4000));
                    zip.closeEntry();
                }
            }
            if (j > 0) {
                cp.append(File.pathSeparator);
            }
            cp.append(jar);
        }
        System.out.println(cp);
    }

    /** The bench's jars on the class path (absolute), in class-path order. */
    static List<Path> jarsOnClassPath() {
        List<Path> jars = new ArrayList<>();
        for (String element : System.getProperty("java.class.path").split(File.pathSeparator)) {
            String file = Paths.get(element).getFileName() == null ? "" : Paths.get(element).getFileName().toString();
            if (file.startsWith("l5w71j") && file.endsWith(".jar")) {
                jars.add(Paths.get(element).toAbsolutePath());
            }
        }
        return jars;
    }

    /** `jar-mappings=<n> jar-mapped-kib=<n> VmRSS-kib=<n>` from /proc/self, or `maps=unavailable`. */
    static String mappings(List<Path> jars) {
        Path maps = Paths.get("/proc/self/maps");
        if (!Files.isReadable(maps)) {
            return "maps=unavailable";
        }
        try {
            Set<String> names = new HashSet<>();
            for (Path jar : jars) {
                names.add(jar.toString());
                names.add(jar.toRealPath().toString());
            }
            int count = 0;
            long bytes = 0;
            for (String line : Files.readAllLines(maps)) {
                int slash = line.indexOf('/');
                if (slash < 0 || !names.contains(line.substring(slash).trim())) {
                    continue;
                }
                String[] range = line.substring(0, line.indexOf(' ')).split("-");
                count++;
                bytes += Long.parseUnsignedLong(range[1], 16) - Long.parseUnsignedLong(range[0], 16);
            }
            String rss = "?";
            for (String line : Files.readAllLines(Paths.get("/proc/self/status"))) {
                if (line.startsWith("VmRSS:")) {
                    rss = line.substring("VmRSS:".length()).trim().split("\\s+")[0];
                }
            }
            return "jar-mappings=" + count + " jar-mapped-kib=" + (bytes / 1024) + " VmRSS-kib=" + rss;
        } catch (IOException | RuntimeException e) {
            return "maps=" + e.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("--make")) {
            make(Paths.get(args[1]));
            return;
        }
        List<Path> jars = jarsOnClassPath();
        System.out.println("on-class-path=" + (jars.size() == JARS));
        if (jars.size() != JARS) {
            return;
        }
        URL[] urls = new URL[JARS];
        for (int j = 0; j < JARS; j++) {
            urls[j] = jars.get(j).toUri().toURL();
        }
        long[] perClass = new long[ROUNDS];
        int loaded = 0;
        for (int r = 0; r < ROUNDS; r++) {
            loaded = 0;
            try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
                long t0 = System.nanoTime();
                for (int j = 0; j < JARS; j++) {
                    for (int i = 0; i < CLASSES; i++) {
                        if (ucl.loadClass(className(j, i)).getClassLoader() == ucl) {
                            loaded++;
                        }
                    }
                }
                perClass[r] = (System.nanoTime() - t0) / ((long) JARS * CLASSES);
                if (r == 0) {
                    // While the loader (and the JDK's JarFiles) are still open.
                    System.err.println(mappings(jars));
                }
            }
            System.err.println("round=" + r + " ns/class=" + perClass[r]);
        }
        long[] later = Arrays.copyOfRange(perClass, 1, ROUNDS);
        Arrays.sort(later);
        System.err.println("first-round ns/class=" + perClass[0]);
        System.err.println("later-rounds median ns/class=" + later[later.length / 2]);
        System.out.println("loaded=" + loaded + " per round");

        // The parallel round: THREADS loaders at once, each over every jar.
        AtomicInteger parallelLoaded = new AtomicInteger();
        CountDownLatch start = new CountDownLatch(1);
        Thread[] threads = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            threads[t] = new Thread(() -> {
                try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
                    start.await();
                    for (int j = 0; j < JARS; j++) {
                        for (int i = 0; i < CLASSES; i++) {
                            if (ucl.loadClass(className(j, i)).getClassLoader() == ucl) {
                                parallelLoaded.incrementAndGet();
                            }
                        }
                    }
                } catch (Exception e) {
                    System.out.println("parallel-error=" + e.getClass().getName());
                }
            });
            threads[t].start();
        }
        long p0 = System.nanoTime();
        start.countDown();
        for (Thread thread : threads) {
            thread.join();
        }
        long parallelNs = System.nanoTime() - p0;
        System.err.println("parallel-" + THREADS + " ns/class=" + parallelNs / ((long) THREADS * JARS * CLASSES));
        System.out.println("parallel-loaded=" + parallelLoaded.get());
    }
}
