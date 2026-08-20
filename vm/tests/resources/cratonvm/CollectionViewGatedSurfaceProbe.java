// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

import java.util.AbstractMap;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Hashtable;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.Spliterator;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.Callable;
import java.util.concurrent.ConcurrentHashMap;
import java.util.stream.StreamSupport;

/**
 * The methods `force_native_over_real_jdk_bytecode` gates on the map/set view
 * carriers, exercised on every carrier it names.
 *
 * A gate entry is not a registration. When the gate lists a method and nothing
 * is registered for it, the lookup falls through to exactly the JDK bytecode
 * the gate exists to avoid — and that bytecode reads fields a CratonVM-minted
 * view keeps somewhere else, so it either NPEs on a null backing or silently
 * answers "empty". Both shapes shipped at once:
 *
 * <ul>
 *   <li>{@code ConcurrentHashMap$EntrySetView.removeIf} → {@code map.removeEntryIf(..)}
 *       over a null {@code map}.</li>
 *   <li>{@code TreeSet.spliterator} / {@code TreeMap$KeySet.spliterator} →
 *       {@code TreeMap.keySpliteratorFor(m)} over a null {@code sm}.</li>
 * </ul>
 *
 * Only the carriers that DECLARE the method notice, which is why a sweep over
 * the whole family is the test and not a single row: the others are inheriting
 * a {@code Collection} default today and are one JDK upgrade from not doing so.
 *
 * Every expectation is derived from the collection's own contents, so the probe
 * needs no HotSpot arm and cannot pass vacuously.
 */
public final class CollectionViewGatedSurfaceProbe {

    private static int failures;

    private static void fail(String message) {
        failures++;
        System.out.println("GATED_SURFACE_FAIL " + message);
    }

    private static void check(String tag, Callable<Object> body, Object expected) {
        Object actual;
        try {
            actual = body.call();
        } catch (Throwable t) {
            fail(tag + " threw " + t);
            return;
        }
        if (!String.valueOf(expected).equals(String.valueOf(actual))) {
            fail(tag + ": " + actual + ", expected " + expected);
        }
    }

    /** The gated surface of a SET-shaped view (`SET_VIEW_CARRIERS` + TreeSet). */
    private static void sweepSet(String tag, Set<String> s, int n) {
        check(tag + ".spliterator.count",
                () -> StreamSupport.stream(s.spliterator(), false).count(), (long) n);
        check(tag + ".spliterator.noNulls",
                () -> StreamSupport.stream(s.spliterator(), false).noneMatch(Objects::isNull), true);
        check(tag + ".stream.count", () -> s.stream().count(), (long) n);
        check(tag + ".removeIf(never)", () -> s.removeIf(x -> false), false);
        check(tag + ".size after removeIf(never)", s::size, n);
        check(tag + ".containsAll(self)", () -> s.containsAll(new ArrayList<>(s)), true);
        check(tag + ".equals(copy)", () -> s.equals(new LinkedHashSet<>(s)), true);
        check(tag + ".hashCode==setHash", () -> s.hashCode() == new HashSet<>(s).hashCode(), true);
        check(tag + ".toArray(T[]).length", () -> s.toArray(new String[0]).length, n);
        check(tag + ".toArray(T[]).noNulls",
                () -> Arrays.stream(s.toArray(new String[0])).noneMatch(Objects::isNull), true);
        check(tag + ".forEach.count", () -> {
            int[] seen = {0};
            s.forEach(x -> seen[0]++);
            return seen[0];
        }, n);
    }

    /** The gated surface of an ENTRYSET view. */
    private static void sweepEntrySet(String tag, Map<String, Integer> m) {
        Set<Map.Entry<String, Integer>> es = m.entrySet();
        int n = m.size();
        check(tag + ".spliterator.count",
                () -> StreamSupport.stream(es.spliterator(), false).count(), (long) n);
        check(tag + ".stream.count", () -> es.stream().count(), (long) n);
        check(tag + ".removeIf(never)", () -> es.removeIf(e -> false), false);
        check(tag + ".size after removeIf(never)", m::size, n);
        check(tag + ".toArray(T[]).noNulls",
                () -> Arrays.stream(es.toArray(new Map.Entry[0])).noneMatch(Objects::isNull), true);
        check(tag + ".forEach.count", () -> {
            int[] seen = {0};
            es.forEach(e -> seen[0]++);
            return seen[0];
        }, n);
        // removeIf that DOES match has to write through to the backing map.
        check(tag + ".removeIf(one)", () -> es.removeIf(e -> e.getKey().equals("b")), true);
        check(tag + ".backing map lost it", () -> m.containsKey("b"), false);
        check(tag + ".backing map size", m::size, n - 1);
    }

    /** The gated surface of a VALUES view (`MAP_VIEW_CARRIERS`). */
    private static void sweepValues(String tag, Map<String, Integer> m) {
        Collection<Integer> vs = m.values();
        int n = m.size();
        check(tag + ".spliterator.count",
                () -> StreamSupport.stream(vs.spliterator(), false).count(), (long) n);
        check(tag + ".stream.sum", () -> vs.stream().mapToInt(Integer::intValue).sum(), 6);
        check(tag + ".removeIf(never)", () -> vs.removeIf(v -> false), false);
        check(tag + ".toArray(T[]).noNulls",
                () -> Arrays.stream(vs.toArray(new Integer[0])).noneMatch(Objects::isNull), true);
        check(tag + ".removeIf(one)", () -> vs.removeIf(v -> v == 2), true);
        check(tag + ".backing map lost it", () -> m.containsKey("b"), false);
        check(tag + ".backing map size", m::size, n - 1);
    }

    private static Map<String, Integer> seed(Map<String, Integer> m) {
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        return m;
    }

    public static void main(String[] args) {
        sweepSet("treeMap.keySet", seed(new TreeMap<>()).keySet(), 3);
        sweepSet("treeSet", new TreeSet<>(Arrays.asList("a", "b", "c")), 3);
        sweepSet("hashMap.keySet", seed(new HashMap<>()).keySet(), 3);
        sweepSet("linkedHashMap.keySet", seed(new LinkedHashMap<>()).keySet(), 3);
        sweepSet("hashtable.keySet", seed(new Hashtable<>()).keySet(), 3);
        sweepSet("chm.keySet", seed(new ConcurrentHashMap<>()).keySet(), 3);
        sweepSet("hashSet", new HashSet<>(Arrays.asList("a", "b", "c")), 3);
        sweepSet("linkedHashSet", new LinkedHashSet<>(Arrays.asList("a", "b", "c")), 3);

        sweepEntrySet("hashMap.entrySet", seed(new HashMap<>()));
        sweepEntrySet("linkedHashMap.entrySet", seed(new LinkedHashMap<>()));
        sweepEntrySet("treeMap.entrySet", seed(new TreeMap<>()));
        sweepEntrySet("hashtable.entrySet", seed(new Hashtable<>()));
        sweepEntrySet("chm.entrySet", seed(new ConcurrentHashMap<>()));

        sweepValues("hashMap.values", seed(new HashMap<>()));
        sweepValues("linkedHashMap.values", seed(new LinkedHashMap<>()));
        sweepValues("treeMap.values", seed(new TreeMap<>()));
        sweepValues("hashtable.values", seed(new Hashtable<>()));
        sweepValues("chm.values", seed(new ConcurrentHashMap<>()));

        // A user AbstractMap: nothing about the gate should be needed here, and
        // a divergence would say the gate is reaching classes it must not.
        Map<String, Integer> plain = new AbstractMap<>() {
            private final Map<String, Integer> d = seed(new LinkedHashMap<>());

            @Override
            public Set<Entry<String, Integer>> entrySet() {
                return d.entrySet();
            }
        };
        check("abstractMap.keySet.spliterator.count",
                () -> StreamSupport.stream(plain.keySet().spliterator(), false).count(), 3L);

        // `Spliterator.getExactSizeIfKnown()` is negative when unknown, which is
        // allowed — but a POSITIVE answer has to be the truth.
        Spliterator<String> sp = seed(new TreeMap<>()).keySet().spliterator();
        long exact = sp.getExactSizeIfKnown();
        if (exact >= 0 && exact != 3) {
            fail("treeMap.keySet.spliterator.getExactSizeIfKnown: " + exact + ", expected 3 or -1");
        }

        System.out.println("failures=" + failures);
        if (failures == 0) {
            System.out.println("GATED_SURFACE_OK");
        }
    }

    private CollectionViewGatedSurfaceProbe() {}
}
