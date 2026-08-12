// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Arrays;
import java.util.ConcurrentModificationException;
import java.util.IntSummaryStatistics;
import java.util.Iterator;
import java.util.List;
import java.util.ListIterator;
import java.util.Locale;
import java.util.Map;
import java.util.NavigableMap;
import java.util.NavigableSet;
import java.util.SortedMap;
import java.util.TreeMap;
import java.util.stream.IntStream;

/**
 * Permanent gate for the four defect families recorded in
 * docs/known-issues/jdk-only/W7-1-treemap-views-and-iterator-remove-contract.md,
 * found 2026-08-10 by the widened ShadowDifferentialProbe in --real-jdk mode.
 * They were found by a PROBE, and a probe is not a gate: nothing in this suite
 * would have noticed them coming back. This class is that gate.
 *
 * The four families:
 *
 *   1. NAVIGABLE VIEWS ARE SNAPSHOTS. headMap/tailMap/subMap/descendingMap/
 *      descendingKeySet are specified as views -- a write through one is a
 *      write to the backing map. CratonVM answered an EMPTY collection for
 *      descendingMap()/descendingKeySet(), and a headMap().remove() never
 *      reached the map. An empty view reads as a pass everywhere a caller only
 *      iterates, so every view assertion here checks CONTENT and ORDER, never
 *      just "not null".
 *   2. Iterator.remove HAS NO STATE MACHINE. remove() before next() must throw
 *      IllegalStateException; CratonVM accepted it AND deleted the first
 *      element, so a loop guarding itself with that exception silently ate one
 *      extra item per iteration. ListIterator.set/add were short by one in the
 *      same direction.
 *   3. String.format's float conversions (%e/%g) printed Double.toString-shaped
 *      output, and StringBuilder.delete accepted an out-of-range start.
 *   4. IntStream.summaryStatistics() killed the run outright.
 *
 * DETERMINISM: run.sh diffs every CK line against HotSpot, so nothing printed
 * here may depend on the host. Ordered containers only, and every format
 * assertion is pinned to Locale.ROOT (see formats()).
 *
 * BOUNDED BY CONSTRUCTION: the two fail-fast loops in failFast() rely on the
 * very exception they assert in order to terminate. On a VM whose iterators are
 * not fail-fast the growing one would run until the heap is gone and take the
 * rest of the suite run with it -- exactly the hazard that truncated the first
 * widened probe run. Both are capped at CME_CAP and report a named failure.
 */
public class RJdkViews {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Iteration cap for the self-terminating fail-fast loops. Small: HotSpot
     * raises the CME on the first {@code next()} after the modification, so a
     * healthy VM never reaches 2. Anything that reaches the cap is a failure.
     */
    private static final int CME_CAP = 100;

    static TreeMap<String, Integer> abcd() {
        TreeMap<String, Integer> tm = new TreeMap<>();
        tm.put("a", 1);
        tm.put("b", 2);
        tm.put("c", 3);
        tm.put("d", 4);
        return tm;
    }

    // ---- family 1: the navigable views are views, not snapshots -------------

    static void navigableViews() {
        TreeMap<String, Integer> tm = abcd();

        // descendingMap: reversed CONTENT, not an empty map. Size is asserted
        // separately from toString so an empty answer cannot be mistaken for a
        // formatting difference.
        NavigableMap<String, Integer> desc = tm.descendingMap();
        check(desc.size() == 4, "descendingMap size: " + desc.size());
        check(desc.toString().equals("{d=4, c=3, b=2, a=1}"), "descendingMap: " + desc);
        check(new ArrayList<>(desc.keySet()).equals(Arrays.asList("d", "c", "b", "a")),
                "descendingMap key order: " + desc.keySet());
        check(desc.firstKey().equals("d") && desc.lastKey().equals("a"),
                "descendingMap first/last: " + desc.firstKey() + "/" + desc.lastKey());
        check(desc.get("c") == 3, "descendingMap lookup: " + desc.get("c"));

        NavigableSet<String> dks = tm.descendingKeySet();
        check(dks.size() == 4, "descendingKeySet size: " + dks.size());
        check(dks.toString().equals("[d, c, b, a]"), "descendingKeySet: " + dks);
        check(new ArrayList<>(dks).equals(Arrays.asList("d", "c", "b", "a")),
                "descendingKeySet order: " + dks);
        System.out.println("CK RJdkViews descendingMap=" + desc + " descendingKeySet=" + dks);

        // A view is live in BOTH directions. First: backing map -> view.
        SortedMap<String, Integer> head = tm.headMap("c");
        check(head.toString().equals("{a=1, b=2}"), "headMap content: " + head);
        tm.put("aa", 9);
        check(head.size() == 3 && head.containsKey("aa"),
                "a write to the backing map must show through the live view: " + head);
        tm.remove("aa");
        check(head.size() == 2 && !head.containsKey("aa"),
                "a remove from the backing map must show through the live view: " + head);

        // Then: view -> backing map. This is the row the record measured.
        Integer removed = head.remove("a");
        check(removed != null && removed == 1, "headMap.remove must return the mapping: " + removed);
        check(tm.toString().equals("{b=2, c=3, d=4}"),
                "a write THROUGH the view must reach the backing map: " + tm);
        check(!tm.containsKey("a"), "the key removed through the view is gone from the map");
        check(desc.toString().equals("{d=4, c=3, b=2}"),
                "the descendingMap view tracks the backing map: " + desc);
        System.out.println("CK RJdkViews headMapRemove=" + tm);

        // pollFirstEntry is a cascade of the row above, not an independent
        // defect: it answers b=2 only because the headMap remove landed.
        Map.Entry<String, Integer> first = tm.pollFirstEntry();
        check(first != null, "pollFirstEntry must not be null on a non-empty map");
        check(first.getKey().equals("b") && first.getValue() == 2,
                "pollFirstEntry: " + first.getKey() + "=" + first.getValue());
        check(tm.toString().equals("{c=3, d=4}"), "pollFirstEntry must remove the entry: " + tm);
        System.out.println("CK RJdkViews pollFirstEntry=" + first.getKey() + "=" + first.getValue()
                + " rest=" + tm);

        // The other two range views write through as well.
        TreeMap<String, Integer> t2 = abcd();
        check(t2.tailMap("c").toString().equals("{c=3, d=4}"), "tailMap content: " + t2.tailMap("c"));
        check(t2.subMap("b", "d").toString().equals("{b=2, c=3}"),
                "subMap content: " + t2.subMap("b", "d"));
        check(t2.tailMap("c").remove("d") == 4, "tailMap.remove returns the mapping");
        check(t2.toString().equals("{a=1, b=2, c=3}"), "tailMap write-through: " + t2);
        t2.subMap("b", "d").clear();
        check(t2.toString().equals("{a=1}"), "subMap.clear write-through: " + t2);

        // NEGATIVE CONTROL. Without this, a "view" that simply forwarded every
        // call to the backing map -- ignoring its range entirely -- would
        // satisfy every write-through assertion above. A range view must REFUSE
        // a key outside its bounds.
        boolean outOfRange = false;
        try {
            abcd().headMap("c").put("z", 9);
        } catch (IllegalArgumentException expected) {
            outOfRange = true;
        }
        check(outOfRange, "headMap.put outside the view's range must throw IllegalArgumentException");
        // ... and the same view must accept a key INSIDE its bounds, or the
        // control above would be satisfied by a view that throws on every put.
        TreeMap<String, Integer> t3 = abcd();
        t3.headMap("c").put("bb", 7);
        check(t3.toString().equals("{a=1, b=2, bb=7, c=3, d=4}"),
                "headMap.put inside the range must reach the backing map: " + t3);
        System.out.println("CK RJdkViews rangeViews=" + t2 + " outOfRange=" + outOfRange
                + " inRange=" + t3);
    }

    // ---- family 2: Iterator.remove has a state machine ----------------------

    static void iteratorContract() {
        List<String> l = new ArrayList<>(Arrays.asList("a", "b", "c", "d"));
        Iterator<String> it = l.iterator();

        boolean ise = false;
        try {
            it.remove();
        } catch (IllegalStateException expected) {
            ise = true;
        }
        check(ise, "Iterator.remove() before next() must throw IllegalStateException");
        // The dangerous half. CratonVM did not throw AND removed the first
        // element; a VM that threw but had already mutated would be a half-fix,
        // so the state of the list is asserted independently of the throw.
        check(l.equals(Arrays.asList("a", "b", "c", "d")),
                "the rejected remove() must not have removed anything: " + l);
        check(l.size() == 4 && l.get(0).equals("a"), "list intact after the rejected remove: " + l);

        // The rejected remove() must not have disturbed the cursor either.
        check(it.next().equals("a"), "the iterator is still at position 0 after the rejected remove");
        it.remove();
        check(l.equals(Arrays.asList("b", "c", "d")), "next(); remove() removes exactly one: " + l);

        boolean ise2 = false;
        try {
            it.remove();
        } catch (IllegalStateException expected) {
            ise2 = true;
        }
        check(ise2, "a second remove() with no intervening next() must throw IllegalStateException");
        check(l.equals(Arrays.asList("b", "c", "d")),
                "the second rejected remove() changed nothing: " + l);
        System.out.println("CK RJdkViews iteratorRemove=" + l);

        // ListIterator.set + add on that same list -- the [B, B2, c, d] row.
        ListIterator<String> li = l.listIterator();
        check(li.nextIndex() == 0 && !li.hasPrevious(), "fresh ListIterator sits before element 0");
        check(li.next().equals("b"), "listIterator.next");
        li.set("B");
        check(l.equals(Arrays.asList("B", "c", "d")), "ListIterator.set replaces in place: " + l);
        li.add("B2");
        check(l.equals(Arrays.asList("B", "B2", "c", "d")),
                "ListIterator.add inserts before the cursor: " + l);
        check(l.size() == 4, "ListIterator.add must not drop an element: " + l);
        check(li.nextIndex() == 2, "the cursor advances past the added element: " + li.nextIndex());

        boolean setAfterAdd = false;
        try {
            li.set("X");
        } catch (IllegalStateException expected) {
            setAfterAdd = true;
        }
        check(setAfterAdd, "ListIterator.set() straight after add() must throw IllegalStateException");
        check(l.equals(Arrays.asList("B", "B2", "c", "d")),
                "the rejected set() changed nothing: " + l);

        // add() leaves next() unaffected and makes previous() answer the new
        // element -- the half of the contract a "just splice it in" shim gets
        // wrong, and the reason the broken VM was one element short.
        check(li.next().equals("c"), "next() is unaffected by add()");
        check(li.previous().equals("c") && li.previous().equals("B2"),
                "previous() walks back over the added element");
        System.out.println("CK RJdkViews listIterator=" + l);
    }

    // ---- family 2b: fail-fast, bounded ------------------------------------

    static void failFast() {
        // BOUNDED ON PURPOSE. This loop is terminated by the exception it is
        // asserting: without the cap, a VM that is not fail-fast grows the list
        // until the heap is gone, and the suite sees a 120 s timeout that is
        // indistinguishable from a VM hang. With the cap it is a named failure
        // in milliseconds.
        List<String> grow = new ArrayList<>(Arrays.asList("x", "y", "z"));
        boolean cme = false;
        int rounds = 0;
        try {
            for (String s : grow) {
                if (++rounds > CME_CAP) {
                    break;
                }
                grow.add(s);
            }
        } catch (ConcurrentModificationException expected) {
            cme = true;
        }
        check(cme, "for-each + List.add must raise ConcurrentModificationException"
                + " (none after " + rounds + " iterations, cap " + CME_CAP + ")");

        // The shrinking shape terminates on its own even without a CME, but is
        // capped for the same reason: a VM whose hasNext() is also wrong could
        // make it unbounded.
        List<String> shrink = new ArrayList<>(Arrays.asList("p", "q", "r", "s"));
        boolean cme2 = false;
        int rounds2 = 0;
        try {
            for (String s : shrink) {
                if (++rounds2 > CME_CAP) {
                    break;
                }
                shrink.remove(0);
            }
        } catch (ConcurrentModificationException expected) {
            cme2 = true;
        }
        check(cme2, "for-each + List.remove must raise ConcurrentModificationException"
                + " (none after " + rounds2 + " iterations, cap " + CME_CAP + ")");

        // NEGATIVE CONTROL. Without it, a VM that raised CME from every
        // structural change during iteration would satisfy both assertions
        // above while breaking the one mutation an iterator is allowed to make.
        List<Integer> ok = new ArrayList<>(Arrays.asList(1, 2, 3, 4, 5, 6));
        Iterator<Integer> oit = ok.iterator();
        while (oit.hasNext()) {
            if (oit.next() % 2 == 0) {
                oit.remove();
            }
        }
        check(ok.equals(Arrays.asList(1, 3, 5)),
                "Iterator.remove() is not a comodification: " + ok);
        System.out.println("CK RJdkViews failFast=" + cme + "," + cme2 + " kept=" + ok);
    }

    // ---- family 3: float formatting and StringBuilder bounds ---------------

    static void formats() {
        // Locale.ROOT on purpose. The defect is in the float CONVERSION -- the
        // broken VM printed Double.toString shapes (1.2345e3 for %e, 1.0E-4 for
        // %g) -- which is locale-independent, whereas the default locale's
        // decimal separator is a property of the host. Pinning the locale keeps
        // this a gate rather than a machine-specific flake.
        String s = String.format(Locale.ROOT, "%.3f|%e|%g", 1.0 / 3, 1234.5, 0.0001);
        check(s.equals("0.333|1.234500e+03|0.000100000"), "format floats: " + s);
        check(String.format(Locale.ROOT, "%e", 0.0).equals("0.000000e+00"),
                "%e of zero: " + String.format(Locale.ROOT, "%e", 0.0));
        check(String.format(Locale.ROOT, "%.3f", 2.0 / 3).equals("0.667"),
                "%.3f rounds HALF_UP: " + String.format(Locale.ROOT, "%.3f", 2.0 / 3));
        check(String.format(Locale.ROOT, "%g", 1234.5).equals("1234.50"),
                "%g stays decimal in range: " + String.format(Locale.ROOT, "%g", 1234.5));
        check(String.format(Locale.ROOT, "%g", 0.00001).equals("1.00000e-05"),
                "%g goes scientific below 1e-4: " + String.format(Locale.ROOT, "%g", 0.00001));
        check(String.format(Locale.ROOT, "%08.3f", -1.0 / 3).equals("-000.333"),
                "zero-padded negative: " + String.format(Locale.ROOT, "%08.3f", -1.0 / 3));
        check(String.format(Locale.ROOT, "%E", 1234.5).equals("1.234500E+03"),
                "%E is the upper-case form: " + String.format(Locale.ROOT, "%E", 1234.5));
        System.out.println("CK RJdkViews format=" + s);

        StringBuilder sb = new StringBuilder("ab");
        boolean threw = false;
        try {
            sb.delete(5, 6);
        } catch (StringIndexOutOfBoundsException expected) {
            threw = true;
        }
        check(threw, "new StringBuilder(\"ab\").delete(5, 6) must throw StringIndexOutOfBoundsException");
        check(sb.toString().equals("ab"), "the rejected delete must not have mutated: " + sb);

        boolean negThrew = false;
        try {
            new StringBuilder("abc").delete(-1, 1);
        } catch (StringIndexOutOfBoundsException expected) {
            negThrew = true;
        }
        check(negThrew, "delete(-1, 1) must throw StringIndexOutOfBoundsException");

        boolean revThrew = false;
        try {
            new StringBuilder("abc").delete(2, 1);
        } catch (StringIndexOutOfBoundsException expected) {
            revThrew = true;
        }
        check(revThrew, "delete(2, 1) (start > end) must throw StringIndexOutOfBoundsException");

        // NEGATIVE CONTROL. An end past the length is CLAMPED, not rejected, and
        // start == length is legal. Without these two, a VM that threw from
        // every delete() would satisfy all three assertions above.
        StringBuilder clamp = new StringBuilder("abc");
        clamp.delete(1, 99);
        check(clamp.toString().equals("a"), "delete(1, 99) clamps end to the length: " + clamp);
        StringBuilder noop = new StringBuilder("abc");
        noop.delete(3, 3);
        check(noop.toString().equals("abc"), "delete(3, 3) at the end is a legal no-op: " + noop);
        System.out.println("CK RJdkViews delete=" + sb + "," + clamp + "," + noop);
    }

    // ---- family 4: the stream surface that killed the run ------------------

    static void streams() {
        // On the broken VM this call did not produce a wrong answer -- it
        // terminated the process (SECTION-DIED.streamsSurface). Either way the
        // runner sees a non-zero rc / missing PASS line, so it is a gate.
        IntSummaryStatistics st = IntStream.rangeClosed(1, 5).summaryStatistics();
        check(st.getCount() == 5L, "summaryStatistics count: " + st.getCount());
        check(st.getSum() == 15L, "summaryStatistics sum: " + st.getSum());
        check(st.getMin() == 1, "summaryStatistics min: " + st.getMin());
        check(st.getMax() == 5, "summaryStatistics max: " + st.getMax());
        check(st.getAverage() == 3.0, "summaryStatistics average: " + st.getAverage());
        System.out.println("CK RJdkViews intStats=" + st.getCount() + "," + st.getSum() + ","
                + st.getMin() + "," + st.getMax() + "," + st.getAverage());

        // The empty case: min/max are the sentinel extremes and the average is
        // 0.0, not NaN. A stub returning a zeroed struct would answer min=0.
        IntSummaryStatistics empty = IntStream.of().summaryStatistics();
        check(empty.getCount() == 0L && empty.getSum() == 0L,
                "empty summaryStatistics count/sum: " + empty.getCount() + "/" + empty.getSum());
        check(empty.getMin() == Integer.MAX_VALUE, "empty min sentinel: " + empty.getMin());
        check(empty.getMax() == Integer.MIN_VALUE, "empty max sentinel: " + empty.getMax());
        check(empty.getAverage() == 0.0, "empty average: " + empty.getAverage());
        System.out.println("CK RJdkViews emptyStats=" + empty.getMin() + "," + empty.getMax()
                + "," + empty.getAverage());
    }

    public static void main(String[] args) {
        navigableViews();
        iteratorContract();
        failFast();
        formats();
        // streams() last on purpose: the summaryStatistics defect killed the
        // process outright, so anything after it would report nothing.
        streams();
        System.out.println("CK RJdkViews checks=" + checks);
        System.out.println("PASS RJdkViews (" + checks + " checks)");
    }
}
