// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Set;
import java.util.Vector;
import java.util.stream.IntStream;
import java.util.stream.Stream;

/**
 * COPYING a collection, as opposed to reading one.
 *
 * WHY THIS VECTOR EXISTS.
 *
 * The native layout probe that reads a foreign collection guessed its
 * shape from the runtime value of slot 1: an `Object[]` in slot 0 and an
 * "int" in slot 1 meant `(elementData, size)`. CratonVM stores a
 * `boolean` and an `int` in the same `Value::Int`, so a class laid out
 * `(Object[], boolean)` read as a collection whose size was that
 * boolean — and a `true` read as size ONE.
 *
 * `java.util.ImmutableCollections$ListN` is exactly that shape since JDK
 * 20: `(E[] elements, boolean allowNulls)`. `Stream.toList()` builds it
 * through `listFromTrustedArrayNullsAllowed`, i.e. with `allowNulls =
 * true`. So every copy of such a list kept its first element and
 * dropped the rest, in silence.
 *
 * WHAT MAKES THIS HARD TO SEE, and why the assertions are shaped the way
 * they are: the list itself was fine. `size()`, `get()`, `contains()`,
 * `indexOf()`, `iterator()`, `stream().count()` and `toArray()` all
 * answered 6 for a 6-element list. ONLY the copy paths were wrong. A
 * vector that checks a list's own accessors — the obvious thing to
 * check — passes against this defect completely. So every assertion
 * below copies INTO something and then measures the copy.
 *
 * The destinations are varied on purpose (ArrayList, LinkedList, Vector,
 * HashSet, LinkedHashSet, copy constructor, List.copyOf, addAll at an
 * index) because the defect was in the shared source-reading helper, so
 * a single destination would have looked like a single native's bug.
 *
 * The real-world victim was GPULlama3's tokenizer: `encodeAsList`
 * returns `Arrays.stream(ids).boxed().toList()`, the chat formatter does
 * `tokens.addAll(...)`, and a 16-token prompt arrived at the model as
 * 11. The model answered a question it had not been asked.
 */
public class RStreamToListCopy {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void checkEquals(Object expected, Object actual, String m) {
        checks++;
        if (!expected.equals(actual)) {
            throw new AssertionError(m + ": expected " + expected + " got " + actual);
        }
    }

    /** The exact shape GPULlama3's tokenizer produces. */
    static List<Integer> boxedFromIntArray() {
        int[] ints = {10445, 374, 279, 13180, 6437, 30};
        return Arrays.stream(ints).boxed().toList();
    }

    static void theListItselfIsFine() {
        List<Integer> src = boxedFromIntArray();
        checkEquals(6, src.size(), "size");
        checkEquals(30, src.get(5), "get(5)");
        check(src.contains(13180), "contains");
        checkEquals(5, src.indexOf(30), "indexOf");
        checkEquals(6L, src.stream().count(), "stream().count()");
        checkEquals(6, src.toArray().length, "toArray().length");
        checkEquals(6, src.toArray(new Integer[0]).length, "typed toArray().length");
        int iterated = 0;
        for (Integer ignored : src) {
            iterated++;
        }
        checkEquals(6, iterated, "iterator");
    }

    static void everyCopyKeepsEveryElement() {
        List<Integer> src = boxedFromIntArray();

        List<Integer> viaAddAll = new ArrayList<>();
        check(viaAddAll.addAll(src), "ArrayList.addAll reports a change");
        checkEquals(src, viaAddAll, "ArrayList.addAll content");

        List<Integer> viaCtor = new ArrayList<>(src);
        checkEquals(src, viaCtor, "ArrayList copy constructor content");

        checkEquals(src, List.copyOf(src), "List.copyOf content");

        List<Integer> ll = new LinkedList<>();
        ll.addAll(src);
        checkEquals(6, ll.size(), "LinkedList.addAll size");

        Vector<Integer> vec = new Vector<>();
        vec.addAll(src);
        checkEquals(6, vec.size(), "Vector.addAll size");

        Set<Integer> hs = new HashSet<>();
        hs.addAll(src);
        checkEquals(6, hs.size(), "HashSet.addAll size");

        Set<Integer> lhs = new LinkedHashSet<>(src);
        checkEquals(6, lhs.size(), "LinkedHashSet copy constructor size");

        // Positional addAll takes a different path than the appending one.
        List<Integer> positional = new ArrayList<>(List.of(-1, -2));
        positional.addAll(1, src);
        checkEquals(8, positional.size(), "positional addAll size");
        checkEquals(-1, positional.get(0), "positional addAll kept the prefix");
        checkEquals(30, positional.get(6), "positional addAll kept the last source element");
        checkEquals(-2, positional.get(7), "positional addAll kept the suffix");
    }

    /**
     * The same for the other stream sources, so a fix that special-cases
     * one construction path is not mistaken for a fix.
     */
    static void everyStreamSourceCopiesWhole() {
        checkEquals(6, new ArrayList<>(Stream.of(1, 2, 3, 4, 5, 6).toList()).size(),
                "Stream.of(...).toList() copies whole");
        checkEquals(6, new ArrayList<>(IntStream.range(0, 6).boxed().toList()).size(),
                "IntStream.range().boxed().toList() copies whole");
        checkEquals(6, new ArrayList<>(
                IntStream.range(0, 12).filter(i -> i % 2 == 0).boxed().toList()).size(),
                "filtered toList copies whole");
        checkEquals(6, new ArrayList<>(
                Stream.of(1, 2, 3, 4, 5, 6).map(x -> x * 2).toList()).size(),
                "mapped toList copies whole");
        checkEquals(6, new ArrayList<>(List.of(1, 2, 3, 4, 5, 6)).size(),
                "List.of copies whole");
        checkEquals(2, new ArrayList<>(Arrays.stream(new int[] {7, 8}).boxed().toList()).size(),
                "two-element boxed toList copies whole");
        checkEquals(1, new ArrayList<>(Arrays.stream(new int[] {7}).boxed().toList()).size(),
                "one-element boxed toList copies whole");
        checkEquals(0, new ArrayList<>(Arrays.stream(new int[0]).boxed().toList()).size(),
                "empty boxed toList copies whole");
    }

    /**
     * A list this long cannot be mistaken for its own first element by
     * accident, and it crosses any small-size special case.
     */
    static void aLongOneToo() {
        int[] many = new int[257];
        for (int i = 0; i < many.length; i++) {
            many[i] = i * 3 + 1;
        }
        List<Integer> src = Arrays.stream(many).boxed().toList();
        List<Integer> copy = new ArrayList<>(src);
        checkEquals(257, copy.size(), "257-element copy size");
        checkEquals(1, copy.get(0), "257-element copy first");
        checkEquals(769, copy.get(256), "257-element copy last");
    }

    public static void main(String[] args) {
        theListItselfIsFine();
        everyCopyKeepsEveryElement();
        everyStreamSourceCopiesWhole();
        aLongOneToo();
        System.out.println("CK RStreamToListCopy checks=" + checks);
        System.out.println("PASS RStreamToListCopy (" + checks + " checks)");
    }
}
