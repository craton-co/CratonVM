// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Prices `FileChannel.read(HeapByteBuffer)` — the shape GGUF's metadata
 * reader uses, and the one a stack sample caught CratonVM sitting in while
 * loading a 2.4GB model that HotSpot loads in about a second.
 *
 * The shape matters. A `FileChannel.read` into a HEAP buffer cannot DMA
 * straight into the Java array, so the JDK borrows a temporary DIRECT
 * buffer from `sun.nio.ch.Util.getTemporaryDirectBuffer`, reads into that,
 * and copies out. That helper keeps a small per-thread cache; when the
 * cache works the direct buffer is allocated once and reused forever, and
 * when it does not, every read allocates a fresh direct buffer and
 * registers a Cleaner for it. The two cost worlds are orders of magnitude
 * apart, and nothing in the application source distinguishes them — which
 * is exactly why this is worth a standalone probe rather than an argument.
 *
 * GGUF's `readString` does two reads per string (8-byte length, then the
 * bytes), and a Llama-3.2 tokenizer array holds ~128K strings, so the real
 * workload is a few hundred thousand of these.
 *
 *   FileChannelHeapReadProbe <reads> [bytesPerRead]
 *
 * Prints one machine-readable line. Compare CratonVM against HotSpot on
 * the same file; the interesting number is ns/read, not the total.
 */
public class FileChannelHeapReadProbe {

    public static void main(String[] args) throws IOException {
        int reads = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int size = args.length > 1 ? Integer.parseInt(args[1]) : 24;

        Path tmp = Files.createTempFile("fcprobe", ".bin");
        try {
            // Enough bytes that the reads never hit EOF and never re-read
            // the same page over and over.
            byte[] filler = new byte[1 << 20];
            for (int i = 0; i < filler.length; i++) {
                filler[i] = (byte) i;
            }
            try (FileChannel out = FileChannel.open(tmp, StandardOpenOption.WRITE)) {
                for (int i = 0; i < 64; i++) {
                    out.write(ByteBuffer.wrap(filler));
                }
            }

            long checksum = 0;
            long best = Long.MAX_VALUE;
            // Three passes; report the best, so a GC pause in one pass does
            // not become the headline.
            for (int pass = 0; pass < 3; pass++) {
                try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ)) {
                    // The two buffer shapes GGUF.readString uses: a small
                    // fixed one for the length prefix, and a fresh
                    // array-wrapped one for each string body.
                    ByteBuffer len8 = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
                    long t0 = System.nanoTime();
                    for (int i = 0; i < reads; i++) {
                        len8.clear();
                        ch.read(len8);
                        byte[] body = new byte[size];
                        ch.read(ByteBuffer.wrap(body));
                        checksum += body[0] + body[size - 1] + len8.get(0);
                        if (ch.position() > (1 << 20) * 60L) {
                            ch.position(0);
                        }
                    }
                    long dt = System.nanoTime() - t0;
                    if (dt < best) {
                        best = dt;
                    }
                }
            }

            System.out.println("FCHEAPREAD reads=" + reads
                    + " bytes_per_read=" + size
                    + " best_ms=" + (best / 1_000_000.0)
                    + " ns_per_read_pair=" + (best / (double) reads)
                    + " checksum=" + checksum);
        } finally {
            Files.deleteIfExists(tmp);
        }
    }
}
