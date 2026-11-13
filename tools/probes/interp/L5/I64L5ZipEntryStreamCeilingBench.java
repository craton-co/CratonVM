// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// i64-L5 closure (2026-11-02): the CEILING of the proposal "serve ZipFile entry
// streams from the VM archive"
// (`i64-L5-proposal-serve-zipfile-entry-streams-from-the-vm-archive`).
//
// An intrinsic that serves `InflaterInputStream.read` of a `ZipFile` entry from
// a native buffer removes the inflate + the read loop's Java layers, but NOT
// `getInputStream` (the `ZipFileInputStream`, the cached `Inflater`, the
// Cleaner / `istreams` bookkeeping) nor `close`, and not the consumer's own
// work (`Properties.load`, a line reader). So the most it can save per stream is
//
//     ceiling = full - open-close - buffer
//
// where, per entry shape x consumer shape (medians over ROUNDS rounds, ns/stream):
//   full         getInputStream(e) + consume + close   (what runs today)
//   open-close   getInputStream(e) + close             (kept by the intrinsic)
//   buffer       consume(new ByteArrayInputStream(bytes))   (the consumer on a served buffer)
//   ceiling      max(0, full - open-close - buffer)    (printed as TIME ... ceiling)
// A stream's ceiling times the census's `streams-ended` of the start
// (`CRATONVM_DBG=zip-stream-census`, `kind=inflater-stream via=other`), divided by
// the start's wall time, is the upper bound of the share the intrinsic could win.
// The decision rule is in the page's "Closure (2026-11-02)" section.
//
// Entry shapes (all in one jar written at run time, deterministic bytes):
//   svc     90-byte deflated text   (META-INF/services/*, the ServiceLoader read)
//   props   2.5 KB deflated text    (a .properties bundle)
//   tld     24 KB deflated XML-ish text (a Tomcat TLD / orm.xml)
//   big     240 KB deflated mixed bytes
//   stored  3 KB STORED entry (never reaches the inflater; the control row)
// Consumer shapes: all (readAllBytes), props (Properties.load), lines
// (BufferedReader.readLine over UTF-8), chunk512 (read(byte[512]) loop).
//
// Every non-TIME line is deterministic and compared across runs / VMs.
//
// Run: javac -d . I64L5ZipEntryStreamCeilingBench.java
//      java -cp . I64L5ZipEntryStreamCeilingBench                         (HotSpot reference)
//      cratonvm --real-jdk [--nojit] -cp . I64L5ZipEntryStreamCeilingBench
//      cratonvm --compatible [--nojit] -cp . I64L5ZipEntryStreamCeilingBench
//   The lead's script: tools/probes/interp/L5/i64-l5-zip-entry-stream-sample.sh
//
// No HOTSPOT_EXPECTED block: the checksum lines are the A/B against `java`.

import java.io.BufferedReader;
import java.io.ByteArrayInputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.Arrays;
import java.util.Properties;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;

public class I64L5ZipEntryStreamCeilingBench {
    static final int ROUNDS = 7;
    /** Streams per timed batch (a batch is timed once; the median is over rounds). */
    static final int BATCH = 200;

    static final String[] ENTRY = {"svc", "props", "tld", "big", "stored"};
    static final String[] CONSUMER = {"all", "props", "lines", "chunk512"};

    static byte[] textBytes(int size, String stem) {
        StringBuilder sb = new StringBuilder(size + 64);
        int i = 0;
        while (sb.length() < size) {
            sb.append(stem).append(i).append('=').append("value-").append(i * 31 % 977).append('\n');
            i++;
        }
        sb.setLength(size - 1);
        sb.append('\n');
        return sb.toString().getBytes(StandardCharsets.ISO_8859_1);
    }

    static byte[] mixedBytes(int size) {
        byte[] b = new byte[size];
        int x = 12345;
        for (int i = 0; i < size; i++) {
            x = x * 1103515245 + 12345;
            b[i] = (i % 5 == 0) ? (byte) (x >>> 16) : (byte) ('a' + (i % 13));
        }
        return b;
    }

    static byte[] payload(String shape) {
        switch (shape) {
            case "svc": return textBytes(90, "p.S");
            case "props": return textBytes(2500, "key.");
            case "tld": return textBytes(24000, "tld.tag.");
            case "big": return mixedBytes(240000);
            default: return textBytes(3000, "stored.");
        }
    }

    static void put(JarOutputStream out, String name, byte[] data, boolean stored) throws IOException {
        JarEntry e = new JarEntry(name);
        if (stored) {
            e.setMethod(ZipEntry.STORED);
            e.setSize(data.length);
            e.setCompressedSize(data.length);
            CRC32 crc = new CRC32();
            crc.update(data);
            e.setCrc(crc.getValue());
        }
        out.putNextEntry(e);
        out.write(data);
        out.closeEntry();
    }

    // ---- consumers: each a static method called once per stream ----

    static long consumeAll(InputStream in) throws IOException {
        return sum(in.readAllBytes());
    }

    static long consumeProps(InputStream in) throws IOException {
        Properties p = new Properties();
        p.load(in);
        return p.size() * 31L + p.stringPropertyNames().size();
    }

    static long consumeLines(InputStream in) throws IOException {
        BufferedReader r = new BufferedReader(new InputStreamReader(in, StandardCharsets.UTF_8));
        long n = 0;
        String line;
        while ((line = r.readLine()) != null) {
            n += line.length() + 1;
        }
        return n;
    }

    static long consumeChunks(InputStream in) throws IOException {
        byte[] buf = new byte[512];
        long n = 0;
        int k;
        while ((k = in.read(buf, 0, buf.length)) > 0) {
            for (int j = 0; j < k; j++) {
                n = n * 31 + buf[j]; // independent of where the stream splits its chunks
            }
        }
        return n;
    }

    static long consume(int consumer, InputStream in) throws IOException {
        switch (consumer) {
            case 0: return consumeAll(in);
            case 1: return consumeProps(in);
            case 2: return consumeLines(in);
            default: return consumeChunks(in);
        }
    }

    static long sum(byte[] b) {
        long s = b.length;
        for (int i = 0; i < b.length; i += 7) {
            s = s * 31 + b[i];
        }
        return s;
    }

    // ---- the three timed shapes, one stream per call ----

    static long full(JarFile jf, JarEntry e, int consumer) throws IOException {
        try (InputStream in = jf.getInputStream(e)) {
            return consume(consumer, in);
        }
    }

    static long openClose(JarFile jf, JarEntry e) throws IOException {
        try (InputStream in = jf.getInputStream(e)) {
            return in == null ? 0 : 1;
        }
    }

    static long buffer(byte[] data, int consumer) throws IOException {
        return consume(consumer, new ByteArrayInputStream(data));
    }

    static long median(long[] v) {
        long[] c = v.clone();
        Arrays.sort(c);
        return c[c.length / 2];
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("i64l5ceil").toFile();
        File jar = new File(dir, "ceiling.jar");
        byte[][] data = new byte[ENTRY.length][];
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            for (int s = 0; s < ENTRY.length; s++) {
                data[s] = payload(ENTRY[s]);
                put(out, "e/" + ENTRY[s], data[s], ENTRY[s].equals("stored"));
            }
        }
        long totalStreams = 0;
        try (JarFile jf = new JarFile(jar)) {
            for (int s = 0; s < ENTRY.length; s++) {
                JarEntry e = jf.getJarEntry("e/" + ENTRY[s]);
                System.out.println("entry " + ENTRY[s] + " size " + e.getSize() + " method "
                        + (e.getMethod() == ZipEntry.STORED ? "STORED" : "DEFLATED"));
                long[] openClose = new long[ROUNDS];
                long openCloseSink = 0;
                for (int r = 0; r < ROUNDS + 1; r++) {
                    long t0 = System.nanoTime();
                    for (int i = 0; i < BATCH; i++) {
                        openCloseSink += openClose(jf, e);
                    }
                    long dt = (System.nanoTime() - t0) / BATCH;
                    if (r > 0) { // round 0 warms up
                        openClose[r - 1] = dt;
                    }
                }
                long ocMed = median(openClose);
                System.out.println("open-close-sink " + ENTRY[s] + " " + openCloseSink);
                System.out.println("TIME " + ENTRY[s] + " open-close ns/stream " + ocMed);
                for (int c = 0; c < CONSUMER.length; c++) {
                    if (ENTRY[s].equals("big") && CONSUMER[c].equals("props")) {
                        continue; // random bytes are not a valid .properties file (unicode escapes)
                    }
                    long[] fullNs = new long[ROUNDS];
                    long[] bufNs = new long[ROUNDS];
                    long fullSum = 0;
                    long bufSum = 0;
                    for (int r = 0; r < ROUNDS + 1; r++) {
                        long t0 = System.nanoTime();
                        long acc = 0;
                        for (int i = 0; i < BATCH; i++) {
                            acc += full(jf, e, c);
                        }
                        long dFull = (System.nanoTime() - t0) / BATCH;
                        t0 = System.nanoTime();
                        long acc2 = 0;
                        for (int i = 0; i < BATCH; i++) {
                            acc2 += buffer(data[s], c);
                        }
                        long dBuf = (System.nanoTime() - t0) / BATCH;
                        if (r > 0) {
                            fullNs[r - 1] = dFull;
                            bufNs[r - 1] = dBuf;
                        } else {
                            fullSum = acc;
                            bufSum = acc2;
                        }
                        totalStreams += BATCH;
                    }
                    // The served-buffer consumer must see exactly what the stream gives.
                    System.out.println("checksum " + ENTRY[s] + " " + CONSUMER[c] + " "
                            + (fullSum == bufSum ? "same" : "DIFFERENT") + " " + (fullSum / BATCH));
                    long f = median(fullNs);
                    long b = median(bufNs);
                    System.out.println("TIME " + ENTRY[s] + " " + CONSUMER[c] + " full ns/stream " + f);
                    System.out.println("TIME " + ENTRY[s] + " " + CONSUMER[c] + " buffer ns/stream " + b);
                    System.out.println("TIME " + ENTRY[s] + " " + CONSUMER[c] + " ceiling ns/stream "
                            + Math.max(0L, f - ocMed - b));
                }
            }
        }
        System.out.println("streams-opened " + (totalStreams + ENTRY.length * (long) (ROUNDS + 1) * BATCH));
        for (File f : dir.listFiles()) {
            f.delete();
        }
        dir.delete();
    }
}
