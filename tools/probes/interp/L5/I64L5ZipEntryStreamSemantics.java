// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// i64-L5 closure (2026-11-02): the observable behaviour of a `ZipFile` entry
// stream that any change touching it (the proposal's intrinsic, a future switch
// on `Inflater` / `ZipFile$ZipFileInputStream`) must keep. Self-contained and
// deterministic: the jar is written at run time. Compare stdout against
// `java` (JDK 21 and, where available, 25): the shapes below are the same in both.
//
//   * available() trajectory of a DEFLATED entry: size - bytesWritten (the
//     ZipFileInflaterInputStream override), through read / skip / EOF / close;
//     of a STORED entry: size - pos (ZipFileInputStream.available);
//   * skip() reads through read(): the returned counts and the position after;
//   * read(), read(b,0,0), read at EOF, bad-argument exceptions;
//   * close() idempotent; read / available / skip after close;
//   * zf.close() with an unclosed stream: the stream's next read;
//   * markSupported / mark / reset;
//   * many concurrently open streams closed in another order, then reopened
//     (the cached Inflater goes back to the ZipFile and is reused cleanly);
//   * the same entry read twice by two live streams interleaved.
//
// Run: javac -d . I64L5ZipEntryStreamSemantics.java
//      java -cp . I64L5ZipEntryStreamSemantics > ref.txt
//      cratonvm --real-jdk -cp . I64L5ZipEntryStreamSemantics | diff - ref.txt
//      cratonvm --compatible -cp . I64L5ZipEntryStreamSemantics | diff - ref.txt
//
// No HOTSPOT_EXPECTED block: the diff against `java` is the check.

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.zip.CRC32;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

public class I64L5ZipEntryStreamSemantics {
    static final int N = 5000;

    static byte[] text(int size) {
        byte[] b = new byte[size];
        for (int i = 0; i < size; i++) {
            b[i] = (byte) ('a' + (i * 7 + i / 13) % 26);
        }
        return b;
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

    static String thrown(String what, IOCall call) {
        try {
            Object r = call.run();
            return what + " -> " + r;
        } catch (IOException e) {
            return what + " -> " + e.getClass().getName() + ": " + e.getMessage();
        } catch (RuntimeException e) {
            return what + " -> " + e.getClass().getName() + ": " + e.getMessage();
        }
    }

    interface IOCall {
        Object run() throws IOException;
    }

    static void walk(String label, ZipFile zf, String entry) throws IOException {
        ZipEntry e = zf.getEntry(entry);
        byte[] expect = text(N);
        InputStream in = zf.getInputStream(e);
        System.out.println(label + " class " + in.getClass().getName());
        System.out.println(label + " available-start " + in.available());
        byte[] buf = new byte[100];
        int n = in.read(buf, 0, 100);
        System.out.println(label + " read100 " + n + " " + Arrays.equals(buf, Arrays.copyOf(expect, 100)));
        System.out.println(label + " available-after-100 " + in.available());
        System.out.println(label + " read-one " + in.read() + " (expect " + (expect[100] & 0xff) + ")");
        System.out.println(label + " available-after-101 " + in.available());
        System.out.println(label + " skip1000 " + in.skip(1000));
        System.out.println(label + " available-after-skip " + in.available());
        System.out.println(label + " skip0 " + in.skip(0));
        System.out.println(thrown(label + " skip-negative", () -> in.skip(-1)));
        System.out.println(label + " read-zero " + in.read(buf, 0, 0));
        System.out.println(thrown(label + " read-bad-off", () -> in.read(buf, -1, 10)));
        System.out.println(thrown(label + " read-bad-len", () -> in.read(buf, 0, 101)));
        System.out.println(thrown(label + " read-null", () -> in.read(null, 0, 1)));
        System.out.println(label + " mark-supported " + in.markSupported());
        System.out.println(thrown(label + " reset", () -> { in.reset(); return "ok"; }));
        byte[] rest = in.readAllBytes();
        System.out.println(label + " rest " + rest.length + " "
                + Arrays.equals(rest, Arrays.copyOfRange(expect, 1101, N)));
        System.out.println(label + " available-at-eof " + in.available());
        System.out.println(label + " read-at-eof " + in.read());
        System.out.println(label + " read-array-at-eof " + in.read(buf, 0, 10));
        System.out.println(label + " skip-at-eof " + in.skip(10));
        in.close();
        in.close();
        System.out.println(label + " closed-twice ok");
        System.out.println(label + " available-after-close " + thrown("", () -> in.available()));
        System.out.println(thrown(label + " read-after-close", () -> in.read()));
        System.out.println(thrown(label + " read-array-after-close", () -> in.read(buf, 0, 10)));
        System.out.println(thrown(label + " skip-after-close", () -> in.skip(1)));
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("i64l5sem").toFile();
        File jar = new File(dir, "semantics.jar");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            put(out, "deflated.bin", text(N), false);
            put(out, "stored.bin", text(N), true);
            put(out, "empty.bin", new byte[0], false);
        }
        try (JarFile zf = new JarFile(jar)) {
            walk("deflated", zf, "deflated.bin");
            walk("stored", zf, "stored.bin");

            // The empty entry.
            try (InputStream in = zf.getInputStream(zf.getEntry("empty.bin"))) {
                System.out.println("empty available " + in.available() + " read " + in.read());
            }

            // Skip past the end and an exact-size read.
            try (InputStream in = zf.getInputStream(zf.getEntry("deflated.bin"))) {
                System.out.println("skip-past-end " + in.skip(N + 1000) + " available " + in.available());
            }
            try (InputStream in = zf.getInputStream(zf.getEntry("deflated.bin"))) {
                byte[] all = new byte[N];
                int got = 0;
                int k;
                while (got < N && (k = in.read(all, got, N - got)) > 0) {
                    got += k;
                }
                System.out.println("exact-size-read " + got + " " + Arrays.equals(all, text(N))
                        + " next " + in.read());
            }

            // Two live streams of one entry, interleaved.
            try (InputStream a = zf.getInputStream(zf.getEntry("deflated.bin"));
                    InputStream b = zf.getInputStream(zf.getEntry("deflated.bin"))) {
                byte[] x = new byte[300];
                byte[] y = new byte[700];
                int xa = a.read(x, 0, 300);
                int yb = b.read(y, 0, 700);
                int xb = a.read(x, 0, 300);
                System.out.println("interleaved " + xa + " " + yb + " " + xb + " "
                        + a.available() + " " + b.available());
                byte[] ra = a.readAllBytes();
                byte[] rb = b.readAllBytes();
                System.out.println("interleaved-rest " + ra.length + " " + rb.length);
            }

            // Many live streams, closed in another order, then the cache is reused.
            List<InputStream> live = new ArrayList<>();
            for (int i = 0; i < 40; i++) {
                InputStream s = zf.getInputStream(zf.getEntry("deflated.bin"));
                s.read(new byte[10], 0, 10);
                live.add(s);
            }
            for (int i = 0; i < live.size(); i += 2) {
                live.get(i).close();
            }
            for (int i = 1; i < live.size(); i += 2) {
                live.get(i).close();
            }
            long sum = 0;
            for (int i = 0; i < 100; i++) {
                try (InputStream s = zf.getInputStream(zf.getEntry("deflated.bin"))) {
                    sum += s.readAllBytes().length;
                }
            }
            System.out.println("reuse-after-many " + sum);

            // An unclosed stream outlives zf.close().
            InputStream dangling = zf.getInputStream(zf.getEntry("deflated.bin"));
            dangling.read(new byte[10], 0, 10);
            zf.close();
            System.out.println(thrown("read-after-zipfile-close", () -> dangling.read()));
            System.out.println(thrown("read-array-after-zipfile-close", () -> dangling.read(new byte[4], 0, 4)));
            System.out.println("available-after-zipfile-close " + thrown("", () -> dangling.available()));
            System.out.println(thrown("get-input-stream-after-close", () -> zf.getInputStream(new ZipEntry("deflated.bin"))));
        }
        for (File f : dir.listFiles()) {
            f.delete();
        }
        dir.delete();
        System.out.println("done " + new String(new byte[] {'o', 'k'}, StandardCharsets.US_ASCII));
    }
}
