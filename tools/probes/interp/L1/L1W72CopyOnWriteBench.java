// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L1 -- bench probe for the `JdkCopyOnWrite`
// row (`CRATONVM_COMPAT_JDK_COPY_ON_WRITE`, default OFF): with it, over a real
// JDK `--compatible` runs the JDK 25 `CopyOnWriteArrayList` bodies (a volatile
// `array` read and an `aaload` for the readers; `synchronized (lock)` and an
// `Arrays.copyOf` for the mutators) instead of the `Bridge` stand-ins (a native
// door, a field resolve by name and pinned copies per call).
//
// A/B, one binary, `--compatible` over a real JDK:
//   cratonvm --compatible L1W72CopyOnWriteBench                                       (row off, default: stand-ins)
//   CRATONVM_COMPAT_JDK_COPY_ON_WRITE=1 cratonvm --compatible L1W72CopyOnWriteBench   (row on)
// and `cratonvm L1W72CopyOnWriteBench` (jdk-only) as the reference the row-on
// arm should equal. Interleave the arms and take medians of the per-run
// minimums (in-JVM timings swing ~3x on a noisy host). Add `--nojit` for the
// interpreter-only picture.
//
// Rows (stdout, min ns/op over REPS=7 rounds; the list holds 16 strings):
//   get          get(i & 15);
//   size         size();
//   contains     contains(<the 12th element>), an equals walk;
//   iterate16    a for-each over the 16 elements, ns per element;
//   set          set(i & 15, s) (a copy of 16 per call);
//   add-remove   add(s); remove(16) (two copies per pair), ns per pair.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum get=200000 size=3200000 contains=200000 iterate16=3200000 set=16 add-remove=16
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CopyOnWriteArrayList;

public class L1W72CopyOnWriteBench {
    static final int REPS = 7;
    static final int N = 200_000;
    static final int SMALL = 50_000;

    static long get(CopyOnWriteArrayList<String> l) {
        long n = 0;
        for (int i = 0; i < N; i++) {
            if (l.get(i & 15) != null) {
                n++;
            }
        }
        return n;
    }

    static long size(CopyOnWriteArrayList<String> l) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += l.size();
        }
        return sum;
    }

    static long contains(CopyOnWriteArrayList<String> l, String needle) {
        long n = 0;
        for (int i = 0; i < N; i++) {
            if (l.contains(needle)) {
                n++;
            }
        }
        return n;
    }

    static long iterate(CopyOnWriteArrayList<String> l) {
        long n = 0;
        for (int i = 0; i < N / 16; i++) {
            for (String s : l) {
                n += s.length();
            }
        }
        return n;
    }

    static long set(CopyOnWriteArrayList<String> l, String[] values) {
        for (int i = 0; i < SMALL; i++) {
            l.set(i & 15, values[i & 15]);
        }
        return l.size();
    }

    static long addRemove(CopyOnWriteArrayList<String> l) {
        for (int i = 0; i < SMALL; i++) {
            l.add("extra");
            l.remove(16);
        }
        return l.size();
    }

    public static void main(String[] args) throws Exception {
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>();
        String[] values = new String[16];
        for (int i = 0; i < 16; i++) {
            // Sixteen distinct strings of length 16 each.
            values[i] = String.format("element-%08d", i);
            l.add(values[i]);
        }
        String needle = new String(values[11]);
        String[] names = {"get", "size", "contains", "iterate16", "set", "add-remove"};
        long[] per = {N, N, N, N, SMALL, SMALL};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        for (int r = 0; r < REPS; r++) {
            long[] t = new long[names.length + 1];
            t[0] = System.nanoTime();
            sums[0] = get(l);
            t[1] = System.nanoTime();
            sums[1] = size(l);
            t[2] = System.nanoTime();
            sums[2] = contains(l, needle);
            t[3] = System.nanoTime();
            sums[3] = iterate(l);
            t[4] = System.nanoTime();
            sums[4] = set(l, values);
            t[5] = System.nanoTime();
            sums[5] = addRemove(l);
            t[6] = System.nanoTime();
            for (int i = 0; i < names.length; i++) {
                best[i] = Math.min(best[i], t[i + 1] - t[i]);
            }
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-12s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
        }
        System.out.println("checksum get=" + sums[0] + " size=" + sums[1]
                + " contains=" + sums[2] + " iterate16=" + sums[3]
                + " set=" + sums[4] + " add-remove=" + sums[5]);
        System.out.println("done");
    }
}
