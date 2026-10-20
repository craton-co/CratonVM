// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// SBR wave 1, lane `bytes`: the timing micro for RETIRED_SHADOW_SBR_BYTES_TRIPLES.
// Retiring `jdk/internal/util/ByteArray` swaps a direct native for the JDK's own
// `byteArrayViewVarHandle` bytecode on the DataInput/DataOutput, RandomAccessFile
// and ObjectStream hot paths, so those are what this times. Run interleaved
// (base, lane, base, lane, ...) with and without --nojit; each line prints the
// best-of-N ns per operation and a checksum that must match across arms.
//
//   java SbrW1BytesBench [rounds]
import java.io.*;
import java.util.*;

public class SbrW1BytesBench {
    static final int N = 1 << 14;
    static final byte[] DATA = new byte[N * 8];
    static long sink;

    static long dataInput() throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(DATA));
        long acc = 0;
        for (int i = 0; i < N / 2; i++) acc += in.readInt();
        for (int i = 0; i < N / 4; i++) acc ^= in.readLong();
        for (int i = 0; i < N / 4; i++) acc += in.readShort() + in.readChar();
        return acc;
    }

    static long dataOutput() throws IOException {
        ByteArrayOutputStream bo = new ByteArrayOutputStream(N * 8);
        DataOutputStream d = new DataOutputStream(bo);
        for (int i = 0; i < N / 2; i++) d.writeInt(i * 31);
        for (int i = 0; i < N / 4; i++) d.writeLong(i * 0x9E3779B97F4A7C15L);
        for (int i = 0; i < N / 4; i++) d.writeDouble(i * 0.5);
        return bo.size();
    }

    static class Rec implements Serializable {
        private static final long serialVersionUID = 1L;
        int a; long b; double c; float d; short e; char f; boolean g;
        Rec(int i) { a = i; b = i * 7L; c = i / 3.0; d = i; e = (short) i; f = (char) i; g = (i & 1) == 0; }
    }

    static long serial() throws Exception {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
            for (int i = 0; i < 256; i++) oo.writeObject(new Rec(i));
            UUID u = new UUID(0x0123456789ABCDEFL, 0x0FEDCBA987654321L);
            for (int i = 0; i < 64; i++) oo.writeObject(u);
        }
        long acc = 0;
        try (ObjectInputStream oi = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray()))) {
            for (int i = 0; i < 256; i++) { Rec r = (Rec) oi.readObject(); acc += r.a + r.b + (long) r.c; }
            for (int i = 0; i < 64; i++) acc ^= ((UUID) oi.readObject()).getMostSignificantBits();
        }
        return acc;
    }

    static long uuid() {
        long acc = 0;
        for (int i = 0; i < 2048; i++) {
            UUID u = new UUID(i * 0x9E3779B97F4A7C15L, ~i);
            acc += UUID.fromString(u.toString()).getLeastSignificantBits();
        }
        return acc;
    }

    interface Op { long run() throws Exception; }

    static void time(String name, Op op, int ops, int rounds) throws Exception {
        long best = Long.MAX_VALUE, check = 0;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            check = op.run();
            long t = System.nanoTime() - t0;
            if (t < best) best = t;
            sink += check;
        }
        System.out.printf("%-11s best %9.1f ns/op  check %d%n", name, best / (double) ops, check);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 30;
        new Random(42).nextBytes(DATA);
        time("dataInput", SbrW1BytesBench::dataInput, N, rounds);
        time("dataOutput", SbrW1BytesBench::dataOutput, N, rounds);
        time("serial", SbrW1BytesBench::serial, 320, Math.max(3, rounds / 3));
        time("uuid", SbrW1BytesBench::uuid, 2048, rounds);
    }
}
