// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.ConcurrentModificationException;
import java.util.DoubleSummaryStatistics;
import java.util.IntSummaryStatistics;
import java.util.Iterator;
import java.util.List;
import java.util.ListIterator;
import java.util.Locale;
import java.util.Map;
import java.util.NavigableMap;
import java.util.NavigableSet;
import java.util.OptionalDouble;
import java.util.OptionalLong;
import java.util.SortedMap;
import java.util.SortedSet;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.stream.BaseStream;
import java.util.stream.IntStream;
import java.util.stream.LongStream;

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

    // ---- family 4b: the rest of the primitive-stream surface ---------------

    /**
     * W7-2 §7.2 -- the members of {@code IntStream}/{@code LongStream}/
     * {@code DoubleStream} that were DECLARED and never REGISTERED. On a
     * CratonVM synthetic primitive stream the receiver's runtime class is the
     * INTERFACE, so an unregistered member resolves to the abstract declaration
     * and the call dies with {@code AbstractMethodError: ... has no Code
     * attribute} -- the same death {@code summaryStatistics()} died in
     * streams() above, which is why this section sits beside it.
     *
     * EVERY stream below must be SYNTHETIC or this section measures nothing.
     * {@code IntStream.of(int...)} and {@code Arrays.stream} return a real
     * {@code IntPipeline$Head} whose own bytecode implements all of this; only
     * {@code range}/{@code rangeClosed} and the intermediate ops built on them
     * (map / filter / asDoubleStream / mapToDouble) mint the interface-stamped
     * object this record is about. That is the difference between a gate and a
     * test of the real JDK.
     */
    static void primitiveStreamSurface() {
        // IntStream.distinct: 1,2,0,1,2 -> first occurrence of each.
        int[] id = IntStream.rangeClosed(1, 5).map(i -> i % 3).distinct().toArray();
        check(Arrays.toString(id).equals("[1, 2, 0]"), "IntStream.distinct: " + Arrays.toString(id));

        // LongStream: sorted / distinct / findFirst / findAny were all missing.
        long[] ls = LongStream.range(0, 5).map(x -> x % 3).sorted().toArray();
        check(Arrays.toString(ls).equals("[0, 0, 1, 1, 2]"), "LongStream.sorted: " + Arrays.toString(ls));
        long[] ld = LongStream.range(0, 5).map(x -> x % 3).distinct().toArray();
        check(Arrays.toString(ld).equals("[0, 1, 2]"), "LongStream.distinct: " + Arrays.toString(ld));
        OptionalLong lf = LongStream.range(0, 5).filter(x -> x > 2).findFirst();
        check(lf.isPresent() && lf.getAsLong() == 3L, "LongStream.findFirst: " + lf);
        OptionalLong la = LongStream.range(0, 5).filter(x -> x > 2).findAny();
        check(la.isPresent() && la.getAsLong() == 3L, "LongStream.findAny: " + la);
        // ... and both must be EMPTY when nothing survives the filter, or a
        // "return the first element unconditionally" stub would pass above.
        check(LongStream.range(0, 5).filter(x -> x > 99).findFirst().isEmpty(),
                "LongStream.findFirst on an empty stream must be empty");
        System.out.println("CK RJdkViews longStream=" + Arrays.toString(ls) + Arrays.toString(ld)
                + " first=" + lf.getAsLong());

        // DoubleStream -- the largest of the three holes. anyMatch is the
        // conspicuous one: allMatch and noneMatch beside it were registered.
        check(IntStream.rangeClosed(1, 4).asDoubleStream().anyMatch(x -> x == 3.0),
                "DoubleStream.anyMatch positive");
        check(!IntStream.rangeClosed(1, 4).asDoubleStream().anyMatch(x -> x > 9.0),
                "DoubleStream.anyMatch negative");
        double seeded = IntStream.rangeClosed(1, 4).asDoubleStream().reduce(0.0, Double::sum);
        check(seeded == 10.0, "DoubleStream.reduce(seed, op): " + seeded);
        OptionalDouble red = IntStream.rangeClosed(1, 4).asDoubleStream().reduce(Double::sum);
        check(red.isPresent() && red.getAsDouble() == 10.0, "DoubleStream.reduce(op): " + red);
        // The no-identity overload must answer EMPTY on an empty stream -- the
        // whole reason it returns OptionalDouble rather than 0.0.
        check(IntStream.rangeClosed(1, 0).asDoubleStream().reduce(Double::sum).isEmpty(),
                "DoubleStream.reduce(op) on an empty stream must be empty");
        OptionalDouble df = IntStream.rangeClosed(1, 4).asDoubleStream().findFirst();
        check(df.isPresent() && df.getAsDouble() == 1.0, "DoubleStream.findFirst: " + df);
        check(IntStream.rangeClosed(1, 4).asDoubleStream().findAny().isPresent(),
                "DoubleStream.findAny");
        double[] dsorted = IntStream.rangeClosed(1, 4).map(i -> 5 - i)
                .asDoubleStream().sorted().toArray();
        check(Arrays.toString(dsorted).equals("[1.0, 2.0, 3.0, 4.0]"),
                "DoubleStream.sorted: " + Arrays.toString(dsorted));
        double[] ddist = IntStream.rangeClosed(1, 4).map(i -> i % 2)
                .asDoubleStream().distinct().toArray();
        check(Arrays.toString(ddist).equals("[1.0, 0.0]"),
                "DoubleStream.distinct: " + Arrays.toString(ddist));

        // The two rows that separate Java's double ordering from Rust's (and
        // from `==`): Double.compare puts -0.0 strictly BELOW 0.0, and
        // Double.equals keeps them distinct. An implementation built on `<`
        // and `==` passes everything above and fails both of these.
        double[] zsorted = IntStream.rangeClosed(1, 2)
                .mapToDouble(i -> i == 1 ? 0.0 : -0.0).sorted().toArray();
        check(Arrays.toString(zsorted).equals("[-0.0, 0.0]"),
                "DoubleStream.sorted orders -0.0 below 0.0: " + Arrays.toString(zsorted));
        double[] zdist = IntStream.rangeClosed(1, 2)
                .mapToDouble(i -> i == 1 ? 0.0 : -0.0).distinct().toArray();
        check(Arrays.toString(zdist).equals("[0.0, -0.0]"),
                "DoubleStream.distinct keeps -0.0 and 0.0 apart: " + Arrays.toString(zdist));
        System.out.println("CK RJdkViews doubleStream=" + Arrays.toString(dsorted)
                + Arrays.toString(ddist) + Arrays.toString(zsorted));

        // `BaseStream.iterator()` -- the ()Ljava/util/Iterator; bridge, which is
        // a DIFFERENT registration from IntStream.iterator()OfInt and is the one
        // reached whenever the static type is BaseStream/Stream. Its backing
        // store on a primitive stream is a primitive array, so the elements have
        // to be BOXED on the way out: Iterator.next() is declared to return a
        // reference, and an unboxed word there is untyped, not merely wrong.
        // Bounded by `guard` for the reason failFast() is bounded.
        BaseStream<?, ?> bs = IntStream.rangeClosed(1, 3);
        Iterator<?> bit = bs.iterator();
        int isum = 0;
        int seen = 0;
        int guard = 0;
        while (bit.hasNext() && ++guard <= 8) {
            Object o = bit.next();
            seen++;
            check(o instanceof Integer, "BaseStream.iterator() must yield boxed Integers"
                    + " (element " + seen + " was not an Integer)");
            isum += ((Integer) o).intValue();
        }
        check(seen == 3 && isum == 6,
                "BaseStream.iterator() over an IntStream: seen=" + seen + " sum=" + isum);
        System.out.println("CK RJdkViews baseStreamIterator=" + seen + "," + isum);
    }

    // Raw bit patterns of the two zeros. `-0.0 == 0.0` is TRUE in Java, so an
    // equality-shaped check on a signed zero passes against an implementation
    // that loses the sign; only the bits can see it.
    private static final long NEG_ZERO_BITS = 0x8000000000000000L;
    private static final long POS_ZERO_BITS = 0x0000000000000000L;

    /**
     * {@code DoubleSummaryStatistics} on the two inputs a {@code <}-based
     * min/max cannot represent: {@code NaN} and the sign of zero.
     *
     * <p>The accumulator is specified in terms of {@code Math.min}/{@code
     * Math.max} ("returns... {@code Double.NaN} if any recorded value was
     * NaN"), so it inherits their contract exactly. Measured on the 2026-08-12
     * pre-fix binary in BOTH modes, after {@code accept(1.0)},
     * {@code accept(NaN)}, {@code accept(3.0)}:
     *
     * <pre>
     *   count=3  min=1.0  max=3.0  sum=NaN
     * </pre>
     *
     * Three fields that cannot have come from the same three values — the sum
     * saw the NaN and the extrema did not. That is the shape this block exists
     * to catch, and it is why {@code getSum} is asserted BESIDE the extrema
     * rather than instead of them: a reader seeing only {@code sum=NaN} would
     * conclude the NaN was recorded.
     *
     * <p>Both zero rows are asserted BY RAW BITS and in BOTH accept orders.
     * The order matters: a body built on {@code a < b} returns whichever
     * operand the comparison leaves standing, so it can be right one way round
     * and wrong the other. Measured pre-fix, it was exactly that — with
     * {@code accept(-0.0)} then {@code accept(0.0)} the minimum came back
     * {@code +0.0}, and with the accepts reversed the maximum came back
     * {@code -0.0}.
     *
     * <p>The last four rows take the same statistics through a SYNTHETIC
     * stream instead of {@code accept}. They were already green pre-fix, which
     * is the finding: {@code DoubleStream.summaryStatistics()} computes its
     * extrema by a different route than {@code DoubleSummaryStatistics.accept}
     * does, so neither covers the other and a fix to one leaves the other
     * free to drift.
     */
    static void doubleSummaryStatisticsSpecialValues() {
        DoubleSummaryStatistics poisoned = new DoubleSummaryStatistics();
        poisoned.accept(1.0);
        poisoned.accept(Double.NaN);
        poisoned.accept(3.0);
        check(poisoned.getCount() == 3, "DoubleSummaryStatistics must record all three values");
        check(Double.isNaN(poisoned.getMin()),
                "DoubleSummaryStatistics.getMin must be NaN once NaN was accepted, was "
                        + poisoned.getMin());
        check(Double.isNaN(poisoned.getMax()),
                "DoubleSummaryStatistics.getMax must be NaN once NaN was accepted, was "
                        + poisoned.getMax());
        check(Double.isNaN(poisoned.getSum()),
                "DoubleSummaryStatistics.getSum must be NaN once NaN was accepted");
        check(Double.isNaN(poisoned.getAverage()),
                "DoubleSummaryStatistics.getAverage must be NaN once NaN was accepted");

        DoubleSummaryStatistics negFirst = new DoubleSummaryStatistics();
        negFirst.accept(-0.0);
        negFirst.accept(0.0);
        check(Double.doubleToRawLongBits(negFirst.getMin()) == NEG_ZERO_BITS,
                "getMin over {-0.0, 0.0} must be -0.0 (bits 0x8000000000000000)");
        check(Double.doubleToRawLongBits(negFirst.getMax()) == POS_ZERO_BITS,
                "getMax over {-0.0, 0.0} must be +0.0 (bits 0x0)");

        DoubleSummaryStatistics posFirst = new DoubleSummaryStatistics();
        posFirst.accept(0.0);
        posFirst.accept(-0.0);
        check(Double.doubleToRawLongBits(posFirst.getMin()) == NEG_ZERO_BITS,
                "getMin over {0.0, -0.0} must be -0.0 (bits 0x8000000000000000)");
        check(Double.doubleToRawLongBits(posFirst.getMax()) == POS_ZERO_BITS,
                "getMax over {0.0, -0.0} must be +0.0 (bits 0x0)");

        // The empty seeds, which are what the accumulator's first Math.min /
        // Math.max are applied to. An implementation that seeds from the first
        // accepted value instead passes everything above and fails here.
        DoubleSummaryStatistics empty = new DoubleSummaryStatistics();
        check(Double.doubleToRawLongBits(empty.getMin())
                        == Double.doubleToRawLongBits(Double.POSITIVE_INFINITY),
                "empty DoubleSummaryStatistics.getMin must be +Infinity");
        check(Double.doubleToRawLongBits(empty.getMax())
                        == Double.doubleToRawLongBits(Double.NEGATIVE_INFINITY),
                "empty DoubleSummaryStatistics.getMax must be -Infinity");

        // Same contract, reached through a synthetic DoubleStream. `mapToDouble`
        // on `rangeClosed` mints the interface-stamped stream this section is
        // about; `DoubleStream.of` would hand back a real IntPipeline$Head and
        // measure the JDK.
        DoubleSummaryStatistics streamed = IntStream.rangeClosed(1, 3)
                .mapToDouble(i -> i == 2 ? Double.NaN : (double) i).summaryStatistics();
        check(streamed.getCount() == 3, "DoubleStream.summaryStatistics must record three values");
        check(Double.isNaN(streamed.getMin()),
                "DoubleStream.summaryStatistics().getMin must be NaN, was " + streamed.getMin());
        check(Double.isNaN(streamed.getMax()),
                "DoubleStream.summaryStatistics().getMax must be NaN, was " + streamed.getMax());
        DoubleSummaryStatistics streamedZeros = IntStream.rangeClosed(1, 2)
                .mapToDouble(i -> i == 1 ? -0.0 : 0.0).summaryStatistics();
        check(Double.doubleToRawLongBits(streamedZeros.getMin()) == NEG_ZERO_BITS,
                "DoubleStream.summaryStatistics().getMin over {-0.0, 0.0} must be -0.0");
        check(Double.doubleToRawLongBits(streamedZeros.getMax()) == POS_ZERO_BITS,
                "DoubleStream.summaryStatistics().getMax over {-0.0, 0.0} must be +0.0");

        System.out.println("CK RJdkViews doubleStats=15");
    }

    // ---- W7-36 residuals: refusals the sorted containers never made --------

    /**
     * The natural-ordering key check. {@code TreeMap.getEntry} runs
     * {@code if (key == null) throw new NullPointerException();} and the
     * {@code (Comparable) key} checkcast BEFORE it looks at {@code root}, so an
     * EMPTY container refuses too -- and {@code NavigableSubMap}'s constructor
     * runs {@code m.compare(hi, hi)} for a single-bound view for the same
     * reason. CratonVM returned normally from all of it.
     *
     * Each refusal is paired with the case that must NOT refuse, because a
     * container that threw from every key would satisfy the positive half on
     * its own -- the W6-5 shape.
     */
    static void sortedContainerRefusals() {
        boolean ck = false;
        try {
            new TreeMap<String, Integer>().containsKey(null);
        } catch (NullPointerException expected) {
            ck = true;
        }
        check(ck, "TreeMap.containsKey(null) under natural ordering must throw NullPointerException");

        boolean rm = false;
        try {
            new TreeMap<String, Integer>().remove(null);
        } catch (NullPointerException expected) {
            rm = true;
        }
        check(rm, "TreeMap.remove(null) under natural ordering must throw NullPointerException");

        boolean god = false;
        try {
            new TreeMap<String, Integer>().getOrDefault(null, 7);
        } catch (NullPointerException expected) {
            god = true;
        }
        check(god, "TreeMap.getOrDefault(null, d) must throw NullPointerException, not answer d");

        boolean tsc = false;
        try {
            new TreeSet<String>().contains(null);
        } catch (NullPointerException expected) {
            tsc = true;
        }
        check(tsc, "TreeSet.contains(null) under natural ordering must throw NullPointerException");

        // The type half of the same check, on an EMPTY container -- where no
        // comparison happens and so nothing raised it before.
        boolean cce = false;
        try {
            new TreeMap<Object, Integer>().containsKey(new Object());
        } catch (ClassCastException expected) {
            cce = true;
        }
        check(cce, "TreeMap.containsKey(non-Comparable) must throw ClassCastException");

        // Single-bound views: NavigableSubMap's `else` arm, a type-and-null
        // check on the one bound. Not the reversed-bounds check, which only the
        // two-bound entry points reach.
        boolean hm = false;
        try {
            abcd().headMap(null);
        } catch (NullPointerException expected) {
            hm = true;
        }
        check(hm, "TreeMap.headMap(null) must throw NullPointerException");
        boolean tmn = false;
        try {
            abcd().tailMap(null);
        } catch (NullPointerException expected) {
            tmn = true;
        }
        check(tmn, "TreeMap.tailMap(null) must throw NullPointerException");

        // TreeSet.subSet(hi, lo) -- the TreeMap twin was fixed and this was not.
        TreeSet<String> ts = new TreeSet<>(Arrays.asList("a", "b", "c"));
        // Asserted so a construction failure is attributed here rather than
        // showing up as an empty subSet two lines down.
        check(ts.size() == 3 && ts.first().equals("a"), "TreeSet(Collection) populated: " + ts);
        boolean rev = false;
        try {
            ts.subSet("c", "a");
        } catch (IllegalArgumentException expected) {
            rev = true;
        }
        check(rev, "TreeSet.subSet(hi, lo) must throw IllegalArgumentException");

        // NEGATIVE CONTROLS.
        // 1. A comparator that permits nulls legitimately holds them, and the
        //    JDK scopes the whole check to `comparator == null`. A VM that
        //    refused here would break `new TreeSet<>(nullsFirst(..))`.
        Comparator<String> nullsFirst = (a, b) -> a == null
                ? (b == null ? 0 : -1)
                : (b == null ? 1 : a.compareTo(b));
        TreeMap<String, Integer> nullOk = new TreeMap<>(nullsFirst);
        nullOk.put("a", 1);
        check(!nullOk.containsKey(null), "a null-permitting comparator must not refuse containsKey(null)");
        check(nullOk.remove(null) == null, "a null-permitting comparator must not refuse remove(null)");
        check(nullOk.getOrDefault(null, 7) == 7,
                "a null-permitting comparator must not refuse getOrDefault(null, d)");
        // 2. The ordinary cases still answer.
        check(!new TreeMap<String, Integer>().containsKey("nope"), "absent key is still absent");
        check(abcd().headMap("c").size() == 2, "a valid single bound still builds the view");
        SortedSet<String> okSub = ts.subSet("a", "c");
        check(okSub.toString().equals("[a, b]"), "a valid subSet is unaffected: " + okSub);
        // 3. getOrDefault over a PRESENT null value answers null, not the
        //    default -- W7-36 suspected this was broken here and it was not;
        //    the assertion locks the correction rather than a change.
        TreeMap<String, Integer> withNull = new TreeMap<>();
        withNull.put("k", null);
        check(withNull.getOrDefault("k", 7) == null,
                "getOrDefault over a present null VALUE answers null, not the default");
        System.out.println("CK RJdkViews refusals=" + ck + rm + god + tsc + cce + hm + tmn + rev
                + " sub=" + okSub);
    }

    // ---- W7-1 residual: the map key iterator past its end ------------------

    static void keyIteratorExhaustion() {
        // `HashMap$KeyItr.next()` past the end answered null, where the JDK
        // throws. null is also a legitimate ELEMENT of a key set, so a caller
        // that over-ran its own hasNext() could not tell the two apart.
        Iterator<String> it = new java.util.HashSet<>(Arrays.asList("only")).iterator();
        check(it.next().equals("only"), "the one element comes out");
        check(!it.hasNext(), "and the iterator is then exhausted");
        boolean nse = false;
        try {
            it.next();
        } catch (java.util.NoSuchElementException expected) {
            nse = true;
        }
        check(nse, "Iterator.next() past the end must throw NoSuchElementException");
        System.out.println("CK RJdkViews keyItrExhausted=" + nse);
    }

    // ---- W7-1 residual: an entrySet() view must stay an ENTRY set ----------

    /**
     * A `MAP_VIEW_CARRIERS` carrier's KIND — entries or values — was inferred
     * from the class of its head element, and that inference defaulted to
     * "values" whenever it could not classify one. `TreeMap$EntrySet` is the one
     * entry-shaped member of that list, so an entrySet the guess could not read
     * came back holding the map's VALUES: `Map.Entry` elements silently replaced
     * by `V` objects, which a caller only notices at its first cast.
     *
     * Two ordinary ways to reach the unreadable case, one per method below.
     * Both are asserted on CONTENT, never on `getClass()`: this VM's entries are
     * `AbstractMap$SimpleEntry` where HotSpot's are `TreeMap$Entry`, which is a
     * separate (and deliberate) divergence, and a class-name assertion here
     * would fail the cross-VM diff for the wrong reason.
     */
    static void entrySetStaysEntries() {
        // 1. The view was EMPTY when the carrier was minted, so there was no
        //    head element to read. No GC, no timing: `entrySet()` held across a
        //    `put` answered `[1, 2]` where HotSpot answers `[a=1, b=2]`.
        TreeMap<String, Integer> late = new TreeMap<>();
        Map<String, Integer> lateMapView = late;
        java.util.Set<Map.Entry<String, Integer>> es = lateMapView.entrySet();
        check(es.isEmpty(), "an entrySet of an empty map is empty: " + es);
        late.put("a", 1);
        late.put("b", 2);
        check(es.size() == 2, "the live entrySet tracks the backing map: " + es.size());
        Iterator<Map.Entry<String, Integer>> lateItr = es.iterator();
        Object lateHead = lateItr.next();
        check(lateHead instanceof Map.Entry,
                "an entrySet taken while the map was EMPTY still yields Map.Entry, not values");
        // Wildcard cast: the element type is not what is under test here, and an
        // unchecked cast would be the only warning javac emits for this file.
        Map.Entry<?, ?> lateEntry = (Map.Entry<?, ?>) lateHead;
        check(lateEntry.getKey().equals("a") && lateEntry.getValue().equals(1),
                "and the entry carries its key: " + lateEntry.getKey() + "=" + lateEntry.getValue());
        check(es.toString().equals("[a=1, b=2]"), "empty-then-populated entrySet: " + es);
        System.out.println("CK RJdkViews lateEntrySet=" + es);

        // 2. A RANGE view's entrySet. `RTreeRangeGc` reddened here on the
        //    default collector: `headMap(k, false).entrySet()` handed back the
        //    view's 300 values and the vector's checkcast to Map.Entry failed.
        //    The kind must not depend on whether a head element can be read, so
        //    assert every element rather than the first.
        TreeMap<String, Integer> src = abcd();
        NavigableMap<String, Integer> range = src.headMap("c", false);
        int seen = 0;
        StringBuilder rendered = new StringBuilder();
        for (Map.Entry<String, Integer> e : range.entrySet()) {
            check(e.getKey() != null, "range entrySet element " + seen + " has a key");
            check(e.getValue() != null, "range entrySet element " + seen + " has a value");
            if (seen > 0) {
                rendered.append(',');
            }
            rendered.append(e.getKey()).append('=').append(e.getValue());
            seen++;
        }
        check(seen == 2, "headMap(k,false).entrySet() yields both entries: " + seen);
        check(rendered.toString().equals("a=1,b=2"), "range entrySet content: " + rendered);
        System.out.println("CK RJdkViews rangeEntrySet=" + rendered);

        // The kind decides equals/hashCode too. `TreeMap$EntrySet`'s real
        // superclass is AbstractSet, which DOES override both, so two equal maps
        // have EQUAL entry sets -- where a values view (AbstractCollection,
        // which overrides neither) compares by identity. Reading the carrier as
        // a values view got this backwards.
        TreeMap<String, Integer> twin = abcd();
        check(src.entrySet().equals(twin.entrySet()),
                "equal maps have equal entry sets");
        check(src.entrySet().hashCode() == twin.entrySet().hashCode(),
                "and equal entry sets hash alike");
        check(!src.values().equals(twin.values()),
                "while values() keeps AbstractCollection's identity equality");
        System.out.println("CK RJdkViews entrySetEquality="
                + src.entrySet().equals(twin.entrySet()) + "/" + src.values().equals(twin.values()));
    }

    public static void main(String[] args) {
        navigableViews();
        entrySetStaysEntries();
        iteratorContract();
        failFast();
        formats();
        sortedContainerRefusals();
        keyIteratorExhaustion();
        // streams() last on purpose: the summaryStatistics defect killed the
        // process outright, so anything after it would report nothing. The rest
        // of the primitive-stream surface fails the same way, so it goes with it.
        streams();
        primitiveStreamSurface();
        doubleSummaryStatisticsSpecialValues();
        System.out.println("CK RJdkViews checks=" + checks);
        System.out.println("PASS RJdkViews (" + checks + " checks)");
    }
}
