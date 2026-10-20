// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.List;

/**
 * gcj j1/jt (2026-10-19): sub-word {@code Unsafe} accesses to a STATIC field of
 * each child-first copy of one class, through that copy's own
 * {@code staticFieldBase}. Page:
 * {@code docs/internal/gc/gci-i48-unsafe-byte-lane-static-sub-target-ignores-the-base-RETIRED-UNFIXED-20261018.md}.
 *
 * <p>{@code L} child-first loaders each define {@link Holder}; each copy's
 * static {@code long w} gets its own bit pattern. Per round, for every copy,
 * the probe reads bytes 1..7 ({@code getByte}), the short at +2 and the int at
 * +4 of {@code w}, writes byte 3 ({@code putByte}) and checks, through
 * reflection, that the copy's own {@code w} changed in exactly that byte and
 * every other copy's {@code w} is unchanged. Young churn between rounds.
 *
 * <p>Args: {@code [L rounds]} (defaults 8 40). Stdout (HotSpot, any collector):
 * {@code gcj1jt bytelane loaders=L rounds=R bad=0}. Diagnostics on stderr.
 */
public class Gcj1JtByteLaneStaticProbe {
    public static class Holder {
        public static long w;
    }

    static final String HOLDER = "Gcj1JtByteLaneStaticProbe$Holder";
    static volatile Object sink;

    static final class ChildFirst extends ClassLoader {
        private final byte[] bytes;

        ChildFirst(byte[] bytes) {
            super(Gcj1JtByteLaneStaticProbe.class.getClassLoader());
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(HOLDER)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    static byte[] holderBytes() throws Exception {
        try (InputStream in =
                Gcj1JtByteLaneStaticProbe.class.getResourceAsStream(HOLDER + ".class")) {
            if (in == null) {
                throw new IllegalStateException("no class bytes for " + HOLDER);
            }
            return in.readAllBytes();
        }
    }

    static long pattern(int l, int r) {
        long x = 0x0102030405060708L * (l + 1) + 0x1111111111111111L * r;
        return x ^ ((long) (l * 37 + r) << 24);
    }

    static int byteOf(long v, int k) {
        return (byte) (v >>> (8 * k));
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) throws Exception {
        int loaders = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        Field tf = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        tf.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) tf.get(null);
        byte[] bytes = holderBytes();
        List<ClassLoader> keep = new ArrayList<>();
        Field[] fields = new Field[loaders];
        Object[] bases = new Object[loaders];
        long[] offs = new long[loaders];
        for (int l = 0; l < loaders; l++) {
            ChildFirst cl = new ChildFirst(bytes);
            Class<?> c = cl.loadClass(HOLDER);
            if (c.getClassLoader() != cl) {
                throw new IllegalStateException("not child-first: " + c.getClassLoader());
            }
            keep.add(cl);
            fields[l] = c.getField("w");
            bases[l] = u.staticFieldBase(fields[l]);
            offs[l] = u.staticFieldOffset(fields[l]);
        }
        int bad = 0;
        long started = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 4000; i++) {
                sink = new byte[256 + (i & 1023)];
            }
            long[] want = new long[loaders];
            for (int l = 0; l < loaders; l++) {
                want[l] = pattern(l, r);
                fields[l].setLong(null, want[l]);
            }
            for (int l = 0; l < loaders; l++) {
                Object b = bases[l];
                long o = offs[l];
                for (int k = 1; k < 8; k++) {
                    int got = u.getByte(b, o + k);
                    if (got != byteOf(want[l], k)) {
                        bad++;
                        System.err.println("[gcj1jt] r=" + r + " l=" + l + " getByte+" + k
                                + " got=" + got + " want=" + byteOf(want[l], k));
                    }
                }
                short s = u.getShort(b, o + 2);
                if (s != (short) (want[l] >>> 16)) {
                    bad++;
                    System.err.println("[gcj1jt] r=" + r + " l=" + l + " getShort+2 got=" + s);
                }
                int iv = u.getInt(b, o + 4);
                if (iv != (int) (want[l] >>> 32)) {
                    bad++;
                    System.err.println("[gcj1jt] r=" + r + " l=" + l + " getInt+4 got=" + iv);
                }
                byte nb = (byte) (0x5A ^ l ^ r);
                u.putByte(b, o + 3, nb);
                want[l] = (want[l] & ~(0xFFL << 24)) | ((nb & 0xFFL) << 24);
                for (int m = 0; m < loaders; m++) {
                    long now = fields[m].getLong(null);
                    if (now != want[m]) {
                        bad++;
                        System.err.println("[gcj1jt] r=" + r + " after putByte on l=" + l
                                + " copy " + m + " holds " + Long.toHexString(now)
                                + " want " + Long.toHexString(want[m]));
                    }
                }
            }
        }
        System.out.println("gcj1jt bytelane loaders=" + loaders + " rounds=" + rounds + " bad=" + bad);
        System.err.println("[gcj1jt] keep=" + keep.size() + " ms="
                + (System.nanoTime() - started) / 1_000_000);
    }
}
