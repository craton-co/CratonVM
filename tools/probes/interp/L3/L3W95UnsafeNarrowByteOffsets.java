// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 95, lane L3
// (docs/internal/fixed-bugs/interpreter-i94-orch-dev-jit-round-20-regresses-four-interpreter-probes-FIXED-20261128.md):
// narrow Unsafe reads of a wider field, at the field's own offset and INSIDE
// it, under byte field offsets (`CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`, default
// ON since JIT round 20). Once offsets became `16 + 8 * slot`, the field view
// compared the raw offset with the field count and declined, and the
// int-width fallback answered the whole slot un-narrowed (getShort of a long
// gave its low int, getChar of a short -2, getByte of an int the int); an
// offset inside a field answered four bytes un-narrowed. The char CAS row
// compared 65535 with -1 again.
//
// HotSpot field offsets are byte offsets: `off(f) + k` is byte k of f
// (little-endian). Only bytes INSIDE each field are probed (no neighbour).
//
// Run (both modes, default and --nojit, must print the HotSpot lines):
//   javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out L3W95UnsafeNarrowByteOffsets.java
//   cratonvm --jdk-only   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W95UnsafeNarrowByteOffsets
//   cratonvm --compatible --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W95UnsafeNarrowByteOffsets
//   java                  --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W95UnsafeNarrowByteOffsets
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// short-of-long: 30600
// short-of-long+2: 21862
// byte-of-long+7: 17
// byte-of-long+1: 119
// char-of-long+6: 4386
// int-of-long+4: 11223344
// short-of-int+2: 4386
// byte-of-int: 68
// boolean-of-int+1: true
// char-of-short: 65534
// byte-of-short+1: -1
// short-of-char: -1
// volatile-short-of-char: -1
// volatile-byte-of-int: 68
// volatile-char-of-long+6: 4386
// short-unaligned-be-of-long+1: 30566
// cas-short-in-long+2: true 1122334401027788
// char-cas: true 32769 | 32769 65
// hot-narrow: 118081 2361620000
// HOTSPOT_EXPECTED_END

public class L3W95UnsafeNarrowByteOffsets {
    static class Box {
        long l = 0x1122334455667788L;
        long m = 0x1122334455667788L;
        int i = 0x11223344;
        short s = -2;
        char c = (char) 0xFFFF;
    }

    static final jdk.internal.misc.Unsafe U = jdk.internal.misc.Unsafe.getUnsafe();

    static long off(String name) throws Exception {
        return U.objectFieldOffset(Box.class.getDeclaredField(name));
    }

    public static void main(String[] args) throws Exception {
        Box o = new Box();
        long l = off("l"), m = off("m"), i = off("i"), s = off("s"), c = off("c");

        System.out.println("short-of-long: " + U.getShort(o, l));
        System.out.println("short-of-long+2: " + U.getShort(o, l + 2));
        System.out.println("byte-of-long+7: " + U.getByte(o, l + 7));
        System.out.println("byte-of-long+1: " + U.getByte(o, l + 1));
        System.out.println("char-of-long+6: " + (int) U.getChar(o, l + 6));
        System.out.println("int-of-long+4: " + Integer.toHexString(U.getInt(o, l + 4)));
        System.out.println("short-of-int+2: " + U.getShort(o, i + 2));
        System.out.println("byte-of-int: " + U.getByte(o, i));
        System.out.println("boolean-of-int+1: " + U.getBoolean(o, i + 1));
        System.out.println("char-of-short: " + (int) U.getChar(o, s));
        System.out.println("byte-of-short+1: " + U.getByte(o, s + 1));
        System.out.println("short-of-char: " + U.getShort(o, c));
        System.out.println("volatile-short-of-char: " + U.getShortVolatile(o, c));
        System.out.println("volatile-byte-of-int: " + U.getByteVolatile(o, i));
        System.out.println("volatile-char-of-long+6: " + (int) U.getCharVolatile(o, l + 6));
        System.out.println("short-unaligned-be-of-long+1: " + U.getShortUnaligned(o, l + 1, true));
        boolean ok = U.compareAndSetShort(o, m + 2, (short) 0x5566, (short) 0x0102);
        System.out.println("cas-short-in-long+2: " + ok + " " + Long.toHexString(o.m));
        StringBuilder sb = new StringBuilder();
        sb.append(U.compareAndSetChar(o, c, (char) 0xFFFF, (char) 0x8001)).append(' ').append((int) o.c);
        sb.append(" | ").append((int) U.compareAndExchangeChar(o, c, (char) 0x8001, 'A')).append(' ').append((int) o.c);
        System.out.println("char-cas: " + sb);

        // The same narrow reads from a hot (compiled) method.
        Box h = new Box();
        long hl = off("l"), hi = off("i"), hs = off("s");
        long sum = 0;
        for (int n = 0; n < 20000; n++) {
            sum += hot(h, hl, hi, hs);
        }
        System.out.println("hot-narrow: " + hot(h, hl, hi, hs) + " " + sum);
    }

    // 30600 + 21862 + 17 + 68 + 65534 = 118081 per call.
    static int hot(Box o, long l, long i, long s) {
        return U.getShort(o, l) + U.getShort(o, l + 2) + U.getByte(o, l + 7)
                + U.getByte(o, i) + U.getChar(o, s);
    }
}
