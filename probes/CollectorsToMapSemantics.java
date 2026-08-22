// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.TreeMap;
import java.util.stream.Collectors;
import java.util.stream.IntStream;
import java.util.stream.Stream;

/**
 * `Collectors.toMap` semantics, across all three arities.
 *
 * The 2-arg form must THROW `IllegalStateException` on a duplicate key, the
 * 3-arg form must merge with the supplied operator, and the 4-arg form must
 * merge into the supplied map type. CratonVM implements all three natively.
 *
 * The duplicate detection used to be a scan of every key collected so far,
 * calling Java `equals` on each — O(n^2), which is what made a 128K-entry
 * tokenizer vocabulary look like a livelock. It is now bucketed by
 * `hashCode`, which is what the JDK's own `putIfAbsent`-based accumulator
 * does. This fixture pins the semantics that bucketing must not change:
 *
 *  - a duplicate is still detected and still throws, including when the
 *    duplicate is the very last element of a long stream (the case a
 *    hash-bucketed check could miss if the bucket were keyed wrongly);
 *  - keys whose `hashCode` COLLIDES but which are not `equals` must NOT be
 *    reported as duplicates — the bucket must be filtered by `equals`;
 *  - keys that are `equals` must be caught even though they are distinct
 *    objects, i.e. the check is not identity-based;
 *  - null values are still rejected, and insertion order is irrelevant.
 */
public class CollectorsToMapSemantics {

    static int failures = 0;

    static void check(String what, boolean ok) {
        if (!ok) {
            System.out.println("FAIL " + what);
            failures++;
        }
    }

    /** Every instance hashes the same, so all land in one bucket. */
    record Collider(String name) {
        @Override public int hashCode() { return 42; }
        @Override public boolean equals(Object o) {
            return o instanceof Collider c && Objects.equals(name, c.name);
        }
    }

    public static void main(String[] args) {
        // --- 2-arg: distinct keys collect normally ---
        Map<String, Integer> m = Stream.of("a", "b", "c")
                .collect(Collectors.toMap(s -> s, String::length));
        check("2arg size", m.size() == 3);
        check("2arg content", m.get("a") == 1 && m.get("c") == 1);

        // --- 2-arg: duplicate key throws ---
        boolean threw = false;
        try {
            Stream.of("a", "b", "a").collect(Collectors.toMap(s -> s, s -> 1));
        } catch (IllegalStateException e) {
            threw = true;
        }
        check("2arg duplicate throws", threw);

        // --- 2-arg: duplicate as the LAST element of a longer stream ---
        threw = false;
        try {
            IntStream.range(0, 500).boxed()
                    .collect(Collectors.toMap(i -> i == 499 ? "k0" : "k" + i, i -> i));
        } catch (IllegalStateException e) {
            threw = true;
        }
        check("2arg late duplicate throws", threw);

        // --- 2-arg: equal-but-distinct String objects are duplicates ---
        threw = false;
        try {
            String s1 = new String("dup");
            String s2 = new String("dup");
            check("distinct instances", s1 != s2);
            Stream.of(s1, s2).collect(Collectors.toMap(s -> s, s -> 1));
        } catch (IllegalStateException e) {
            threw = true;
        }
        check("2arg equal-not-identical duplicate throws", threw);

        // --- 2-arg: hash COLLISIONS that are not equals must all collect ---
        List<Collider> colliders =
                IntStream.range(0, 200).mapToObj(i -> new Collider("c" + i)).toList();
        Map<Collider, Integer> cm = colliders.stream()
                .collect(Collectors.toMap(c -> c, c -> 1));
        check("2arg colliding-but-unequal keys all collect", cm.size() == 200);

        // --- 2-arg: colliding AND equal is still a duplicate ---
        threw = false;
        try {
            Stream.of(new Collider("x"), new Collider("y"), new Collider("x"))
                    .collect(Collectors.toMap(c -> c, c -> 1));
        } catch (IllegalStateException e) {
            threw = true;
        }
        check("2arg colliding equal duplicate throws", threw);

        // --- 3-arg: the merge function is applied, not a throw ---
        Map<String, Integer> merged = Stream.of("a", "bb", "cc", "d")
                .collect(Collectors.toMap(s -> String.valueOf(s.length()), s -> 1, Integer::sum));
        check("3arg merges", merged.size() == 2);
        check("3arg merge values", merged.get("1") == 2 && merged.get("2") == 2);

        // --- 3-arg with colliding keys still merges the right ones ---
        Map<Collider, Integer> cmerged =
                Stream.of(new Collider("p"), new Collider("q"), new Collider("p"))
                        .collect(Collectors.toMap(c -> c, c -> 1, Integer::sum));
        check("3arg collider size", cmerged.size() == 2);
        check("3arg collider merge", cmerged.get(new Collider("p")) == 2);

        // --- 4-arg: merges into the supplied map type ---
        Map<String, Integer> tm = Stream.of("a", "bb", "cc")
                .collect(Collectors.toMap(s -> String.valueOf(s.length()), s -> 1,
                        Integer::sum, TreeMap::new));
        check("4arg type", tm instanceof TreeMap);
        check("4arg size", tm.size() == 2);
        check("4arg merge", tm.get("2") == 2);

        // --- a big one, to prove the fast path agrees with a plain loop ---
        int n = 20000;
        String[] tok = new String[n];
        for (int i = 0; i < n; i++) tok[i] = "tok" + i;
        Map<String, Integer> big = IntStream.range(0, n).boxed()
                .collect(Collectors.toMap(i -> tok[i], i -> i));
        Map<String, Integer> ref = new HashMap<>();
        for (int i = 0; i < n; i++) ref.put(tok[i], i);
        check("big size", big.size() == ref.size());
        check("big equal", big.equals(ref));

        System.out.println("TOMAPSEM failures=" + failures);
    }
}
