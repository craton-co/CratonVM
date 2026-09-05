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

/**
 * A faithful standalone replica of `org.h2.test.store.TestRandomMapOps.testOps`
 * at its first, fixed seed.
 *
 * # Why this exists
 *
 * The real test's only progress output is `Done pass #N`, and one pass is 100
 * calls of `testOps` at 3000 ops each -- 300 000 ops, which HotSpot does in
 * 11 s and the CratonVM interpreter does not finish in an hour. Every arm that
 * forces interpretation (`--nojit`, `CRATONVM_JIT_DENY=org/h2`) therefore ends
 * as "no failure in T seconds, zero passes", which is not a result: the run may
 * simply be slower than the defect is deep. The box/unbox page (retired as
 * `zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`)
 * records exactly that arm being voided, and names the missing progress signal
 * as the thing to build. This is it.
 *
 * The failure is at `seed:0 op:1033`, i.e. inside the FIRST `testOps` call, so
 * one 3000-op loop reproduces the whole of it. Ops per second is printed so a
 * slow arm can be scored on whether it REACHED op 1033, not on wall clock.
 *
 * # It also says what diverged
 *
 * The real test's message is the bare cursor range (`(1810, null)`), which does
 * not say which entry was extra. On a mismatch this prints the reference keys,
 * the keys the cursor actually yielded, and the difference.
 *
 * Usage: H2MapOpsProbe [ops] [seed] [progressEvery]
 */
public final class H2MapOpsProbe {

    private static final Random r = new Random();
    private static int op;

    public static void main(String[] args) {
        int loopCount = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        long seed = args.length > 1 ? Long.parseLong(args[1]) : 0L;
        int every = args.length > 2 ? Integer.parseInt(args[2]) : 100;

        long t0 = System.currentTimeMillis();
        boolean ok = testOps("memFS:TestRandomMapOps", loopCount, seed, every);
        long ms = System.currentTimeMillis() - t0;
        System.out.println("PROBE reached_op=" + op + " of " + loopCount
                + " ms=" + ms + " verdict=" + (ok ? "OK" : "MISMATCH"));
        System.exit(ok ? 0 : 1);
    }

    private static MVStore openStore(String fileName) {
        MVStore s = new MVStore.Builder().fileName(fileName)
                .keysPerPage(7).autoCommitDisabled().open();
        s.setRetentionTime(1000);
        return s;
    }

    private static boolean testOps(String fileName, int loopCount, long seed, int every) {
        r.setSeed(seed);
        op = 0;
        MVStore s = openStore(fileName);
        int keysPerPage = s.getKeysPerPage();
        int keyRange = 2000;
        MVMap<Integer, String> m = s.openMap("data");
        TreeMap<Integer, String> map = new TreeMap<>();
        int[] recentKeys = new int[2 * keysPerPage];
        for (; op < loopCount; op++) {
            if (every > 0 && op % every == 0) {
                System.out.println("PROGRESS op=" + op + " size=" + map.size());
                System.out.flush();
            }
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
                m.put(k, v);
                map.put(k, v);
                break;
            case 4:
            case 5:
                m.remove(k);
                map.remove(k);
                break;
            case 6:
                s.compact(90, 1024);
                break;
            case 7:
                if (op % 64 == 0) {
                    m.clear();
                    map.clear();
                }
                break;
            case 8:
                s.commit();
                break;
            case 9:
                if (fileName != null) {
                    s.commit();
                    s.close();
                    s = openStore(fileName);
                    m = s.openMap("data");
                }
                break;
            case 10:
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
            default: {
                ArrayList<Integer> keyList = new ArrayList<>(map.keySet());
                int index = Collections.binarySearch(keyList, k, null);
                int index2 = (int) m.getKeyIndex(k);
                if (!eq("getKeyIndex", index, index2)) {
                    return false;
                }
                if (index >= 0) {
                    if (!eq("getKey", m.getKey(index), k)) {
                        return false;
                    }
                }
                break;
            }
            }
            if (!eq("get", map.get(k), m.get(k))
                    || !eq("ceilingKey", map.ceilingKey(k), m.ceilingKey(k))
                    || !eq("floorKey", map.floorKey(k), m.floorKey(k))
                    || !eq("higherKey", map.higherKey(k), m.higherKey(k))
                    || !eq("lowerKey", map.lowerKey(k), m.lowerKey(k))
                    || !eq("isEmpty", map.isEmpty(), m.isEmpty())
                    || !eq("size", map.size(), m.size())) {
                return false;
            }
            if (!map.isEmpty()) {
                if (!eq("firstKey", map.firstKey(), m.firstKey())
                        || !eq("lastKey", map.lastKey(), m.lastKey())) {
                    return false;
                }
            }

            int from = r.nextBoolean() ? r.nextInt(keyRange)
                    : k + r.nextInt(2 * keysPerPage) - keysPerPage;
            int to = r.nextBoolean() ? r.nextInt(keyRange)
                    : from + r.nextInt(2 * keysPerPage) - keysPerPage;

            if (from <= to) {
                if (!cmp("(" + from + ", null)", map.tailMap(from).entrySet(),
                        m.cursor(from, null, false))) {
                    return false;
                }
                if (!cmp("(null, " + from + ")", map.headMap(from + 1).entrySet(),
                        m.cursor(null, from, false))) {
                    return false;
                }
                if (!cmp("(" + from + ", " + to + ")", map.subMap(from, to + 1).entrySet(),
                        m.cursor(from, to, false))) {
                    return false;
                }
            }
            if (from >= to) {
                if (!cmp("rev (" + from + ", null)", reverse(map.headMap(from + 1).entrySet()),
                        m.cursor(from, null, true))) {
                    return false;
                }
                if (!cmp("rev (null, " + from + ")", reverse(map.tailMap(from).entrySet()),
                        m.cursor(null, from, true))) {
                    return false;
                }
                if (!cmp("rev (" + from + ", " + to + ")", reverse(map.subMap(to, from + 1).entrySet()),
                        m.cursor(from, to, true))) {
                    return false;
                }
            }
        }
        s.close();
        return true;
    }

    private static <K, V> Collection<Map.Entry<K, V>> reverse(Collection<Map.Entry<K, V>> entrySet) {
        ArrayList<Map.Entry<K, V>> list = new ArrayList<>(entrySet);
        Collections.reverse(list);
        return list;
    }

    /** The cursor comparison, with the diff the real test does not print. */
    private static <K, V> boolean cmp(String msg, Iterable<Map.Entry<K, V>> entrySet,
            Cursor<K, V> cursor) {
        ArrayList<K> want = new ArrayList<>();
        ArrayList<K> got = new ArrayList<>();
        boolean bad = false;
        String why = null;
        int cnt = 0;
        for (Map.Entry<K, V> entry : entrySet) {
            want.add(entry.getKey());
            if (!cursor.hasNext()) {
                bad = true;
                why = "cursor ran out at " + cnt;
                break;
            }
            K next = cursor.next();
            got.add(next);
            if (!Objects.equals(entry.getKey(), next)) {
                bad = true;
                why = "key mismatch at " + cnt + ": want " + entry.getKey() + " got " + next;
                break;
            }
            if (!Objects.equals(entry.getKey(), cursor.getKey())) {
                bad = true;
                why = "getKey mismatch at " + cnt;
                break;
            }
            if (!Objects.equals(entry.getValue(), cursor.getValue())) {
                bad = true;
                why = "value mismatch at " + cnt + ": want " + entry.getValue()
                        + " got " + cursor.getValue();
                break;
            }
            cnt++;
        }
        if (!bad && cursor.hasNext()) {
            bad = true;
            ArrayList<K> extra = new ArrayList<>();
            while (cursor.hasNext() && extra.size() < 20) {
                extra.add(cursor.next());
            }
            why = "cursor yielded EXTRA after " + cnt + ": " + extra;
        }
        if (bad) {
            System.out.println("MISMATCH op=" + op + " cursor " + msg + " -- " + why);
            System.out.println("  want(" + want.size() + ")=" + head(want));
            System.out.println("  got (" + got.size() + ")=" + head(got));
            return false;
        }
        return true;
    }

    private static <T> String head(ArrayList<T> l) {
        if (l.size() <= 24) {
            return l.toString();
        }
        return l.subList(0, 12) + " ... " + l.subList(l.size() - 12, l.size());
    }

    private static boolean eq(String what, Object want, Object got) {
        if (Objects.equals(want, got)) {
            return true;
        }
        System.out.println("MISMATCH op=" + op + " " + what + ": want " + want + " got " + got);
        return false;
    }
}
