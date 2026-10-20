// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gch h2/jr (2026-10-08): the positive control of
 * {@code docs/internal/gc/gch-h2og-true-root-major-frees-the-old-referents-of-a-pooled-young-string-FIXED-20261012.md}.
 *
 * <p>Per round: a large text (2 MiB of LATIN1, so its {@code value} array is
 * placed in the old generation directly) is copied by {@code new String(big)},
 * which SHARES {@code big.value}; the young copy {@code s} is interned (pooled
 * itself: {@code intern()} returns its receiver on a miss) and stored only in
 * a large {@code Object[]} (old directly, so a card names {@code s}); the
 * holder is then dropped and {@code System.gc()} runs (a requested major: the
 * true-root seed on Generational). The young phase keeps {@code s} for the
 * dead holder's card; a true-root seed that does not include the pool's young
 * entries never marks the old {@code value}, which the sweep frees under a
 * live pool entry. Old garbage of the same size is then allocated (to reuse
 * the block), and the same text is interned again and compared.
 *
 * <p>Stdout (HotSpot 25 {@code -XX:+UseSerialGC -Xmx256m} prints it):
 * <pre>
 *   pooled-young: ok rounds=8
 * </pre>
 * A wrong answer prints {@code pooled-young: GARBLED round=R len=L} instead
 * (or the VM faults). Run on Generational with {@code CRATONVM_DBG_SWEEP_ZERO=1}
 * so a freed block reads as zeros; {@code CRATONVM_GC_WEAK_STRING_TABLE=0}
 * (the strong pool) is the control that must print {@code ok} on any build.
 */
public final class GchJr2PooledYoungStringProbe {
    static Object[] holder;
    static volatile Object sink;

    static String text(int round) {
        StringBuilder sb = new StringBuilder(2 << 20);
        for (int i = 0; i < (2 << 20) - 16; i++) {
            sb.append((char) ('a' + ((i * 7 + round) % 26)));
        }
        sb.append("-gch-jr2-").append(round);
        return sb.toString();
    }

    /** Builds, pools and parks the young copy; leaves no local behind. */
    static void park(int round) {
        String big = text(round);
        String s = new String(big);
        String pooled = s.intern();
        holder = new Object[1 << 20];
        holder[12345] = pooled;
        big = null;
        s = null;
        pooled = null;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        for (int r = 0; r < rounds; r++) {
            park(r);
            holder = null;
            System.gc();
            // Old garbage the size of the freed block, so a reused block reads
            // as other bytes.
            for (int k = 0; k < 4; k++) {
                byte[] junk = new byte[2 << 20];
                java.util.Arrays.fill(junk, (byte) 'Z');
                sink = junk;
            }
            sink = null;
            String want = text(r);
            String got = want.intern();
            if (got.length() != want.length() || !got.equals(want) || got.hashCode() != want.hashCode()) {
                System.out.println("pooled-young: GARBLED round=" + r + " len=" + got.length());
                return;
            }
        }
        System.out.println("pooled-young: ok rounds=" + rounds);
    }
}
