// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L1 -- bench probe for the
// `JdkCopyOnWriteArraySet` row (`CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET`,
// default OFF): with it, over a real JDK `--compatible` runs the JDK 25
// `CopyOnWriteArraySet` bodies directly (a field read of `al` and the list's
// body) instead of entering a `HashSet`-surface `Bridge` that asks
// `cow_set_route` whether the receiver is a real set and then re-dispatches to
// the same body through `invoke_special_bytecode_only`.
//
// A/B, one binary, `--compatible` over a real JDK (four arms: the set row and
// the list row underneath it are independent):
//   cratonvm --compatible L1W74CopyOnWriteSetBench                                                (both off, default)
//   CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET=1 cratonvm --compatible L1W74CopyOnWriteSetBench        (set row)
//   CRATONVM_COMPAT_JDK_COPY_ON_WRITE=1 cratonvm --compatible L1W74CopyOnWriteSetBench            (list row)
//   CRATONVM_COMPAT_JDK_COPY_ON_WRITE=1 CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET=1 cratonvm --compatible L1W74CopyOnWriteSetBench
// and `cratonvm L1W74CopyOnWriteSetBench` (jdk-only) as the reference the
// both-on arm should equal. Interleave the arms and take medians of the
// per-run minimums (in-JVM timings swing ~3x on a noisy host). Add `--nojit`
// for the interpreter-only picture.
//
// Rows (stdout, min ns/op over REPS=7 rounds; the set holds 16 strings;
// `contains` and `add-present` run 50000 calls a round, the others as noted --
// row off they reach the list's `Bridge` stand-ins, ~9 us a call on the host):
//   size          size();
//   contains      contains(<the 12th element>), an equals walk;
//   add-present   add(<the 5th element>), present: false, no copy;
//   iterate16     a for-each over the 16 elements, ns per element;
//   add-remove    add(s); remove(s) (two copies per pair), ns per pair.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum size=3200000 contains=50000 add-present=0 iterate16=3200000 add-remove=16
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CopyOnWriteArraySet;

public class L1W74CopyOnWriteSetBench {
    static final int REPS = 7;
    static final int N = 200_000;
    static final int SMALL = 50_000;

    static long size(CopyOnWriteArraySet<String> s) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += s.size();
        }
        return sum;
    }

    static long contains(CopyOnWriteArraySet<String> s, String needle) {
        long n = 0;
        for (int i = 0; i < SMALL; i++) {
            if (s.contains(needle)) {
                n++;
            }
        }
        return n;
    }

    static long addPresent(CopyOnWriteArraySet<String> s, String present) {
        long n = 0;
        for (int i = 0; i < SMALL; i++) {
            if (s.add(present)) {
                n++;
            }
        }
        return n;
    }

    static long iterate(CopyOnWriteArraySet<String> s) {
        long n = 0;
        for (int i = 0; i < N / 16; i++) {
            for (String e : s) {
                n += e.length();
            }
        }
        return n;
    }

    static long addRemove(CopyOnWriteArraySet<String> s) {
        for (int i = 0; i < SMALL; i++) {
            s.add("extra");
            s.remove("extra");
        }
        return s.size();
    }

    public static void main(String[] args) throws Exception {
        CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
        String[] values = new String[16];
        for (int i = 0; i < 16; i++) {
            // Sixteen distinct strings of length 16 each.
            values[i] = String.format("element-%08d", i);
            s.add(values[i]);
        }
        String needle = new String(values[11]);
        String present = new String(values[4]);
        String[] names = {"size", "contains", "add-present", "iterate16", "add-remove"};
        long[] per = {N, SMALL, SMALL, N, SMALL};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        for (int r = 0; r < REPS; r++) {
            long[] t = new long[names.length + 1];
            t[0] = System.nanoTime();
            sums[0] = size(s);
            t[1] = System.nanoTime();
            sums[1] = contains(s, needle);
            t[2] = System.nanoTime();
            sums[2] = addPresent(s, present);
            t[3] = System.nanoTime();
            sums[3] = iterate(s);
            t[4] = System.nanoTime();
            sums[4] = addRemove(s);
            t[5] = System.nanoTime();
            for (int i = 0; i < names.length; i++) {
                best[i] = Math.min(best[i], t[i + 1] - t[i]);
            }
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-12s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
        }
        System.out.println("checksum size=" + sums[0] + " contains=" + sums[1]
                + " add-present=" + sums[2] + " iterate16=" + sums[3]
                + " add-remove=" + sums[4]);
        System.out.println("done");
    }
}
