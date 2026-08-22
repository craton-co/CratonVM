// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Replays GGUF's `readString` loop exactly: an 8-byte length read into a
 * reused heap buffer, then a VARIABLE-length read into a freshly wrapped
 * `byte[]`. A Llama-3.2 tokenizer array is ~128K of these.
 *
 * The variable length is the point. `FileChannel.read` into a heap buffer
 * borrows a temporary direct buffer sized to the request, and the JDK's
 * per-thread cache only hits when a cached buffer is at least as large as
 * the request. A fixed-size loop hides that; GGUF's does not.
 *
 *   GgufStringReadProbe <strings> [maxLen]
 */
public class GgufStringReadProbe {
    public static void main(String[] args) throws Exception {
        int strings = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int maxLen = args.length > 1 ? Integer.parseInt(args[1]) : 32;

        Path tmp = Files.createTempFile("ggufprobe", ".bin");
        try {
            byte[] filler = new byte[1 << 20];
            for (int i = 0; i < filler.length; i++) filler[i] = (byte) i;
            try (FileChannel out = FileChannel.open(tmp, StandardOpenOption.WRITE)) {
                for (int i = 0; i < 16; i++) out.write(ByteBuffer.wrap(filler));
            }

            long checksum = 0;
            long best = Long.MAX_VALUE;
            for (int pass = 0; pass < 3; pass++) {
                try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ)) {
                    ByteBuffer bb8 = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
                    long t0 = System.nanoTime();
                    for (int i = 0; i < strings; i++) {
                        bb8.clear();
                        ch.read(bb8);
                        // Deterministic pseudo-random length, like real tokens.
                        int len = 1 + ((i * 2654435761L) >>> 32 == 0 ? i : (int) (((i * 2654435761L) >>> 32) % maxLen));
                        if (len > maxLen) len = maxLen;
                        byte[] body = new byte[len];
                        ch.read(ByteBuffer.wrap(body));
                        checksum += body[0] + body[len - 1];
                        if (ch.position() > (1L << 20) * 15L) ch.position(0);
                    }
                    long dt = System.nanoTime() - t0;
                    if (dt < best) best = dt;
                }
            }
            System.out.println("GGUFSTR strings=" + strings + " max_len=" + maxLen
                    + " best_ms=" + (best / 1_000_000.0)
                    + " us_per_string=" + (best / 1000.0 / strings)
                    + " checksum=" + checksum);
        } finally {
            Files.deleteIfExists(tmp);
        }
    }
}
