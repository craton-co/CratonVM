// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

import java.util.AbstractCollection;
import java.util.AbstractSet;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * `AbstractCollection.toArray(T[])` on a collection whose `iterator()` is a
 * Map view's iterator must return the elements, not a right-length array of
 * nulls.
 *
 * This is Jetty's `org.eclipse.jetty.util.ClassMatcher` shape exactly: an
 * `AbstractSet<String>` over a private `Map`, with `getPatterns()` =
 * `toArray(new String[size()])`. CratonVM's `real_jdk_to_array_typed` used to
 * drive that inherited method's iterator loop with a BY-NAME
 * `invoke("java/util/LinkedHashMap$LinkedKeyIterator", "hasNext", ...)`, which
 * runs the real `HashIterator` bytecode over fields a CratonVM-minted snapshot
 * carrier never populates — `hasNext()` answered false on the FIRST element,
 * so the loop broke at i = 0 and every slot stayed null while `size()` had
 * already fixed the length.
 *
 * The probe compares each `toArray` overload against this collection's own
 * iterator, so it needs no HotSpot arm and cannot pass vacuously: a VM that
 * iterates nothing at all reports mismatched lengths rather than agreement.
 */
public final class MapBackedSetToArrayProbe {

    /** `iterator()` delegates to a `LinkedHashMap` keySet — `LinkedKeyIterator`. */
    static final class LinkedKeyBackedSet extends AbstractSet<String> {
        private final Map<String, Integer> entries = new LinkedHashMap<>();

        @Override
        public Iterator<String> iterator() {
            return entries.keySet().iterator();
        }

        @Override
        public int size() {
            return entries.size();
        }

        @Override
        public boolean add(String s) {
            return entries.put(s, 1) == null;
        }
    }

    /** `iterator()` delegates to a plain `HashMap` keySet — `HashMap$KeyIterator`. */
    static final class HashKeyBackedSet extends AbstractSet<String> {
        private final Map<String, Integer> entries = new HashMap<>();

        @Override
        public Iterator<String> iterator() {
            return entries.keySet().iterator();
        }

        @Override
        public int size() {
            return entries.size();
        }

        @Override
        public boolean add(String s) {
            return entries.put(s, 1) == null;
        }
    }

    /** `iterator()` delegates to a map's `values()` view. */
    static final class ValuesBackedCollection extends AbstractCollection<String> {
        private final Map<String, String> entries = new LinkedHashMap<>();

        @Override
        public Iterator<String> iterator() {
            return entries.values().iterator();
        }

        @Override
        public int size() {
            return entries.size();
        }

        @Override
        public boolean add(String s) {
            return entries.put(s, s) == null;
        }
    }

    private static int failures;

    private static void check(String tag, AbstractCollection<String> c) {
        // The reference answer is this collection's OWN iterator, walked
        // directly — the thing every `toArray` overload is defined in terms of.
        String[] expected = new String[c.size()];
        int n = 0;
        for (String s : c) {
            expected[n++] = s;
        }
        if (n != c.size()) {
            fail(tag + ": iterator yielded " + n + " of " + c.size() + " elements");
            return;
        }

        compare(tag + " toArray(new String[0])", expected, c.toArray(new String[0]), expected.length);
        compare(
                tag + " toArray(new String[size])",
                expected,
                c.toArray(new String[c.size()]),
                expected.length);
        // An oversized template keeps the caller's array and gets a null
        // terminator at `size`; only the first `size` slots are the elements.
        compare(
                tag + " toArray(new String[size + 2])",
                expected,
                c.toArray(new String[c.size() + 2]),
                expected.length + 2);
        // `toArray()` shares none of the typed path's plumbing — a control that
        // says whether a failure is specific to the typed overload.
        Object[] untyped = c.toArray();
        compare(tag + " toArray()", expected, untyped, expected.length);
    }

    private static void compare(String tag, String[] expected, Object[] actual, int expectedLen) {
        if (actual == null) {
            fail(tag + ": returned null");
            return;
        }
        if (actual.length != expectedLen) {
            fail(tag + ": length " + actual.length + ", expected " + expectedLen);
            return;
        }
        for (int i = 0; i < expected.length; i++) {
            if (!expected[i].equals(actual[i])) {
                fail(tag + ": " + Arrays.toString(actual) + " != " + Arrays.toString(expected));
                return;
            }
        }
        for (int i = expected.length; i < expectedLen; i++) {
            if (actual[i] != null) {
                fail(tag + ": slot " + i + " past the elements is " + actual[i] + ", expected null");
                return;
            }
        }
    }

    private static void fail(String message) {
        failures++;
        System.out.println("MAPSET_TOARRAY_FAIL " + message);
    }

    public static void main(String[] args) {
        LinkedKeyBackedSet linked = new LinkedKeyBackedSet();
        linked.add("org.eclipse.jetty.");
        linked.add("-org.eclipse.jetty.util.");
        linked.add("org.springframework.boot.loader.");
        check("linkedKeySet", linked);

        HashKeyBackedSet hashed = new HashKeyBackedSet();
        hashed.add("alpha");
        hashed.add("beta");
        check("hashKeySet", hashed);

        ValuesBackedCollection values = new ValuesBackedCollection();
        values.add("one");
        values.add("two");
        values.add("three");
        check("valuesView", values);

        System.out.println("failures=" + failures);
        if (failures == 0) {
            System.out.println("MAPSET_TOARRAY_OK");
        }
    }

    private MapBackedSetToArrayProbe() {}
}
