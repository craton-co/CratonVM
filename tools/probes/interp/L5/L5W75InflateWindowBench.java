// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 75, lane L5 -- small reads over a large inflater
// input buffer (`docs/internal/fixed-bugs/interpreter-i74-L5-proposal-inflate-without-copying-the-whole-input-20261107-DONE-20261108.md`).
//
// `Inflater.inflateBytesBytes` copies ALL the input `setInput` handed over and
// zlib has not consumed yet on every call, so a call that asks for a few
// bytes out of a 64 KiB `GZIPInputStream` buffer copies up to 64 KiB.
// Switch (opt-in, default OFF; the same bytes and counts either way):
//   CRATONVM_INFLATER_INPUT_WINDOW=1  zlib gets the input in windows of
//       max(output asked, 512) bytes, the next window only while a step
//       consumed its whole window.
//
// One 1 MiB text payload (deterministic), gzipped once at start-up (outside
// the timing). Rows, each reported per read call:
//   gzip-read-int      new DataInputStream(new GZIPInputStream(in, 65536))
//                      .readInt() to the end (262 144 calls of 4 bytes): the
//                      row the switch is for.
//   gzip-read-byte     GZIPInputStream(in, 65536).read() for the first
//                      64 KiB (65 536 one-byte calls).
//   gzip-read-8k       readNBytes(8192) windows to the end (128 calls): must
//                      not get slower.
//   gzip-read-all      readAllBytes() (one call): a control.
//
// stdout: per row one deterministic "<row> <counts>" line (no "ns/") and one
// timing line "<row> <n> ns/op (median of rounds 1..ROUNDS-1, per read call)".
// One row only: `L5W75InflateWindowBench <row>`.
//
// A/B ($B the cratonvm binary, $J the JDK 25 bin directory; the same binary,
// interleaved, three repetitions, medians; with and without --nojit):
//   $J/javac -d out L5W75InflateWindowBench.java
//   $B --java-home $J/.. -cp out L5W75InflateWindowBench                                        # off
//   env CRATONVM_INFLATER_INPUT_WINDOW=1 $B --java-home $J/.. -cp out L5W75InflateWindowBench   # on
//   $J/java -cp out L5W75InflateWindowBench                                                     # HotSpot
// Expected: gzip-read-int and gzip-read-byte drop by most of a 32 KiB average
// copy per call; gzip-read-8k and gzip-read-all do not move.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// payload bytes=1048576
// gzip-read-int calls=262144 checksum=343874973952667
// gzip-read-byte calls=65536 checksum=5173056
// gzip-read-8k calls=128 bytes=1048576
// gzip-read-all calls=1 bytes=1048576
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.zip.GZIPInputStream;
import java.util.zip.GZIPOutputStream;

public class L5W75InflateWindowBench {
    static final int PAYLOAD = 1 << 20;
    static final int BUFFER = 65536;
    static final int ROUNDS = 5;

    static byte[] PLAIN;
    static byte[] GZIPPED;

    static byte[] payload() {
        StringBuilder text = new StringBuilder(PAYLOAD + 128);
        String[] words = {"zip", "inflate", "window", "class", "loader", "entry", "buffer", "stream"};
        for (int i = 0; text.length() < PAYLOAD; i++) {
            text.append("line ").append(i).append(' ')
                .append(words[i % words.length]).append(' ')
                .append(words[(i * 7 + 3) % words.length]).append(' ')
                .append(i * 31 % 1000).append('\n');
        }
        return Arrays.copyOf(text.toString().getBytes(StandardCharsets.US_ASCII), PAYLOAD);
    }

    static byte[] gzip(byte[] plain) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        try (GZIPOutputStream gz = new GZIPOutputStream(out)) {
            gz.write(plain);
        }
        return out.toByteArray();
    }

    static InputStream open() throws IOException {
        return new GZIPInputStream(new ByteArrayInputStream(GZIPPED), BUFFER);
    }

    interface Round {
        long[] run() throws IOException;
    }

    /** `result[0]` is the number of read calls the round made (the per-op divisor). */
    static void row(String label, String[] counts, Round round) throws IOException {
        long[] perCall = new long[ROUNDS];
        long[] result = null;
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            result = round.run();
            perCall[r] = (System.nanoTime() - t0) / Math.max(1, result[0]);
        }
        long[] later = Arrays.copyOfRange(perCall, 1, ROUNDS);
        Arrays.sort(later);
        StringBuilder line = new StringBuilder(label);
        for (int k = 0; k < counts.length; k++) {
            line.append(' ').append(counts[k]).append('=').append(result[k]);
        }
        System.out.println(line);
        System.out.println(label + " " + later[later.length / 2]
                + " ns/op (median of rounds 1.." + (ROUNDS - 1) + ", per read call)");
    }

    public static void main(String[] args) throws IOException {
        String only = args.length > 0 ? args[0] : null;
        PLAIN = payload();
        GZIPPED = gzip(PLAIN);
        System.out.println("payload bytes=" + PLAIN.length);

        if (only == null || only.equals("gzip-read-int")) {
            row("gzip-read-int", new String[] {"calls", "checksum"}, () -> {
                long calls = 0;
                long sum = 0;
                try (DataInputStream in = new DataInputStream(open())) {
                    while (true) {
                        int v;
                        try {
                            v = in.readInt();
                        } catch (EOFException e) {
                            break;
                        }
                        calls++;
                        sum += v & 0xFFFFFFFFL;
                    }
                }
                return new long[] {calls, sum};
            });
        }
        if (only == null || only.equals("gzip-read-byte")) {
            row("gzip-read-byte", new String[] {"calls", "checksum"}, () -> {
                long calls = 0;
                long sum = 0;
                try (InputStream in = open()) {
                    for (int i = 0; i < BUFFER; i++) {
                        int b = in.read();
                        if (b < 0) {
                            break;
                        }
                        calls++;
                        sum += b;
                    }
                }
                return new long[] {calls, sum};
            });
        }
        if (only == null || only.equals("gzip-read-8k")) {
            row("gzip-read-8k", new String[] {"calls", "bytes"}, () -> {
                long calls = 0;
                long bytes = 0;
                try (InputStream in = open()) {
                    while (true) {
                        byte[] chunk = in.readNBytes(8192);
                        if (chunk.length == 0) {
                            break;
                        }
                        calls++;
                        bytes += chunk.length;
                    }
                }
                return new long[] {calls, bytes};
            });
        }
        if (only == null || only.equals("gzip-read-all")) {
            row("gzip-read-all", new String[] {"calls", "bytes"}, () -> {
                try (InputStream in = open()) {
                    return new long[] {1, in.readAllBytes().length};
                }
            });
        }
    }
}
