// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 76, lane L5 -- the cost of a `ZipFile` entry
// miss, the lookup every class-path jar before a class's own jar answers
// (`docs/internal/fixed-bugs/interpreter-i75-L5-proposal-zip-entry-miss-without-the-java-walk-20261108-KEPT-OFF-20261109.md`,
// "Progress (wave 76)").
//
// Switch it A/Bs (opt-in, default OFF, the same answers either way):
//   CRATONVM_ZIP_ENTRY_MISS_INTRINSIC=1  ZipFile$Source.getEntryPos answers a
//       no-hash-match miss natively; a hash hit runs the JDK body.
//
// 16 jars x 50 entries (`q<j>/Aa<i>.class`, a few bytes each). Rows:
//
//   zip-miss-entry    ZipFile.getEntry(path) on every jar before the path's
//                     own (W75's row shape: 6000 misses a round): the hash
//                     walk only. MUST drop with the switch.
//   jar-miss-entry    the same through JarFile.getJarEntry (JarFile.getEntry's
//                     isMultiRelease check around it). MUST drop.
//   zip-miss-collide  getEntry("q<j>/BB<i>.class") on the path's own jar: the
//                     same String.hashCode as a stored name, so a hash hit
//                     whose compare fails. The switch declines it (one native
//                     walk, then the JDK body): its cost with the switch.
//   zip-get-entry     getEntry(path) hits (the control: the declined path
//                     plus a ZipEntry). Must not move by more than the
//                     native walk.
//
// stdout: per row one deterministic "<row> <counts>" line (no "ns/") and one
// timing line "<row> <n> ns/op (median of rounds 1..ROUNDS-1, per lookup)".
// One row only: `L5W76ZipMissBench <row>`.
//
// A/B ($B the cratonvm binary, $J the JDK 25 bin directory; the same binary,
// interleaved, three repetitions, medians; with and without --nojit):
//   $J/javac -d out L5W76ZipMissBench.java
//   $B --java-home $J/.. -cp out L5W76ZipMissBench                                          # off
//   env CRATONVM_ZIP_ENTRY_MISS_INTRINSIC=1 $B --java-home $J/.. -cp out L5W76ZipMissBench  # on
//   $J/java -cp out L5W76ZipMissBench                                                       # HotSpot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// entries-per-round=800
// zip-miss-entry missed=6000
// jar-miss-entry missed=6000
// zip-miss-collide missed=800
// zip-get-entry found=800
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.jar.JarFile;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W76ZipMissBench {
    static final int JARS = 16;
    static final int ENTRIES = 50;
    static final int ROUNDS = 7;

    static final File[] JAR_FILES = new File[JARS];
    static final List<Integer> JAR_OF = new ArrayList<>();
    static final List<String> PATHS = new ArrayList<>();
    static final List<String> COLLIDE = new ArrayList<>();

    static void make(Path dir) throws IOException {
        for (int j = 0; j < JARS; j++) {
            Path jar = dir.resolve("l5w76m" + j + ".jar");
            JAR_FILES[j] = jar.toFile();
            try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
                for (int i = 0; i < ENTRIES; i++) {
                    String path = "q" + j + "/Aa" + i + ".class";
                    ZipEntry entry = new ZipEntry(path);
                    entry.setTime(1577836800000L);
                    zip.putNextEntry(entry);
                    zip.write(new byte[] {(byte) 0xCA, (byte) 0xFE, (byte) i, (byte) j});
                    zip.closeEntry();
                    JAR_OF.add(j);
                    PATHS.add(path);
                    COLLIDE.add("q" + j + "/BB" + i + ".class");
                    if (path.hashCode() != COLLIDE.get(COLLIDE.size() - 1).hashCode()) {
                        throw new AssertionError("no collision for " + path);
                    }
                }
            }
        }
    }

    static void row(String label, String count, int perRound, boolean jar, Lookup lookup) throws Exception {
        long[] perOp = new long[ROUNDS];
        long result = 0;
        for (int r = 0; r < ROUNDS; r++) {
            ZipFile[] zips = new ZipFile[JARS];
            for (int j = 0; j < JARS; j++) {
                zips[j] = jar ? new JarFile(JAR_FILES[j]) : new ZipFile(JAR_FILES[j]);
            }
            try {
                long t0 = System.nanoTime();
                result = lookup.run(zips);
                perOp[r] = (System.nanoTime() - t0) / perRound;
            } finally {
                for (ZipFile z : zips) {
                    z.close();
                }
            }
        }
        long[] later = Arrays.copyOfRange(perOp, 1, ROUNDS);
        Arrays.sort(later);
        System.out.println(label + " " + count + "=" + result);
        System.out.println(label + " " + later[later.length / 2]
                + " ns/op (median of rounds 1.." + (ROUNDS - 1) + ", per lookup)");
    }

    interface Lookup {
        long run(ZipFile[] zips) throws Exception;
    }

    static boolean wanted(String only, String row) {
        return only == null || only.equals(row);
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        Path dir = Files.createTempDirectory("l5w76m");
        try {
            make(dir);
            int total = JARS * ENTRIES;
            int missesPerRound = 0;
            for (int c = 0; c < total; c++) {
                missesPerRound += JAR_OF.get(c);
            }
            final int misses = missesPerRound;
            System.out.println("entries-per-round=" + total);

            if (wanted(only, "zip-miss-entry")) {
                row("zip-miss-entry", "missed", misses, false, zips -> {
                    long n = 0;
                    for (int c = 0; c < total; c++) {
                        String path = PATHS.get(c);
                        int own = JAR_OF.get(c);
                        for (int j = 0; j < own; j++) {
                            if (zips[j].getEntry(path) == null) {
                                n++;
                            }
                        }
                    }
                    return n;
                });
            }
            if (wanted(only, "jar-miss-entry")) {
                row("jar-miss-entry", "missed", misses, true, zips -> {
                    long n = 0;
                    for (int c = 0; c < total; c++) {
                        String path = PATHS.get(c);
                        int own = JAR_OF.get(c);
                        for (int j = 0; j < own; j++) {
                            if (((JarFile) zips[j]).getJarEntry(path) == null) {
                                n++;
                            }
                        }
                    }
                    return n;
                });
            }
            if (wanted(only, "zip-miss-collide")) {
                row("zip-miss-collide", "missed", total, false, zips -> {
                    long n = 0;
                    for (int c = 0; c < total; c++) {
                        if (zips[JAR_OF.get(c)].getEntry(COLLIDE.get(c)) == null) {
                            n++;
                        }
                    }
                    return n;
                });
            }
            if (wanted(only, "zip-get-entry")) {
                row("zip-get-entry", "found", total, false, zips -> {
                    long n = 0;
                    for (int c = 0; c < total; c++) {
                        if (zips[JAR_OF.get(c)].getEntry(PATHS.get(c)) != null) {
                            n++;
                        }
                    }
                    return n;
                });
            }
        } finally {
            for (File f : JAR_FILES) {
                if (f != null) {
                    f.delete();
                }
            }
            dir.toFile().delete();
        }
    }
}
