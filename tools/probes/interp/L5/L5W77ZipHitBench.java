// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 77, lane L5 -- `ZipFile$Source.getEntryPos` with
// the hit answered in the same native as the miss
// (`docs/internal/fixed-bugs/interpreter-i76-L5-proposal-zip-entry-hit-in-one-native-20261109-DONE-20261110.md`).
// Every lookup runs in a static method called once per iteration, so with
// the JIT on the caller is method-compiled (wave 76 measured the miss
// intrinsic through W76ZipMissBench, whose 800-iteration lambda loops never
// leave the interpreter: see i76-L5-proposal-osr-loops-pay-the-monitor-helper).
//
// Switches it A/Bs (both default OFF; the same answers either way):
//   CRATONVM_ZIP_ENTRY_MISS_INTRINSIC=1  the wave-76 miss-only intrinsic: a
//       hit walks natively to its hash match, then the JDK body walks again.
//   CRATONVM_ZIP_ENTRY_HIT_INTRINSIC=1   the same row answering a hit too
//       (wins over the miss switch when both are set).
//
// Rows (8000 lookups a round over 4 jars x 200 entries `h<j>/Aa<i>.class`):
//   zip-get-entry          ZipFile.getEntry hit (L + the ZipEntry build).
//   jar-get-entry          the same through JarFile.getJarEntry.
//   zip-miss-entry         getEntry of the name in the NEXT jar (no hash
//                          match: the miss row).
//   zip-miss-collide       getEntry of `h<j>/BB<i>.class` in the jar holding
//                          `h<j>/Aa<i>.class` (same String.hashCode: a hash
//                          match whose compare fails, then the end of chain).
//   zip-open-close-lookup  getInputStream(entry fetched before) + close():
//                          the stream objects plus one addSlash=false lookup.
//
// Expected with HIT=1 against off and MISS=1, under --nojit: zip-get-entry and
// jar-get-entry lower than both (no double walk), zip-miss-collide lower,
// zip-miss-entry as MISS=1. With the JIT: measure before any flip; if HIT=1
// loses there the flip registers it only without a JIT
// (`register_zip_real_natives`: `&& !r.jit_enabled()`, as the wave-76
// StackTraceElement.toString intrinsic).
//
// stdout: per row one deterministic "<row> <counts>" line (no "ns/") and one
// timing line "<row> <n> ns/op (median of rounds 1..ROUNDS-1, per lookup)".
// One row only: `L5W77ZipHitBench <row>`.
//
// A/B ($B the cratonvm binary, $J the JDK 25 bin directory; interleaved,
// three repetitions, medians):
//   $J/javac -d out L5W77ZipHitBench.java L5W77ZipEntryHit.java
//   for JIT in "" --nojit; do for SW in CRATONVM_NONE=1 CRATONVM_ZIP_ENTRY_MISS_INTRINSIC=1 \
//       CRATONVM_ZIP_ENTRY_HIT_INTRINSIC=1; do
//     env $SW $B --java-home $J/.. $JIT -cp out L5W77ZipHitBench
//   done; done
//   $J/java -cp out L5W77ZipHitBench                                              # HotSpot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// zip-get-entry found=8000
// jar-get-entry found=8000
// zip-miss-entry missed=8000
// zip-miss-collide missed=8000
// zip-open-close-lookup opened=8000
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.util.Arrays;
import java.util.jar.JarFile;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W77ZipHitBench {
    static final int JARS = 4;
    static final int PER_JAR = 200;
    static final int NAMES = JARS * PER_JAR;
    static final int ITERS = 8000;
    static final int ROUNDS = 7;

    static final File[] FILES = new File[JARS];
    static final String[] NAME = new String[NAMES];
    static final String[] COLLIDE = new String[NAMES];
    static final int[] JAR_OF = new int[NAMES];
    static ZipFile[] zips;
    static JarFile[] jars;
    static ZipEntry[] entries;
    static long count;

    interface Body {
        void run(int c) throws Exception;
    }

    static void zipGet(int c) {
        if (zips[JAR_OF[c]].getEntry(NAME[c]) != null) {
            count++;
        }
    }

    static void jarGet(int c) {
        if (jars[JAR_OF[c]].getJarEntry(NAME[c]) != null) {
            count++;
        }
    }

    static void zipMiss(int c) {
        if (zips[(JAR_OF[c] + 1) % JARS].getEntry(NAME[c]) == null) {
            count++;
        }
    }

    static void zipMissCollide(int c) {
        if (zips[JAR_OF[c]].getEntry(COLLIDE[c]) == null) {
            count++;
        }
    }

    static void openCloseLookup(int c) throws IOException {
        try (InputStream in = zips[JAR_OF[c]].getInputStream(entries[c])) {
            if (in != null) {
                count++;
            }
        }
    }

    static void row(String label, String what, Body body) throws Exception {
        long[] perLookup = new long[ROUNDS];
        long last = 0;
        for (int r = 0; r < ROUNDS; r++) {
            count = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < ITERS; i++) {
                body.run(i % NAMES);
            }
            perLookup[r] = (System.nanoTime() - t0) / ITERS;
            last = count;
        }
        long[] later = Arrays.copyOfRange(perLookup, 1, ROUNDS);
        Arrays.sort(later);
        System.out.println(label + " " + what + "=" + last);
        System.out.println(label + " " + later[later.length / 2]
                + " ns/op (median of rounds 1.." + (ROUNDS - 1) + ", per lookup)");
    }

    static boolean wanted(String only, String row) {
        return only == null || only.equals(row);
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        File dir = Files.createTempDirectory("l5w77h").toFile();
        try {
            byte[] body = new byte[64];
            for (int j = 0; j < JARS; j++) {
                FILES[j] = new File(dir, "h" + j + ".jar");
                try (ZipOutputStream out = new ZipOutputStream(Files.newOutputStream(FILES[j].toPath()))) {
                    for (int i = 0; i < PER_JAR; i++) {
                        int c = j * PER_JAR + i;
                        NAME[c] = "h" + j + "/Aa" + i + ".class";
                        COLLIDE[c] = "h" + j + "/BB" + i + ".class";
                        JAR_OF[c] = j;
                        out.putNextEntry(new ZipEntry(NAME[c]));
                        out.write(body);
                        out.closeEntry();
                    }
                }
            }
            zips = new ZipFile[JARS];
            jars = new JarFile[JARS];
            for (int j = 0; j < JARS; j++) {
                zips[j] = new ZipFile(FILES[j]);
                jars[j] = new JarFile(FILES[j]);
            }
            entries = new ZipEntry[NAMES];
            for (int c = 0; c < NAMES; c++) {
                entries[c] = zips[JAR_OF[c]].getEntry(NAME[c]);
            }
            // Leave every ZipFile's `lastEntryName` on a name the loops below
            // never ask for, so zip-open-close-lookup looks each name up.
            for (int j = 0; j < JARS; j++) {
                zips[j].getEntry("h" + j + "/Aa" + (PER_JAR - 1) + ".class");
                zips[j].getEntry(NAME[j * PER_JAR]);
            }
            try {
                if (wanted(only, "zip-get-entry")) {
                    row("zip-get-entry", "found", L5W77ZipHitBench::zipGet);
                }
                if (wanted(only, "jar-get-entry")) {
                    row("jar-get-entry", "found", L5W77ZipHitBench::jarGet);
                }
                if (wanted(only, "zip-miss-entry")) {
                    row("zip-miss-entry", "missed", L5W77ZipHitBench::zipMiss);
                }
                if (wanted(only, "zip-miss-collide")) {
                    row("zip-miss-collide", "missed", L5W77ZipHitBench::zipMissCollide);
                }
                if (wanted(only, "zip-open-close-lookup")) {
                    // zip-get-entry moved `lastEntryName` onto the last name it
                    // asked for, which this row asks for once per pass: one
                    // shortcut in 200 lookups.
                    row("zip-open-close-lookup", "opened", L5W77ZipHitBench::openCloseLookup);
                }
            } finally {
                for (int j = 0; j < JARS; j++) {
                    zips[j].close();
                    jars[j].close();
                }
            }
        } finally {
            for (File f : FILES) {
                if (f != null) {
                    f.delete();
                }
            }
            dir.delete();
        }
    }
}
