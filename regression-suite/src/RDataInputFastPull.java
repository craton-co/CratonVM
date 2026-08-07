// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.BufferedInputStream;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.util.Arrays;

/**
 * Regression: `DataInputStream`'s typed reads must observe exactly the same
 * stream position as everything else reading the same underlying stream.
 *
 * CratonVM implements the typed reads natively, and `dis_read_exact` has a
 * fast path (`dis_fast_pull`, native-io/src/lib.rs) that copies bytes straight
 * out of a `BufferedInputStream`'s / `ByteArrayInputStream`'s own `buf` and
 * advances `pos` in Rust, instead of allocating a scratch array and re-entering
 * the VM to run the wrapped stream's `read(byte[],int,int)` bytecode. That
 * removed ~2.6x from Tomcat's webapp annotation scan, and every way it can be
 * wrong is SILENT:
 *
 *   * advancing `pos` by the wrong amount shifts every later read, so the
 *     values stay well-formed and are simply wrong;
 *   * failing to advance it at all re-delivers the same bytes;
 *   * bypassing an overridden `read` gives raw bytes where the override's
 *     transformed ones are due;
 *   * mishandling the buffer boundary silently truncates or duplicates.
 *
 * None of those throw, so the vectors below are built so that each mistake
 * changes a PRINTED value. The suite diffs this output against HotSpot, which
 * has no such fast path, so any divergence in position accounting shows up.
 *
 * The buffer sizes are deliberately small and coprime with the record sizes so
 * that multi-byte reads straddle the buffer boundary constantly - that is the
 * case where the fast path can only satisfy PART of the request and has to hand
 * the remainder to the generic path.
 */
public class RDataInputFastPull {

    /** A BufferedInputStream subclass that complements every byte it yields. */
    static final class FlipStream extends BufferedInputStream {
        FlipStream(InputStream in, int size) {
            super(in, size);
        }

        @Override
        public synchronized int read() throws IOException {
            int b = super.read();
            return b < 0 ? b : (~b) & 0xFF;
        }

        @Override
        public synchronized int read(byte[] b, int off, int len) throws IOException {
            int n = super.read(b, off, len);
            for (int i = 0; i < n; i++) {
                b[off + i] = (byte) ~b[off + i];
            }
            return n;
        }
    }

    static byte[] record() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        for (int i = 0; i < 40; i++) {
            out.writeByte(i - 20);
            out.writeShort(i * 1013 - 30000);
            out.writeInt(i * 0x01020304 - 7);
            out.writeLong(0x0102030405060708L * (i + 1));
            out.writeFloat(i * -1.5f);
            out.writeDouble(i * 0.125d - 3);
            out.writeUTF("entry-" + i);
        }
        out.flush();
        return bytes.toByteArray();
    }

    static long typedPass(DataInputStream in) throws IOException {
        long acc = 17;
        for (int i = 0; i < 40; i++) {
            acc = acc * 31 + in.readByte();
            acc = acc * 31 + in.readShort();
            acc = acc * 31 + in.readInt();
            acc = acc * 31 + in.readLong();
            acc = acc * 31 + Float.floatToRawIntBits(in.readFloat());
            acc = acc * 31 + Double.doubleToRawLongBits(in.readDouble());
            acc = acc * 31 + in.readUTF().hashCode();
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        byte[] data = record();
        System.out.println("data.length=" + data.length);

        // 1. Typed round trip through buffers of many sizes. Every size that is
        //    not a multiple of a record field straddles the boundary somewhere,
        //    which is the partial-fast-pull case.
        for (int size : new int[] {1, 2, 3, 5, 7, 13, 64, 97, 1024, 8192}) {
            DataInputStream in =
                    new DataInputStream(new BufferedInputStream(new ByteArrayInputStream(data), size));
            System.out.println("typed buf=" + size + " acc=" + typedPass(in));
        }

        // 2. No BufferedInputStream at all - DataInputStream straight onto a
        //    ByteArrayInputStream, the other class the fast path accepts.
        System.out.println(
                "typed bais acc=" + typedPass(new DataInputStream(new ByteArrayInputStream(data))));

        // 3. Position coherence: the SAME BufferedInputStream is read both
        //    through the DataInputStream and directly. If a typed read advances
        //    `pos` by the wrong amount, the direct read below sees the wrong
        //    byte - and vice versa.
        BufferedInputStream shared = new BufferedInputStream(new ByteArrayInputStream(data), 17);
        DataInputStream typed = new DataInputStream(shared);
        StringBuilder mixed = new StringBuilder();
        for (int i = 0; i < 25; i++) {
            mixed.append(typed.readByte()).append(',');
            mixed.append(shared.read()).append(',');
            mixed.append(typed.readShort()).append(',');
            byte[] three = new byte[3];
            int got = shared.read(three, 0, 3);
            mixed.append(got).append(':').append(Arrays.toString(three)).append(';');
            mixed.append(typed.readInt()).append('|');
        }
        System.out.println("mixed=" + mixed.toString().hashCode());

        // 4. mark/reset must survive typed reads. Serving from the buffer never
        //    touches markpos, so a reset after a typed read has to replay the
        //    exact same bytes.
        BufferedInputStream marked = new BufferedInputStream(new ByteArrayInputStream(data), 23);
        DataInputStream mdis = new DataInputStream(marked);
        mdis.readInt();
        marked.mark(512);
        long first = 0;
        for (int i = 0; i < 8; i++) {
            first = first * 31 + mdis.readLong();
        }
        marked.reset();
        long second = 0;
        for (int i = 0; i < 8; i++) {
            second = second * 31 + mdis.readLong();
        }
        System.out.println("markreset equal=" + (first == second) + " v=" + first);

        // 5. An overridden read() must be honoured. FlipStream is NOT exactly
        //    java.io.BufferedInputStream, so the fast path must decline and let
        //    the override run; if it read the raw buffer instead, every value
        //    below changes.
        DataInputStream flipped =
                new DataInputStream(new FlipStream(new ByteArrayInputStream(data), 11));
        long facc = 0;
        for (int i = 0; i < 60; i++) {
            facc = facc * 31 + flipped.readInt();
        }
        System.out.println("flipped=" + facc);

        // 6. readUTF with a payload far larger than the buffer, so one call
        //    spans many refills.
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 4000; i++) {
            big.append((char) ('a' + (i % 26)));
        }
        ByteArrayOutputStream ub = new ByteArrayOutputStream();
        DataOutputStream uo = new DataOutputStream(ub);
        uo.writeUTF(big.toString());
        uo.writeInt(0x5A5A5A5A);
        uo.flush();
        DataInputStream udis =
                new DataInputStream(new BufferedInputStream(new ByteArrayInputStream(ub.toByteArray()), 37));
        String back = udis.readUTF();
        System.out.println(
                "utf ok=" + back.equals(big.toString()) + " len=" + back.length()
                        + " tail=" + Integer.toHexString(udis.readInt()));

        // 7. skipBytes has to move the same position the typed reads use.
        DataInputStream sdis =
                new DataInputStream(new BufferedInputStream(new ByteArrayInputStream(data), 29));
        sdis.readByte();
        int skipped = sdis.skipBytes(50);
        System.out.println("skipped=" + skipped + " next=" + sdis.readInt());

        // 8. EOF must still be EOF - a fast path that over-reports available
        //    bytes would return zeros instead of throwing.
        DataInputStream edis =
                new DataInputStream(new BufferedInputStream(new ByteArrayInputStream(new byte[6]), 4));
        edis.readInt();
        String eof;
        try {
            edis.readLong();
            eof = "no-throw";
        } catch (EOFException e) {
            eof = "EOFException";
        }
        System.out.println("eof=" + eof);

        System.out.println("PASS RDataInputFastPull");
    }
}
