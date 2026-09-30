// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.Map;
import java.util.Objects;
import java.util.Random;
import java.util.TreeMap;

import org.h2.mvstore.Cursor;
import org.h2.mvstore.MVMap;
import org.h2.mvstore.MVStore;
import org.h2.store.fs.FileUtils;

/**
 * A TERMINATING driver for the op mix of {@code org.h2.test.store.TestRandomMapOps}.
 *
 * <p>Why this exists. {@code TestRandomMapOps.main} runs ten passes of a
 * hundred seeds each and never returns inside any cap a bisect can afford, so
 * every run of it has to be killed by {@code timeout} — and a killed VM never
 * reaches {@code vm-cli}'s exit summary, where {@code CRATONVM_GC_STATS=1}
 * prints {@code compaction_cycles}, {@code relocation_skip_reasons},
 * {@code stale_frame_words} and {@code unmapped_dupe_remap}. Measuring the
 * collector on that workload was therefore impossible: every engagement counter
 * read zero because the process was SIGTERMed before it could print, which is
 * indistinguishable from a counter that never fired.
 *
 * <p>This driver replicates the same operation sequence — same key range, same
 * fifteen-way op mix, same {@code keysPerPage(7).autoCommitDisabled()} store,
 * same {@code memFS:} backing so the "file" is heap-resident byte arrays —
 * with a bounded pass count, and RETURNS. A run either prints
 * {@code MVSTORE_RANDOM_OPS PASS} and exits 0, or prints the seed, the op index
 * and the exception and exits 1.
 *
 * <p>Usage: {@code MvStoreRandomOps [passes] [opsPerPass] [firstSeed]}.
 * Defaults are 40 passes of 3000 ops from seed 0, which is the same
 * {@code getSize(500, 3000)} the {@code big} configuration uses.
 */
public final class MvStoreRandomOps {

    private static final boolean LOG = Boolean.getBoolean("mvrandom.log");

    private final Random r = new Random();
    private int op;

    public static void main(String[] args) {
        int passes = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int opsPerPass = args.length > 1 ? Integer.parseInt(args[1]) : 3000;
        long seed = args.length > 2 ? Long.parseLong(args[2]) : 0L;

        MvStoreRandomOps t = new MvStoreRandomOps();
        long t0 = System.currentTimeMillis();
        for (int pass = 0; pass < passes; pass++) {
            String fileName = "memFS:mvStoreRandomOps";
            try {
                t.testOps(fileName, opsPerPass, seed);
            } catch (Exception | AssertionError ex) {
                System.out.println("MVSTORE_RANDOM_OPS FAIL pass:" + pass
                        + " seed:" + seed + " op:" + t.op + " " + ex);
                ex.printStackTrace(System.out);
                System.exit(1);
            } finally {
                FileUtils.delete(fileName);
            }
            seed = t.r.nextLong();
        }
        System.out.println("MVSTORE_RANDOM_OPS PASS passes=" + passes
                + " ops=" + opsPerPass
                + " millis=" + (System.currentTimeMillis() - t0));
    }

    private void testOps(String fileName, int loopCount, long seed) {
        r.setSeed(seed);
        op = 0;
        MVStore s = openStore(fileName);
        int keysPerPage = s.getKeysPerPage();
        int keyRange = 2000;
        MVMap<Integer, String> m = s.openMap("data");
        TreeMap<Integer, String> map = new TreeMap<>();
        int[] recentKeys = new int[2 * keysPerPage];
        for (; op < loopCount; op++) {
            int k = r.nextInt(3 * keyRange / 2);
            if (k >= keyRange) {
                k = recentKeys[k % recentKeys.length];
            } else {
                recentKeys[op % recentKeys.length] = k;
            }
            String v = k + "_Value_" + op;
            int type = r.nextInt(15);
            switch (type) {
            case 0:
            case 1:
            case 2:
            case 3:
                log(op, k, v, "m.put");
                m.put(k, v);
                map.put(k, v);
                break;
            case 4:
            case 5:
                log(op, k, v, "m.remove");
                m.remove(k);
                map.remove(k);
                break;
            case 6:
                log(op, k, v, "s.compact");
                s.compact(90, 1024);
                break;
            case 7:
                if (op % 64 == 0) {
                    log(op, k, v, "m.clear");
                    m.clear();
                    map.clear();
                }
                break;
            case 8:
                log(op, k, v, "s.commit");
                s.commit();
                break;
            case 9:
                log(op, k, v, "reopen");
                s.commit();
                s.close();
                s = openStore(fileName);
                m = s.openMap("data");
                break;
            case 10:
                log(op, k, v, "s.compactFile");
                s.commit();
                s.compactFile(0);
                break;
            case 11: {
                int rangeSize = r.nextInt(2 * keysPerPage);
                int step = r.nextBoolean() ? 1 : -1;
                for (int i = 0; i < rangeSize; i++) {
                    m.put(k, v);
                    map.put(k, v);
                    k += step;
                    v = k + "_Value_" + op;
                }
                break;
            }
            case 12: {
                int rangeSize = r.nextInt(2 * keysPerPage);
                int step = r.nextBoolean() ? 1 : -1;
                for (int i = 0; i < rangeSize; i++) {
                    m.remove(k);
                    map.remove(k);
                    k += step;
                }
                break;
            }
            default:
                ArrayList<Integer> keyList = new ArrayList<>(map.keySet());
                int index = Collections.binarySearch(keyList, k, null);
                int index2 = (int) m.getKeyIndex(k);
                assertEquals("getKeyIndex", index, index2);
                if (index >= 0) {
                    int k2 = m.getKey(index);
                    assertEquals("getKey", k2, k);
                }
                break;
            }
            assertEquals("get", map.get(k), m.get(k));
            assertEquals("ceilingKey", map.ceilingKey(k), m.ceilingKey(k));
            assertEquals("floorKey", map.floorKey(k), m.floorKey(k));
            assertEquals("higherKey", map.higherKey(k), m.higherKey(k));
            assertEquals("lowerKey", map.lowerKey(k), m.lowerKey(k));
            assertEquals("isEmpty", map.isEmpty(), m.isEmpty());
            assertEquals("size", map.size(), m.size());
            if (!map.isEmpty()) {
                assertEquals("firstKey", map.firstKey(), m.firstKey());
                assertEquals("lastKey", map.lastKey(), m.lastKey());
            }

            int from = r.nextBoolean() ? r.nextInt(keyRange)
                    : k + r.nextInt(2 * keysPerPage) - keysPerPage;
            int to = r.nextBoolean() ? r.nextInt(keyRange)
                    : from + r.nextInt(2 * keysPerPage) - keysPerPage;

            Cursor<Integer, String> cursor;
            Collection<Map.Entry<Integer, String>> entrySet;
            if (from <= to) {
                cursor = m.cursor(from, null, false);
                entrySet = map.tailMap(from).entrySet();
                assertCursor("(" + from + ", null)", entrySet, cursor);

                cursor = m.cursor(null, from, false);
                entrySet = map.headMap(from + 1).entrySet();
                assertCursor("(null, " + from + ")", entrySet, cursor);

                cursor = m.cursor(from, to, false);
                entrySet = map.subMap(from, to + 1).entrySet();
                assertCursor("(" + from + ", " + to + ")", entrySet, cursor);
            }

            if (from >= to) {
                cursor = m.cursor(from, null, true);
                entrySet = reverse(map.headMap(from + 1).entrySet());
                assertCursor("rev (" + from + ", null)", entrySet, cursor);

                cursor = m.cursor(null, from, true);
                entrySet = reverse(map.tailMap(from).entrySet());
                assertCursor("rev (null, " + from + ")", entrySet, cursor);

                cursor = m.cursor(from, to, true);
                entrySet = reverse(map.subMap(to, from + 1).entrySet());
                assertCursor("rev (" + from + ", " + to + ")", entrySet, cursor);
            }
        }
        s.close();
    }

    private static MVStore openStore(String fileName) {
        MVStore s = new MVStore.Builder().fileName(fileName)
                .keysPerPage(7).autoCommitDisabled().open();
        s.setRetentionTime(1000);
        return s;
    }

    private static <K, V> Collection<Map.Entry<K, V>> reverse(
            Collection<Map.Entry<K, V>> entrySet) {
        ArrayList<Map.Entry<K, V>> list = new ArrayList<>(entrySet);
        Collections.reverse(list);
        return list;
    }

    private static <K, V> void assertCursor(String msg,
            Iterable<Map.Entry<K, V>> entrySet, Cursor<K, V> cursor) {
        int cnt = 0;
        for (Map.Entry<K, V> entry : entrySet) {
            String message = msg + " " + cnt;
            if (!cursor.hasNext()) {
                throw new AssertionError(message + " cursor exhausted early");
            }
            assertEquals(message, entry.getKey(), cursor.next());
            assertEquals(message, entry.getKey(), cursor.getKey());
            assertEquals(message, entry.getValue(), cursor.getValue());
            ++cnt;
        }
        if (cursor.hasNext()) {
            throw new AssertionError(msg + " cursor has extra entries");
        }
    }

    private static void assertEquals(String msg, Object expected, Object actual) {
        if (!Objects.equals(expected, actual)) {
            throw new AssertionError(msg + " expected: " + expected + " actual: " + actual);
        }
    }

    private static void assertEquals(String msg, int expected, int actual) {
        if (expected != actual) {
            throw new AssertionError(msg + " expected: " + expected + " actual: " + actual);
        }
    }

    private static void log(int op, int k, String v, String what) {
        if (LOG) {
            System.out.println(op + " " + what + " k=" + k + " v=" + v);
        }
    }
}
