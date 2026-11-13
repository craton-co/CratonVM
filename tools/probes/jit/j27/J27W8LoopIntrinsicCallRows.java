// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;
import java.util.zip.CRC32C;

/**
 * JIT j27 wave 8, lane W8REV: the loop-shaped single-pass intrinsic sites
 * (Arrays.fill integral overloads, Arrays.sort integral whole-array overloads,
 * CRC32C.update(I)V / ([BII)V) that the optimizing tier keeps as an ordinary
 * call under CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL=1 (default OFF; jit/src/
 * ir_loop_intrinsic_call.rs) instead of refusing the whole enclosing method.
 *
 * Each row method holds one such site plus arithmetic, is called WARM times
 * (so it compiles), and every answer is checked against an independent Java
 * reference loop. Run unset and with the switch =1, both modes; stdout must
 * equal HotSpot 25 byte for byte:
 *
 *   fact fill-mismatches 0
 *   fact sort-mismatches 0
 *   fact crc32c-check e3069283
 *   fact crc32c-mismatches 0
 *   fact fill-null java.lang.NullPointerException
 *   fact sort-null java.lang.NullPointerException
 *   fact crc32c-null java.lang.NullPointerException
 *   fact crc32c-range java.lang.ArrayIndexOutOfBoundsException
 *   fact warm true
 *
 * Engagement (switch =1): CRATONVM_DBG_IR_COMPILES=1 names fillRow / sortRow /
 * crcRow compiled by the optimizing tier; unset, CRATONVM_DBG_JITC=1 prints
 * "is a loop-shaped or memory call-site intrinsic" for them instead.
 * Timing (stderr, "row <name> <ns/op>") only with -Dj27.time=true.
 */
public final class J27W8LoopIntrinsicCallRows {
    static final int WARM = 40_000;

    static int seed = 12345;

    static int next() {
        seed = seed * 1103515245 + 12345;
        return seed >>> 1;
    }

    // ---- Arrays.fill: one call per overload, checked against a loop ----
    static int fillRow(int v, byte[] b, boolean[] z, char[] c, short[] s, int[] i, long[] j) {
        Arrays.fill(b, (byte) v);
        Arrays.fill(z, (v & 1) != 0);
        Arrays.fill(c, (char) (v * 3));
        Arrays.fill(s, (short) (v - 7));
        Arrays.fill(i, v * 31 + 1);
        Arrays.fill(j, ((long) v << 33) ^ v);
        return b.length + c.length + i.length + v;
    }

    static int fillCheck(int v, byte[] b, boolean[] z, char[] c, short[] s, int[] i, long[] j) {
        int bad = 0;
        for (byte x : b) bad += x == (byte) v ? 0 : 1;
        for (boolean x : z) bad += x == ((v & 1) != 0) ? 0 : 1;
        for (char x : c) bad += x == (char) (v * 3) ? 0 : 1;
        for (short x : s) bad += x == (short) (v - 7) ? 0 : 1;
        for (int x : i) bad += x == v * 31 + 1 ? 0 : 1;
        for (long x : j) bad += x == (((long) v << 33) ^ v) ? 0 : 1;
        return bad;
    }

    // ---- Arrays.sort: integral whole-array overloads ----
    static int sortRow(int[] i, long[] j, char[] c, short[] s, byte[] b) {
        Arrays.sort(i);
        Arrays.sort(j);
        Arrays.sort(c);
        Arrays.sort(s);
        Arrays.sort(b);
        return i.length + j.length;
    }

    static int sortCheck(int[] i, long[] j, char[] c, short[] s, byte[] b) {
        int bad = 0;
        for (int k = 1; k < i.length; k++) bad += i[k - 1] <= i[k] ? 0 : 1;
        for (int k = 1; k < j.length; k++) bad += j[k - 1] <= j[k] ? 0 : 1;
        for (int k = 1; k < c.length; k++) bad += c[k - 1] <= c[k] ? 0 : 1;
        for (int k = 1; k < s.length; k++) bad += s[k - 1] <= s[k] ? 0 : 1;
        for (int k = 1; k < b.length; k++) bad += b[k - 1] <= b[k] ? 0 : 1;
        return bad;
    }

    // ---- CRC32C.update(I)V and ([BII)V ----
    static long crcRow(byte[] data, int off, int len) {
        CRC32C crc = new CRC32C();
        crc.update(data, off, len);
        crc.update(data[0]);
        return crc.getValue();
    }

    /** Bitwise reference CRC-32C (Castagnoli, reflected 0x82F63B78). */
    static long crcRef(byte[] data, int off, int len) {
        int crc = 0xFFFFFFFF;
        for (int k = off; k < off + len; k++) crc = crcByte(crc, data[k]);
        crc = crcByte(crc, data[0]);
        return (~crc) & 0xFFFFFFFFL;
    }

    static int crcByte(int crc, byte b) {
        crc ^= b & 0xFF;
        for (int bit = 0; bit < 8; bit++) {
            crc = (crc >>> 1) ^ (0x82F63B78 & -(crc & 1));
        }
        return crc;
    }

    static String thrown(Runnable r) {
        try {
            r.run();
            return "no-throw";
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        int fillBad = 0;
        byte[] b = new byte[37];
        boolean[] z = new boolean[5];
        char[] c = new char[19];
        short[] s = new short[3];
        int[] i = new int[64];
        long[] j = new long[9];
        long t0 = System.nanoTime();
        for (int r = 0; r < WARM; r++) {
            fillRow(r, b, z, c, s, i, j);
            if ((r & 1023) == 0 || r == WARM - 1) fillBad += fillCheck(r, b, z, c, s, i, j);
        }
        long t1 = System.nanoTime();
        System.out.println("fact fill-mismatches " + fillBad);

        int sortBad = 0;
        for (int r = 0; r < WARM / 8; r++) {
            int n = 1 + (r % 70);
            int[] si = new int[n];
            long[] sj = new long[n];
            char[] sc = new char[n];
            short[] ss = new short[n];
            byte[] sb = new byte[n];
            for (int k = 0; k < n; k++) {
                int x = next();
                si[k] = x - 0x40000000;
                sj[k] = ((long) x << 20) - x;
                sc[k] = (char) x;
                ss[k] = (short) x;
                sb[k] = (byte) x;
            }
            sortRow(si, sj, sc, ss, sb);
            sortBad += sortCheck(si, sj, sc, ss, sb);
        }
        long t2 = System.nanoTime();
        System.out.println("fact sort-mismatches " + sortBad);

        byte[] check = "123456789".getBytes();
        CRC32C plain = new CRC32C();
        plain.update(check, 0, check.length);
        System.out.println("fact crc32c-check " + Long.toHexString(plain.getValue()));
        int crcBad = 0;
        byte[] data = new byte[96];
        for (int k = 0; k < data.length; k++) data[k] = (byte) next();
        for (int r = 0; r < WARM; r++) {
            int off = r % 17;
            int len = r % 61;
            long got = crcRow(data, off, len);
            if ((r & 255) == 0 || r == WARM - 1) crcBad += got == crcRef(data, off, len) ? 0 : 1;
        }
        long t3 = System.nanoTime();
        System.out.println("fact crc32c-mismatches " + crcBad);

        System.out.println("fact fill-null "
                + thrown(() -> fillRow(1, null, z, c, s, i, j)));
        System.out.println("fact sort-null "
                + thrown(() -> sortRow(null, new long[1], new char[1], new short[1], new byte[1])));
        System.out.println("fact crc32c-null " + thrown(() -> crcRow(null, 0, 0)));
        System.out.println("fact crc32c-range " + thrown(() -> crcRow(new byte[4], 2, 10)));
        System.out.println("fact warm true");
        if (time) {
            System.err.println("row fill " + (t1 - t0) / WARM);
            System.err.println("row sort " + (t2 - t1) / (WARM / 8));
            System.err.println("row crc32c " + (t3 - t2) / WARM);
        }
    }
}
