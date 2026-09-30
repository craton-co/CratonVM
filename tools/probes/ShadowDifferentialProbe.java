import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.RoundingMode;
import java.text.DecimalFormat;
import java.text.DecimalFormatSymbols;
import java.text.MessageFormat;
import java.text.NumberFormat;
import java.text.SimpleDateFormat;
import java.time.Duration;
import java.time.Instant;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.LocalTime;
import java.time.Period;
import java.time.YearMonth;
import java.time.ZoneOffset;
import java.time.ZonedDateTime;
import java.time.format.DateTimeFormatter;
import java.time.temporal.ChronoUnit;
import java.util.*;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.atomic.LongAdder;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * A shadow's disposition is decided by whether it diverges from the bytecode it
 * shadows.
 *
 * The census counts registrations standing in front of concrete JDK code
 * (contract §1.4 permits it), but counting them says nothing about whether any
 * of them is WRONG. This exercises the most-reached shadowed surface —
 * `java.util`'s immutable factories, `Map.entry`, and the views they hand out —
 * and prints every observable in `key=value` form so the whole run diffs
 * byte-for-byte against real HotSpot.
 *
 * Anything that differs is a shadow to fix or delete. Anything that matches is
 * a shadow with evidence behind it, which is what "adjudicated" has to mean for
 * a population this size.
 */
public class ShadowDifferentialProbe {

    // =======================================================================
    // THE OBSERVABLE LEDGER
    // =======================================================================
    //
    // W7-42. Two of the fourteen residual "divergences" on this probe were
    // holes in the probe, not in the VM. Five observables
    // (`ArrayDeque.addNull`, `addFirstNull`, `offerNull`,
    // `sizeAfterRefusedNulls`, `COW.addAllAbsent`) were present on the HotSpot
    // side and simply ABSENT on the CratonVM side — not a different value, no
    // line at all — while the enclosing sections ran to completion and the
    // fence never fired. The cause is recorded in
    // W7-42-differential-instrument-holes.md: the two sides were not running
    // the same class file. W7-33 excised those five statements into a
    // scratchpad copy so the dead sections could be measured on an unfixed
    // binary, and the next run compiled HotSpot from the tree and CratonVM
    // from the copy. Nothing threw, so nothing could be reported.
    //
    // A missing line is the worst possible failure for a differential: an
    // absent row reads as agreement everywhere a reader scans for `<`/`>`
    // PAIRS, and the fence — which exists to turn truncation into one marker
    // — is blind to it by construction, because the fence can only see
    // throws.
    //
    // So the probe no longer relies on control flow reaching a `line(..)`
    // call. Every section DECLARES the observables it owes (`manifest()`
    // below), `line(..)` ticks them off, and the end of the section prints
    // `MISSING-OBSERVABLE=<name>` for every one still owed. The declaration
    // and the statement are separate text, which is the whole point: deleting
    // the statement — or running a side whose source never had it — leaves
    // the declaration behind to accuse it.
    //
    // The ledger is deliberately built out of `String[]`, `boolean[]` and
    // `String.equals` only. This probe exists to test `java.util`; an
    // instrument that stores its own bookkeeping in a `LinkedHashSet` is a
    // variable of the comparison it is running, and would report a clean run
    // whenever the collection it depends on is the thing that is broken.

    /** Section names, in registration order. */
    static final String[] SEC_NAME = new String[64];
    /** Observables each section owes, parallel to {@link #SEC_NAME}. */
    static final String[][] SEC_KEYS = new String[64][];
    /** Whether each section was actually entered. */
    static final boolean[] SEC_RAN = new boolean[64];
    static int secCount = 0;

    /** Index into {@link #SEC_NAME} of the section currently running, or -1. */
    static int curSec = -1;
    /** Per-key tick marks for the running section, parallel to its key array. */
    static boolean[] curSeen = null;

    /** Every key emitted so far, for cross-section duplicate detection. */
    static final String[] EMITTED = new String[4096];
    static int emittedCount = 0;

    static int missingCount = 0;
    static int undeclaredCount = 0;
    static int duplicateCount = 0;
    static int multilineCount = 0;
    static int unrenderableCount = 0;

    /** Declare the observables one section owes. Called only from {@link #manifest()}. */
    static void declare(String section, String... names) {
        if (secCount >= SEC_NAME.length) {
            System.out.println("MANIFEST-OVERFLOW=" + section);
            return;
        }
        SEC_NAME[secCount] = section;
        SEC_KEYS[secCount] = names;
        secCount++;
    }

    static int sectionIndex(String name) {
        for (int i = 0; i < secCount; i++) {
            if (SEC_NAME[i].equals(name)) {
                return i;
            }
        }
        return -1;
    }

    /**
     * One observable is ONE transcript line, always, and every emitted key is
     * reconciled against the manifest.
     *
     * Three things happen here that did not before, each closing a way a row
     * could go missing or a foreign row could be mistaken for one:
     *
     *   1. A `toString` that throws no longer takes the rest of the section
     *      with it. It prints `<toString-threw:Type>` — a value, which diffs —
     *      instead of unwinding to the fence and turning ~40 later observables
     *      into absence.
     *   2. A value containing a newline is ESCAPED. An un-escaped one splits a
     *      single observable across several transcript rows and shifts the
     *      alignment of every row after it, which is precisely the corruption
     *      the leaked `[SUREFIRE-NPE]` frames caused on the CratonVM side.
     *   3. The key is ticked off the running section's manifest, and reported
     *      if it was never declared or has already been emitted.
     */
    static void line(String k, Object v) {
        String s;
        try {
            s = String.valueOf(v);
        } catch (Throwable t) {
            s = "<toString-threw:" + t.getClass().getName() + ">";
            unrenderableCount++;
        }
        if (s == null) {
            s = "null";
        }
        if (s.indexOf('\n') >= 0 || s.indexOf('\r') >= 0) {
            multilineCount++;
            s = s.replace("\r", "\\r").replace("\n", "\\n");
        }
        record(k);
        System.out.println(k + "=" + s);
    }

    /** Reconcile one emitted key against the manifest. Prints only when wrong. */
    static void record(String k) {
        for (int i = 0; i < emittedCount; i++) {
            if (EMITTED[i].equals(k)) {
                duplicateCount++;
                // A key emitted twice cannot be checked off by a set, so a LOST
                // second occurrence would be invisible to this ledger. Zero
                // today (measured against the 859-line HotSpot transcript);
                // this is the ratchet that keeps it zero.
                System.out.println("DUPLICATE-OBSERVABLE=" + k);
                break;
            }
        }
        if (emittedCount < EMITTED.length) {
            EMITTED[emittedCount++] = k;
        }
        if (curSec < 0) {
            undeclaredCount++;
            System.out.println("UNDECLARED-OBSERVABLE=" + k + " outside-any-section");
            return;
        }
        String[] names = SEC_KEYS[curSec];
        for (int i = 0; i < names.length; i++) {
            if (names[i].equals(k)) {
                curSeen[i] = true;
                return;
            }
        }
        // An added `line(..)` with no matching `declare(..)` entry would
        // re-open the hole for that row: nothing would notice it going away
        // again. Reported so the manifest cannot silently fall behind.
        undeclaredCount++;
        System.out.println("UNDECLARED-OBSERVABLE=" + k + " section:" + SEC_NAME[curSec]);
    }

    /** Same shape as the JDK's own contract tests: value, type, and identity. */
    static void entryLike(String tag, Map.Entry<?, ?> e) {
        line(tag + ".key", e.getKey());
        line(tag + ".value", e.getValue());
        line(tag + ".toString", e.toString());
        line(tag + ".hashCode.matchesSpec",
                e.hashCode() == (Objects.hashCode(e.getKey()) ^ Objects.hashCode(e.getValue())));
        line(tag + ".equalsSelf", e.equals(e));
        line(tag + ".equalsCopy", e.equals(Map.entry(e.getKey(), e.getValue())));
    }

    public static void main(String[] args) {
        manifest();
        // The 63 observables below used to run INLINE in `main`, outside any
        // section — so they were both unfenced (a throw there truncated the
        // whole transcript with no `SECTION-DIED` marker) and unledgered.
        // They are a section like every other now.
        section("factoriesAndViews", ShadowDifferentialProbe::factoriesAndViews);
        // --- Map.entry --------------------------------------------------
        // (moved into `factoriesAndViews` below)
        section("orderedMaps", ShadowDifferentialProbe::orderedMaps);
        section("deques", ShadowDifferentialProbe::deques);
        section("iteratorContracts", ShadowDifferentialProbe::iteratorContracts);
        section("arraysFamily", ShadowDifferentialProbe::arraysFamily);
        section("collectionsAlgorithms", ShadowDifferentialProbe::collectionsAlgorithms);
        section("optionalAndObjects", ShadowDifferentialProbe::optionalAndObjects);
        section("stringSurface", ShadowDifferentialProbe::stringSurface);
        section("boxesAndParsing", ShadowDifferentialProbe::boxesAndParsing);
        section("joinersAndBuilders", ShadowDifferentialProbe::joinersAndBuilders);
        section("streamsSurface", ShadowDifferentialProbe::streamsSurface);
        section("comparators", ShadowDifferentialProbe::comparators);
        section("bitSetAndUuidAndBase64", ShadowDifferentialProbe::bitSetAndUuidAndBase64);

        // ROUND 2, added 2026-08-11. The sections above found four defect
        // families on their first run. A hit rate like that is not a verdict
        // on those twelve families, it is a verdict on the SAMPLE: the
        // shadowed surface is under-observed, so the highest-yield thing
        // available is another widening. The fifteen below go at families
        // round 1 did not reach at all, picked for one property — a native
        // stands in front of real JDK bytecode, and a wrong answer there is
        // QUIET. An empty view, a stored null where the spec says remove, a
        // `false` from a bounded queue that grew anyway, a no-throw where the
        // spec mandates a throw: none of those announce themselves, and all
        // of them read as a pass to a caller that only iterates.
        //
        // FOUR disciplines hold for every one of them:
        //   1. FENCED — `section(..)` turns a throw into one `SECTION-DIED.x`
        //      line rather than deleting the rest of the transcript.
        //   2. DECLARED — every observable is named in `manifest()`, and one
        //      that does not get emitted prints `MISSING-OBSERVABLE=<name>`.
        //      The fence only sees throws; a line lost any other way (a
        //      skipped branch, a side compiled from different source) is
        //      invisible to it, and that is what actually happened — see
        //      W7-42-differential-instrument-holes.md.
        //   3. BOUNDED — nothing here relies on an exception, or on a
        //      collection shrinking, to terminate. Every drain, every
        //      `Matcher.find` loop, every iterate-and-mutate carries a guard
        //      and reports `...-after-100` rather than hanging.
        //   4. VALUES, NOT VERDICTS — every observable prints its actual
        //      content. `probes/JdkOnlyCollectionViewProbe` exists because an
        //      empty view reads as a pass everywhere a caller only iterates;
        //      a line that prints `ok` cannot diff, a line that prints
        //      `{d=4, c=3}` can. Where a message is the observable (checked
        //      collections, helpful NPEs, `valueOf` on a bad enum constant)
        //      the message is printed too, via `thrownDetail`.
        section("mapDefaults", ShadowDifferentialProbe::mapDefaults);
        section("mapViewWriteThrough", ShadowDifferentialProbe::mapViewWriteThrough);
        section("subListContracts", ShadowDifferentialProbe::subListContracts);
        section("wrapperViews", ShadowDifferentialProbe::wrapperViews);
        section("navigableEdges", ShadowDifferentialProbe::navigableEdges);
        section("dequeEdges", ShadowDifferentialProbe::dequeEdges);
        section("enumCollections", ShadowDifferentialProbe::enumCollections);
        section("concurrentAndAtomic", ShadowDifferentialProbe::concurrentAndAtomic);
        section("bigNumbers", ShadowDifferentialProbe::bigNumbers);
        section("regexSurface", ShadowDifferentialProbe::regexSurface);
        section("formatConversions", ShadowDifferentialProbe::formatConversions);
        section("timeSurface", ShadowDifferentialProbe::timeSurface);
        section("textFormatting", ShadowDifferentialProbe::textFormatting);
        section("seededRandom", ShadowDifferentialProbe::seededRandom);
        section("throwableSurface", ShadowDifferentialProbe::throwableSurface);

        finish();
    }

    // The `java.util` factories and views. Fenced and declared like every
    // other section since W7-42; this was `main`'s inline preamble before.
    static void factoriesAndViews() {
        // --- Map.entry --------------------------------------------------
        Map.Entry<String, Integer> me = Map.entry("k", 7);
        entryLike("Map.entry", me);
        try {
            me.setValue(9);
            line("Map.entry.setValue", "returned");
        } catch (Throwable t) {
            line("Map.entry.setValue", t.getClass().getName());
        }

        // --- the entry classes a user can construct directly ------------
        // `Map.entry` above is minted by a native; these two are what
        // ordinary bytecode reaches with `new`, and that is a different
        // allocation path (`num_total_fields` on the fabricated class, not
        // the slot count a native passes to `alloc_object`). It went unprobed
        // and was broken in synthetic mode: both slots read null.
        AbstractMap.SimpleEntry<String, Integer> se = new AbstractMap.SimpleEntry<>("k", 7);
        entryLike("new SimpleEntry", se);
        entryLike("new SimpleImmutableEntry", new AbstractMap.SimpleImmutableEntry<>("k", 7));
        line("new SimpleEntry.equalsEqualPeer", se.equals(new AbstractMap.SimpleEntry<>("k", 7)));
        line("new SimpleEntry.equalsSymmetric",
                new AbstractMap.SimpleEntry<>("k", 7).equals(se) == se.equals(new AbstractMap.SimpleEntry<>("k", 7)));
        se.setValue(8);
        line("new SimpleEntry.afterSetValue", se.getValue());
        try {
            new AbstractMap.SimpleImmutableEntry<>("k", 7).setValue(8);
            line("new SimpleImmutableEntry.setValue", "returned");
        } catch (Throwable t) {
            line("new SimpleImmutableEntry.setValue", t.getClass().getName());
        }

        // --- immutable factories ---------------------------------------
        List<String> l = List.of("a", "b", "c");
        line("List.of.toString", l);
        line("List.of.size", l.size());
        line("List.of.contains", l.contains("b"));
        line("List.of.indexOf", l.indexOf("c"));
        line("List.of.equalsArrayList", l.equals(new ArrayList<>(List.of("a", "b", "c"))));
        line("List.of.hashCodeMatchesArrayList",
                l.hashCode() == new ArrayList<>(List.of("a", "b", "c")).hashCode());
        try {
            l.add("d");
            line("List.of.add", "returned");
        } catch (Throwable t) {
            line("List.of.add", t.getClass().getName());
        }

        Set<String> s = Set.of("x", "y");
        line("Set.of.size", s.size());
        line("Set.of.contains", s.contains("y"));
        line("Set.of.equalsHashSet", s.equals(new HashSet<>(Set.of("x", "y"))));
        try {
            s.add("z");
            line("Set.of.add", "returned");
        } catch (Throwable t) {
            line("Set.of.add", t.getClass().getName());
        }

        Map<String, Integer> m = Map.of("p", 1, "q", 2);
        line("Map.of.size", m.size());
        line("Map.of.get", m.get("q"));
        line("Map.of.containsKey", m.containsKey("p"));
        line("Map.of.equalsHashMap", m.equals(new HashMap<>(Map.of("p", 1, "q", 2))));
        try {
            m.put("r", 3);
            line("Map.of.put", "returned");
        } catch (Throwable t) {
            line("Map.of.put", t.getClass().getName());
        }

        // Iteration order is unspecified for Set.of / Map.of, so sort.
        List<String> keys = new ArrayList<>(m.keySet());
        Collections.sort(keys);
        line("Map.of.sortedKeys", keys);
        List<String> setElems = new ArrayList<>(s);
        Collections.sort(setElems);
        line("Set.of.sortedElems", setElems);

        // --- entrySet of an ordinary map, and its entries ---------------
        Map<String, Integer> hm = new LinkedHashMap<>();
        hm.put("one", 1);
        hm.put("two", 2);
        StringBuilder sb = new StringBuilder();
        for (Map.Entry<String, Integer> e : hm.entrySet()) {
            sb.append(e).append(';');
        }
        line("LinkedHashMap.entrySet.toString", sb);
        Map.Entry<String, Integer> first = hm.entrySet().iterator().next();
        entryLike("LinkedHashMap.firstEntry", first);
        first.setValue(11);
        line("LinkedHashMap.afterSetValue", hm.get("one"));

        // --- unmodifiable views -----------------------------------------
        List<String> ul = Collections.unmodifiableList(new ArrayList<>(List.of("u", "v")));
        line("unmodifiableList.toString", ul);
        try {
            ul.add("w");
            line("unmodifiableList.add", "returned");
        } catch (Throwable t) {
            line("unmodifiableList.add", t.getClass().getName());
        }
        line("List.copyOf.toString", List.copyOf(new ArrayList<>(List.of("c1", "c2"))));
        line("Set.copyOf.size", Set.copyOf(new ArrayList<>(List.of("s1", "s2"))).size());
        line("Map.copyOf.size", Map.copyOf(new LinkedHashMap<>(Map.of("m1", 1))).size());

        // --- subList, a shadow the retag wave touched --------------------
        List<String> base = new ArrayList<>(List.of("s0", "s1", "s2", "s3"));
        List<String> sub = base.subList(1, 3);
        line("subList.toString", sub);
        line("subList.size", sub.size());
        sub.set(0, "CHANGED");
        line("subList.writeThrough", base);

        // Everything above is `java.util`'s factories and views, and the
        // honest reading of "they match" was "the ones anybody looked at
        // match" — the census counts ~1,600 inherited shadows and this probe
        // reached a few dozen of them. The sections registered in `main` are
        // the families the census names and nothing had exercised against the
        // bytecode they shadow.
    }

    // =======================================================================
    // THE MANIFEST
    // =======================================================================
    //
    // Every observable this probe owes, by section, in emission order. This is
    // NOT documentation: it is the half of the instrument that a lost line
    // cannot delete along with itself, because the declaration lives here and
    // the `line(..)` call lives in the section body. The two must be edited
    // together, and both directions are checked at runtime —
    // `MISSING-OBSERVABLE` if a declared name never got emitted,
    // `UNDECLARED-OBSERVABLE` if an emitted name was never declared — so the
    // manifest cannot rot quietly in either direction.
    //
    // Generated from the source and reconciled against the 859-line HotSpot
    // transcript of 2026-08-12: 858 declared names, 0 declared-not-emitted,
    // 0 emitted-not-declared (`PROBE-DONE` aside). A manifest that were merely
    // asserted rather than reconciled would be exactly the kind of probe that
    // cannot fail, which is what this record is about.
    static void manifest() {
        declare("factoriesAndViews",
                "Map.entry.key", "Map.entry.value", "Map.entry.toString",
                "Map.entry.hashCode.matchesSpec", "Map.entry.equalsSelf", "Map.entry.equalsCopy",
                "Map.entry.setValue", "new SimpleEntry.key", "new SimpleEntry.value",
                "new SimpleEntry.toString", "new SimpleEntry.hashCode.matchesSpec",
                "new SimpleEntry.equalsSelf", "new SimpleEntry.equalsCopy",
                "new SimpleImmutableEntry.key", "new SimpleImmutableEntry.value",
                "new SimpleImmutableEntry.toString",
                "new SimpleImmutableEntry.hashCode.matchesSpec",
                "new SimpleImmutableEntry.equalsSelf", "new SimpleImmutableEntry.equalsCopy",
                "new SimpleEntry.equalsEqualPeer", "new SimpleEntry.equalsSymmetric",
                "new SimpleEntry.afterSetValue", "new SimpleImmutableEntry.setValue",
                "List.of.toString", "List.of.size", "List.of.contains", "List.of.indexOf",
                "List.of.equalsArrayList", "List.of.hashCodeMatchesArrayList", "List.of.add",
                "Set.of.size", "Set.of.contains", "Set.of.equalsHashSet", "Set.of.add",
                "Map.of.size", "Map.of.get", "Map.of.containsKey", "Map.of.equalsHashMap",
                "Map.of.put", "Map.of.sortedKeys", "Set.of.sortedElems",
                "LinkedHashMap.entrySet.toString", "LinkedHashMap.firstEntry.key",
                "LinkedHashMap.firstEntry.value", "LinkedHashMap.firstEntry.toString",
                "LinkedHashMap.firstEntry.hashCode.matchesSpec",
                "LinkedHashMap.firstEntry.equalsSelf", "LinkedHashMap.firstEntry.equalsCopy",
                "LinkedHashMap.afterSetValue", "unmodifiableList.toString", "unmodifiableList.add",
                "List.copyOf.toString", "Set.copyOf.size", "Map.copyOf.size", "subList.toString",
                "subList.size", "subList.writeThrough");
        declare("orderedMaps",
                "TreeMap.toString", "TreeMap.firstKey", "TreeMap.lastKey", "TreeMap.firstEntry",
                "TreeMap.floorKey", "TreeMap.ceilingKey", "TreeMap.higherKey", "TreeMap.lowerKey",
                "TreeMap.headMap", "TreeMap.tailMap", "TreeMap.subMap", "TreeMap.descendingMap",
                "TreeMap.descendingKeySet", "TreeMap.headMapRemoveWritesThrough",
                "TreeMap.pollFirstEntry", "TreeMap.afterPollFirst", "TreeSet.toString",
                "TreeSet.first", "TreeSet.last", "TreeSet.headSet", "TreeSet.tailSet",
                "TreeSet.subSet", "TreeSet.descendingSet", "TreeSet.floor", "TreeSet.ceiling",
                "TreeSet.pollFirst", "TreeSet.afterPollFirst", "LinkedHashMap.accessOrder",
                "LinkedHashMap.reversedIsSupported");
        declare("deques",
                "ArrayDeque.toString", "ArrayDeque.peekFirst", "ArrayDeque.peekLast",
                "ArrayDeque.pollFirst", "ArrayDeque.pollLast", "ArrayDeque.afterPolls",
                "ArrayDeque.contains", "LinkedList.getFirst", "LinkedList.getLast",
                "LinkedList.toString", "LinkedList.removeFirst", "LinkedList.removeLast",
                "LinkedList.indexOf", "LinkedList.descendingIterator", "ArrayDeque.pollEmpty",
                "ArrayDeque.peekEmpty", "ArrayDeque.removeEmpty", "ArrayDeque.elementEmpty",
                "PriorityQueue.drainOrder");
        declare("iteratorContracts",
                "Iterator.removeBeforeNext", "Iterator.afterRemove", "Iterator.removeTwice",
                "ListIterator.afterSetAdd", "ListIterator.nextIndex", "ListIterator.previousIndex",
                "ListIterator.hasPrevious", "ListIterator.previous", "ArrayList.CME", "HashMap.CME",
                "Iterator.exhaustedNext");
        declare("arraysFamily",
                "Arrays.sort", "Arrays.binarySearch", "Arrays.binarySearchMissing", "Arrays.copyOf",
                "Arrays.copyOfRange", "Arrays.equals", "Arrays.hashCodeMatches", "Arrays.fill",
                "Arrays.compare", "Arrays.mismatch", "Arrays.deepToString", "Arrays.deepEquals",
                "Arrays.asListWriteThrough", "Arrays.asListAddThrows", "Arrays.stream.sum",
                "Arrays.sortComparator", "Arrays.copyOfRangeBad", "Arrays.fillRangeBad");
        declare("collectionsAlgorithms",
                "Collections.sort", "Collections.max", "Collections.min", "Collections.frequency",
                "Collections.binarySearch", "Collections.reverse", "Collections.swap",
                "Collections.nCopies", "Collections.emptyList", "Collections.singletonList",
                "Collections.singletonMap", "Collections.disjoint", "Collections.rotate",
                "Collections.fill", "Collections.addAll", "Collections.unmodifiableMap.put",
                "Collections.unmodifiableSet.add", "Collections.singletonList.set",
                "Collections.emptyList.add", "Collections.synchronizedList",
                "Collections.enumerationRoundTrip");
        declare("optionalAndObjects",
                "Optional.toStringSome", "Optional.toStringNone", "Optional.map",
                "Optional.filterOut", "Optional.orElse", "Optional.orElseGet",
                "Optional.orElseThrow", "Optional.getOnEmpty", "Optional.ofNullableNull",
                "Optional.equalsSameValue", "Optional.hashCodeMatchesValue", "Optional.flatMap",
                "Optional.ofNull", "Objects.equalsNulls", "Objects.hashCodeNull",
                "Objects.toStringNull", "Objects.requireNonNullMsg", "Objects.hash",
                "Objects.isNull", "Objects.requireNonNullElse", "Objects.checkIndex",
                "Objects.compare");
        declare("stringSurface",
                "String.substring", "String.substringRange", "String.indexOf", "String.lastIndexOf",
                "String.indexOfFrom", "String.replace", "String.replaceAll", "String.replaceFirst",
                "String.splitJoin", "String.splitLimit", "String.splitNegLimit", "String.trim",
                "String.strip", "String.isBlank", "String.repeat", "String.chars.sum",
                "String.compareTo", "String.compareToIgnoreCase", "String.equalsIgnoreCase",
                "String.startsWithOffset", "String.contentEquals", "String.toCharArray",
                "String.valueOfCharArray", "String.formatted", "String.lines",
                "String.stripLeading", "String.stripTrailing", "String.indent", "String.hashCode",
                "String.internIdentity", "String.concatNull", "String.charAtBad",
                "String.substringBad", "String.regionMatches", "String.codePointAt",
                "String.length.surrogate", "String.toUpperCaseRoot", "String.matches",
                "String.CASE_INSENSITIVE_ORDER");
        declare("boxesAndParsing",
                "Integer.parseInt", "Integer.parseIntRadix", "Integer.parseIntBad",
                "Integer.toBinaryString", "Integer.toHexString", "Integer.toOctalString",
                "Integer.MAX_VALUE", "Integer.compare", "Integer.bitCount", "Integer.reverse",
                "Integer.highestOneBit", "Integer.numberOfLeadingZeros",
                "Integer.valueOfCacheIdentity", "Integer.hashCode", "Long.parseLong",
                "Long.toHexString", "Long.hashCode", "Double.parseDouble", "Double.toString",
                "Double.compareNaN", "Double.isNaN", "Double.doubleToLongBits", "Float.toString",
                "Boolean.parseBoolean", "Character.isLetter", "Character.toUpperCase",
                "Character.digit", "Character.getNumericValue", "Byte.parseByte",
                "Short.reverseBytes", "Math.floorDiv", "Math.floorMod", "Math.abs", "Math.round",
                "Math.addExact", "Math.sqrt", "Math.pow", "String.formatIntegers",
                "String.formatFloats", "String.formatWidth");
        declare("joinersAndBuilders",
                "StringBuilder.appendChain", "StringBuilder.insert", "StringBuilder.replace",
                "StringBuilder.deleteCharAt", "StringBuilder.indexOf", "StringBuilder.reverse",
                "StringBuilder.setLength", "StringBuilder.capacityIndependentEquals",
                "StringBuilder.deleteBad", "StringBuilder.appendNull", "StringBuilder.appendSub",
                "StringJoiner.empty", "StringJoiner.filled", "StringJoiner.emptyValue",
                "StringJoiner.length");
        declare("streamsSurface",
                "stream.filterMapSorted", "stream.count", "stream.reduce", "stream.anyMatch",
                "stream.allMatch", "stream.noneMatch", "stream.findFirst", "stream.limitSkip",
                "stream.flatMap", "stream.distinct", "stream.collectJoining",
                "stream.collectToMapSorted", "stream.groupingBySorted", "stream.summaryStats",
                "stream.iterateLimit", "stream.generateLimit", "stream.mapToIntBoxed",
                "stream.reuseThrows", "stream.emptyReduce", "stream.sortedComparator",
                "stream.peekOrder");
        declare("comparators",
                "Comparator.compare", "Comparator.reversed", "Comparator.thenComparing",
                "Comparator.nullsFirst", "Comparator.nullsLast", "Comparator.naturalOrderReverse",
                "List.sortComparator", "Comparator.comparingIdentity");
        declare("bitSetAndUuidAndBase64",
                "BitSet.toString", "BitSet.cardinality", "BitSet.nextSetBit", "BitSet.nextClearBit",
                "BitSet.length", "BitSet.and", "UUID.toString", "UUID.version", "UUID.variant",
                "UUID.mostSigBits", "UUID.equalsRoundTrip", "UUID.hashCodeStable",
                "UUID.nameUUIDFromBytes", "UUID.badString", "Base64.encode", "Base64.roundTrip",
                "Base64.urlEncode", "Base64.decodeBad");
        declare("mapDefaults",
                "Map.getOrDefaultPresent", "Map.getOrDefaultAbsent", "Map.putIfAbsentPresent",
                "Map.putIfAbsentAbsent", "Map.afterPutIfAbsent", "Map.computeIfAbsentNew",
                "Map.computeIfAbsentExisting", "Map.computeIfAbsentNull",
                "Map.computeIfAbsentNullCreatesNoEntry", "Map.computeIfPresentAbsent",
                "Map.computeIfPresentNullRemoves", "Map.afterComputeIfPresentNull",
                "Map.computeOnAbsent", "Map.computeNullRemoves", "Map.afterComputeNull",
                "Map.mergeAbsentUsesValue", "Map.mergePresentCombines",
                "Map.mergeNullResultRemoves", "Map.afterMergeNull", "Map.mergeNullValueThrows",
                "Map.replacePresent", "Map.replaceAbsent", "Map.replaceThreeArgMismatch",
                "Map.replaceThreeArgMatch", "Map.removeKeyValueMismatch", "Map.removeKeyValueMatch",
                "Map.replaceAll", "Map.forEachOrder", "Map.finalContent", "HashMap.nullKeyGet",
                "HashMap.nullKeyContains", "HashMap.nullValueGet", "HashMap.nullValueContainsKey",
                "HashMap.getOrDefaultOverNullValue", "HashMap.containsValueNull", "HashMap.size",
                "HashMap.removeNullKey", "HashMap.sizeAfterNullKeyRemove");
        declare("mapViewWriteThrough",
                "keySet.content", "values.content", "entrySet.content",
                "keySet.removeWritesThrough", "values.removeWritesThrough", "keySet.removeIf",
                "entrySet.removeIf", "keySet.seesLaterPut", "values.seesLaterPut",
                "keySet.sizeAfterLaterPut", "entrySet.sizeAfterLaterPut", "keySet.addUnsupported",
                "values.addUnsupported", "entrySet.setValueWritesThrough",
                "keySet.retainAllWritesThrough", "values.clearWritesThrough",
                "values.removeRemovesOneMapping", "entrySet.iteratorRemoveWritesThrough",
                "keySet.equalsPlainSet", "TreeMap.keySetIsSorted");
        declare("subListContracts",
                "subList.content", "subList.setWritesThrough", "subList.addWritesThrough",
                "subList.sizeAfterAdd", "subList.baseSizeAfterAdd", "subList.removeWritesThrough",
                "subList.nestedContent", "subList.nestedClearWritesThroughToBase",
                "subList.afterNestedClear", "subList.emptyRange", "subList.emptyRangeIsEmpty",
                "subList.reversedRange", "subList.pastEnd", "subList.negativeStart",
                "subList.staleSize", "subList.staleGet", "subList.staleToString",
                "subList.staleIterator", "List.of.subList", "List.of.subList.addThrows",
                "Arrays.asList.subList.setWritesThrough", "subList.sortWritesThrough",
                "subList.equalsPlainList");
        declare("wrapperViews",
                "unmodifiableList.seesBackingWrite", "unmodifiableList.sizeAfterBackingWrite",
                "unmodifiableList.set", "unmodifiableList.removeIf", "unmodifiableList.sort",
                "unmodifiableList.replaceAll", "unmodifiableList.iteratorRemove",
                "unmodifiableList.subListAdd", "unmodifiableList.equalsBacking",
                "unmodifiableList.contentAfterAll", "unmodifiableMap.seesBackingWrite",
                "unmodifiableMap.getAfterBackingWrite", "unmodifiableMap.entrySetSetValue",
                "unmodifiableMap.keySetRemove", "unmodifiableMap.valuesClear",
                "unmodifiableMap.merge", "unmodifiableMap.computeIfAbsent",
                "unmodifiableMap.contentAfterAll", "unmodifiableSet.content",
                "unmodifiableSet.removeAll", "unmodifiableCollection.content",
                "unmodifiableSortedMap.content", "unmodifiableList.rewrapIsNewObject",
                "checkedList.goodAdd", "checkedList.badAdd", "checkedList.contentAfterBadAdd",
                "checkedList.badSet", "checkedMap.badValuePut", "checkedMap.badKeyPut",
                "checkedMap.contentAfterBadPuts", "checkedCollection.badAdd",
                "synchronizedCollection.content", "synchronizedMap.content",
                "synchronizedMap.sortedKeys", "synchronizedMap.getOrDefault",
                "Collections.emptyMap.get", "Collections.emptyIterator.hasNext",
                "Collections.emptyIterator.next", "Collections.emptySet.equalsEmpty");
        declare("navigableEdges",
                "TreeMap.emptyFirstEntry", "TreeMap.emptyLastEntry", "TreeMap.emptyFloorKey",
                "TreeMap.emptyCeilingEntry", "TreeMap.emptyPollFirstEntry",
                "TreeMap.emptyFirstKeyThrows", "TreeMap.emptyLastKeyThrows", "TreeMap.nullKeyPut",
                "TreeMap.nullKeyGet", "TreeSet.emptyFirstThrows", "TreeSet.emptyPollFirst",
                "TreeMap.headMapInclusive", "TreeMap.headMapExclusive", "TreeMap.tailMapExclusive",
                "TreeMap.subMapBothInclusive", "TreeMap.subMapBothExclusive",
                "TreeMap.subMapReversedBounds", "TreeMap.floorEntry", "TreeMap.ceilingEntry",
                "TreeMap.higherEntryAtLast", "TreeMap.lowerEntryAtFirst", "TreeMap.navigableKeySet",
                "TreeMap.descendingKeySetSize", "TreeMap.firstEntryIsImmutable",
                "TreeMap.entrySetEntryIsLive", "TreeMap.descendingWriteThrough",
                "TreeMap.headMapPutOutOfRange", "TreeMap.contentAfterAll",
                "TreeSet.headSetInclusive", "TreeSet.tailSetExclusive",
                "TreeSet.subSetInclusiveExclusive", "TreeSet.higherAtLast", "TreeSet.lowerAtFirst",
                "TreeSet.pollLast", "TreeSet.descendingWriteThrough",
                "TreeSet.customComparatorOrder", "TreeSet.comparatorIsReported",
                "TreeSet.customFirst", "TreeSet.headSetUnderCustomComparator",
                "TreeSet.nullAddNaturalOrdering", "TreeSet.incomparableFirstAdd",
                "TreeMap.comparatorNullForNaturalOrder");
        declare("dequeEdges",
                "ArrayDeque.addNull", "ArrayDeque.addFirstNull", "ArrayDeque.offerNull",
                "ArrayDeque.sizeAfterRefusedNulls", "ArrayDeque.pushIsAddFirst", "ArrayDeque.pop",
                "ArrayDeque.content", "ArrayDeque.removeFirstOccurrence",
                "ArrayDeque.removeLastOccurrence", "ArrayDeque.descendingIterator",
                "ArrayDeque.toArray", "ArrayDeque.clearThenIsEmpty",
                "ArrayDeque.growsPastInitialCapacity", "ArrayDeque.getFirstOnEmpty",
                "ArrayDeque.popOnEmpty", "LinkedList.acceptsNull", "LinkedList.peekOnEmpty",
                "LinkedList.removeFirstOnEmpty", "LinkedList.addAtIndex",
                "ArrayDequeAsStack.toString", "Stack.toString", "Stack.peekIsTop", "Stack.search",
                "Stack.popOnEmpty", "PriorityQueue.peekIsHead",
                "PriorityQueue.customComparatorDrain", "PriorityQueue.nullAdd",
                "ConcurrentLinkedQueue.poll", "ConcurrentLinkedQueue.nullOffer",
                "ConcurrentLinkedQueue.pollEmpty");
        declare("enumCollections",
                "EnumMap.ordinalOrderRegardlessOfInsertion", "EnumMap.keySet", "EnumMap.values",
                "EnumMap.get", "EnumMap.getAbsent", "EnumMap.nullKeyPut", "EnumMap.size",
                "EnumMap.containsValue", "EnumMap.equalsPlainHashMap", "EnumMap.removeThenSize",
                "EnumSet.ofOrdinalOrder", "EnumSet.allOf", "EnumSet.noneOf", "EnumSet.range",
                "EnumSet.complementOf", "EnumSet.copyOfCollection", "EnumSet.iterationIsOrdinal",
                "EnumSet.removeReturns", "EnumSet.containsNonEnum", "EnumSet.equalsPlainSet",
                "EnumSet.retainAll", "Enum.valueOf", "Enum.valueOfBadName", "Enum.valueOfNull",
                "Enum.ordinal", "Enum.name", "Enum.compareTo", "Enum.valuesLength",
                "Enum.valuesIsAFreshArray", "Enum.getDeclaringClass", "Enum.equalsIsIdentity",
                "Enum.switchDispatch", "Enum.constantBodyApply",
                "Enum.constantBodyClassIsEnumClass", "Enum.constantBodyDeclaringClass",
                "EnumSet.overConstantBodies", "EnumMap.overConstantBodies", "Enum.constantBodyName");
        declare("concurrentAndAtomic",
                "CHM.get", "CHM.nullKeyPut", "CHM.nullValuePut", "CHM.nullKeyGet",
                "CHM.nullValueMerge", "CHM.sizeAfterRefusedNulls", "CHM.putIfAbsentPresent",
                "CHM.computeIfAbsent", "CHM.computeNullRemoves", "CHM.merge", "CHM.getOrDefault",
                "CHM.sortedContent", "CHM.reduceValues", "CHM.keySetViewAdd", "CHM.newKeySet",
                "CHM.searchKeys", "COW.content", "COW.snapshotIteratorDoesNotSeeAdds",
                "COW.iteratorRemoveUnsupported", "COW.addIfAbsentDuplicate", "COW.addIfAbsentNew",
                "COW.addAllAbsent", "ABQ.offerFits", "ABQ.offerWhenFullIsFalse",
                "ABQ.sizeAfterRefusedOffer", "ABQ.remainingCapacity", "ABQ.addWhenFullThrows",
                "ABQ.content", "ABQ.pollThenOffer", "ABQ.drainTo", "ABQ.nullOffer",
                "ABQ.zeroCapacity", "AtomicInteger.getAndIncrement",
                "AtomicInteger.incrementAndGet", "AtomicInteger.compareAndSetMismatch",
                "AtomicInteger.compareAndSetMatch", "AtomicInteger.getAndUpdate",
                "AtomicInteger.accumulateAndGet", "AtomicInteger.getAndSet",
                "AtomicInteger.toString", "AtomicInteger.intValueOverflow", "AtomicLong.addAndGet",
                "AtomicBoolean.compareAndSet", "AtomicReference.updateAndGet",
                "AtomicReference.compareAndSetIsIdentity", "LongAdder.sum");
        declare("bigNumbers",
                "BigDecimal.toString", "BigDecimal.scale", "BigDecimal.precision",
                "BigDecimal.unscaledValue", "BigDecimal.equalsComparesScale",
                "BigDecimal.compareToIgnoresScale", "BigDecimal.hashCodeTracksScale",
                "BigDecimal.setDedupes", "BigDecimal.add", "BigDecimal.subtract",
                "BigDecimal.multiplyScaleAdds", "BigDecimal.divideExact",
                "BigDecimal.divideNonTerminating", "BigDecimal.divideByZero",
                "BigDecimal.divideRounded", "BigDecimal.setScaleHalfEven",
                "BigDecimal.setScaleHalfEvenOdd", "BigDecimal.setScaleHalfUp",
                "BigDecimal.setScaleFloorNegative", "BigDecimal.setScaleUnnecessary",
                "BigDecimal.stripTrailingZeros", "BigDecimal.stripThenPlainString",
                "BigDecimal.negativeZeroStrip", "BigDecimal.fromDoubleIsExact",
                "BigDecimal.valueOfDoubleUsesToString", "BigDecimal.movePointLeft",
                "BigDecimal.pow", "BigDecimal.intValueExactOnFraction",
                "BigDecimal.intValueTruncates", "BigDecimal.toEngineeringString",
                "BigDecimal.badString", "BigInteger.pow", "BigInteger.modPow", "BigInteger.gcd",
                "BigInteger.toStringRadix", "BigInteger.fromRadixString", "BigInteger.divideByZero",
                "BigInteger.modNegative", "BigInteger.remainderNegative", "BigInteger.shiftLeft",
                "BigInteger.bitLengthAndCount", "BigInteger.testBit", "BigInteger.signumNegate",
                "BigInteger.longValueExactOverflow", "BigInteger.longValueTruncates",
                "BigInteger.badString", "BigInteger.compareTo",
                "BigInteger.equalsAcrossConstruction");
        declare("regexSurface",
                "Matcher.groupBeforeFindThrows", "Matcher.startBeforeFindThrows", "Matcher.find1",
                "Matcher.find2", "Matcher.findExhausted", "Matcher.groupCount",
                "Matcher.groupAfterFailedFind", "Matcher.resetRestartsFind",
                "Matcher.groupIndexPastGroupCount", "Matcher.findAllBounded", "Matcher.namedGroups",
                "Matcher.unmatchedOptionalGroupIsNull", "Matcher.matchesVsFind",
                "Matcher.lookingAt", "Matcher.hitEnd", "Matcher.replaceAllBackref",
                "Matcher.replaceFirst", "Matcher.appendReplacement", "Matcher.quoteReplacement",
                "Matcher.badReplacementRef", "Matcher.results", "Pattern.quoteDefeatsMetachars",
                "Pattern.splitLimit2", "Pattern.splitDropsTrailingEmpties",
                "Pattern.splitKeepsTrailingEmpties", "Pattern.splitZeroWidth",
                "Pattern.splitLeadingEmpty", "Pattern.badSyntax", "Pattern.caseInsensitiveFlag",
                "Pattern.dotallFlag", "Pattern.multilineFlag", "Pattern.matchesStatic",
                "Pattern.asPredicate", "Pattern.asMatchPredicate", "Pattern.toStringIsThePattern",
                "Pattern.backreference", "Pattern.lookahead", "Pattern.lookbehind",
                "Pattern.unicodeClass", "Pattern.greedyVsReluctant");
        declare("formatConversions",
                "format.sNull", "format.sUpper", "format.sPrecisionTruncates",
                "format.booleanOfNullAndObject", "format.charFromCharAndInt",
                "format.charFromSupplementary", "format.grouping", "format.parenthesisedNegative",
                "format.zeroPadNegativeFloat", "format.zeroPadInt", "format.argumentIndex",
                "format.previousArgument", "format.literalPercent", "format.hexAndOctal",
                "format.hexOfNegative", "format.scientific", "format.general", "format.hexFloat",
                "format.floatSpecials", "format.floatRoundingHalfUp", "format.bigDecimalPrecision",
                "format.bigInteger", "format.widthOnNull", "format.plusFlag",
                "format.unknownConversion", "format.missingArgument", "format.wrongArgumentType",
                "format.illegalFlagCombination", "format.precisionOnInteger", "format.localeUS",
                "format.localeGermany", "format.localeFrance", "format.formatterAppendable");
        declare("timeSurface",
                "LocalDate.toString", "LocalDate.plusMonthsClampsToShorterMonth",
                "LocalDate.plusMonthsNonLeapYear", "LocalDate.plusMonthsIsNotThirtyDays",
                "LocalDate.roundTripIsNotIdentity", "LocalDate.plusDaysAcrossYear",
                "LocalDate.minusYearsFromLeapDay", "LocalDate.isLeapYear",
                "LocalDate.lengthOfMonth", "LocalDate.dayOfWeek", "LocalDate.dayOfYearAfterLeapDay",
                "LocalDate.month", "LocalDate.withDayOfMonthOutOfRange", "LocalDate.ofBadMonth",
                "LocalDate.ofFeb30", "LocalDate.parseBadDay", "LocalDate.parseUnpadded",
                "LocalDate.compareAndEquals", "LocalDate.until", "ChronoUnit.daysBetween",
                "ChronoUnit.monthsBetweenIsWhole", "YearMonth.atEndOfMonth", "Period.parse",
                "Period.normalized", "Period.toTotalMonths", "Period.parseBad", "Duration.parse",
                "Duration.toStringOfSeconds", "Duration.toStringNegative", "Duration.toStringZero",
                "Duration.plusAndToMillis", "Duration.dividedBy", "Duration.parseBad",
                "Duration.between", "LocalDateTime.toStringDropsZeroSeconds",
                "LocalDateTime.withSeconds", "LocalTime.toStringDropsZeroSeconds",
                "LocalTime.ofNanoOfDay", "Instant.epoch", "Instant.plusNanos",
                "Instant.toEpochMilli", "ZonedDateTime.atFixedOffset", "ZonedDateTime.offsetShift",
                "OffsetDateTime.toInstant", "DateTimeFormatter.ISO_DATE",
                "DateTimeFormatter.ISO_LOCAL_DATE_TIME", "DateTimeFormatter.numericPattern",
                "DateTimeFormatter.textPattern", "DateTimeFormatter.shortTextPattern",
                "DateTimeFormatter.twelveHourClock", "DateTimeFormatter.parseRoundTrip",
                "DateTimeFormatter.parseWrongPattern", "DateTimeFormatter.badPattern",
                "DateTimeFormatter.formatWrongTemporal");
        declare("textFormatting",
                "DecimalFormat.basic", "DecimalFormat.negative", "DecimalFormat.doubleTieRounding",
                "DecimalFormat.exactTieDefaultIsHalfEven", "DecimalFormat.exactTieHalfUp",
                "DecimalFormat.roundingModeIsReported", "DecimalFormat.optionalDigits",
                "DecimalFormat.percentPattern", "DecimalFormat.scientificPattern",
                "DecimalFormat.negativeSubpattern", "DecimalFormat.groupingSize",
                "DecimalFormat.toPattern", "DecimalFormat.parse",
                "DecimalFormat.parseTrailingGarbage", "DecimalFormat.parseNotANumber",
                "DecimalFormat.parseIntegerOnly", "DecimalFormat.formatBigDecimalExactly",
                "DecimalFormat.formatLong", "NumberFormat.integerInstanceRounds",
                "NumberFormat.percentInstance", "NumberFormat.currencyUS",
                "NumberFormat.currencyNegativeUS", "NumberFormat.maxFractionDigits",
                "NumberFormat.defaultMaxFraction", "MessageFormat.simple",
                "MessageFormat.numberSubformat", "MessageFormat.quotedBrace",
                "MessageFormat.choiceSubformat", "MessageFormat.missingArgument",
                "MessageFormat.reorderedIndices", "SimpleDateFormat.utc",
                "SimpleDateFormat.parseRoundTrip", "SimpleDateFormat.lenientAcceptsOverflow",
                "SimpleDateFormat.strictRejectsOverflow");
        declare("seededRandom",
                "Random.nextIntSequence", "Random.nextIntBoundSequence",
                "Random.nextIntPowerOfTwoBound", "Random.nextIntOriginBound", "Random.nextLong",
                "Random.nextDouble", "Random.nextFloat", "Random.nextBooleanSequence",
                "Random.nextGaussian", "Random.nextBytes", "Random.intsStream",
                "Random.doublesStreamFirst", "Random.setSeedRestartsSequence",
                "Random.sameSeedSameSequence", "Random.differentSeedsCollide",
                "Random.nextIntZeroBound", "Random.nextIntNegativeBound",
                "Collections.shuffleSeeded", "Collections.shuffleSeededTwiceIsStable");
        declare("throwableSurface",
                "Throwable.getMessage", "Throwable.getCauseMessage", "Throwable.toString",
                "Throwable.causeToString", "Throwable.getLocalizedMessage",
                "Throwable.noArgMessageIsNull", "Throwable.noArgToString",
                "Throwable.causeOfNoCauseIsNull", "Throwable.initCauseAfterCtorThrows",
                "Throwable.initCauseOnce", "Throwable.initCauseTwiceThrows",
                "Throwable.selfCauseThrows", "Throwable.addSuppressedSelfThrows",
                "Throwable.suppressedFromTryWithResources", "Throwable.suppressedDefaultIsEmpty",
                "Throwable.suppressionDisabled", "Throwable.stackTraceTopFrame",
                "Throwable.stackTraceNonEmpty", "Throwable.setStackTraceIsHonoured",
                "Throwable.customSubclassMessage",
                "Throwable.shadowedCauseInitCauseSucceeds",
                "Throwable.shadowedCauseInitCauseNull",
                "Throwable.shadowedCauseGetCauseAfterInit",
                "Throwable.shadowedCauseOwnFieldUntouched",
                "Throwable.shadowedCauseCtorCauseWins",
                "Throwable.shadowedCauseSecondInitCauseThrows",
                "Throwable.shadowedDetailMessageGetMessage",
                "Throwable.shadowedDetailMessageOwnFieldUntouched",
                "VM.nullPointerHelpfulMessage",
                "VM.nullFieldAccessMessage", "VM.nullArrayStoreMessage", "VM.divideByZero",
                "VM.modByZero", "VM.longDivideByZero", "VM.doubleDivideByZeroIsInfinity",
                "VM.classCast", "VM.arrayStore", "VM.arrayIndexOutOfBounds", "VM.negativeArraySize",
                "VM.arrayLengthOfNull", "VM.checkcastToArray", "VM.integerOverflowWraps",
                "VM.intMinValueNegated", "VM.intDivideMinByMinusOne");
    }

    // -- TreeMap/TreeSet navigation, LinkedHashMap access order -------------
    // Navigable views are the densest shadow family after the factories: a
    // `headMap`/`tailMap`/`descendingMap` that answers a snapshot instead of a
    // view is indistinguishable from a working one until something writes.
    static void orderedMaps() {
        TreeMap<String, Integer> tm = new TreeMap<>();
        tm.put("b", 2);
        tm.put("d", 4);
        tm.put("a", 1);
        tm.put("c", 3);
        line("TreeMap.toString", tm);
        line("TreeMap.firstKey", tm.firstKey());
        line("TreeMap.lastKey", tm.lastKey());
        line("TreeMap.firstEntry", tm.firstEntry());
        line("TreeMap.floorKey", tm.floorKey("bb"));
        line("TreeMap.ceilingKey", tm.ceilingKey("bb"));
        line("TreeMap.higherKey", tm.higherKey("b"));
        line("TreeMap.lowerKey", tm.lowerKey("b"));
        line("TreeMap.headMap", tm.headMap("c"));
        line("TreeMap.tailMap", tm.tailMap("c"));
        line("TreeMap.subMap", tm.subMap("b", "d"));
        line("TreeMap.descendingMap", tm.descendingMap());
        line("TreeMap.descendingKeySet", tm.descendingKeySet());
        // A view, not a snapshot: writing through it must reach the map.
        tm.headMap("c").remove("a");
        line("TreeMap.headMapRemoveWritesThrough", tm);
        line("TreeMap.pollFirstEntry", tm.pollFirstEntry());
        line("TreeMap.afterPollFirst", tm);

        TreeSet<Integer> ts = new TreeSet<>(List.of(5, 1, 9, 3));
        line("TreeSet.toString", ts);
        line("TreeSet.first", ts.first());
        line("TreeSet.last", ts.last());
        line("TreeSet.headSet", ts.headSet(5));
        line("TreeSet.tailSet", ts.tailSet(5));
        line("TreeSet.subSet", ts.subSet(1, 9));
        line("TreeSet.descendingSet", ts.descendingSet());
        line("TreeSet.floor", ts.floor(4));
        line("TreeSet.ceiling", ts.ceiling(4));
        line("TreeSet.pollFirst", ts.pollFirst());
        line("TreeSet.afterPollFirst", ts);

        // Access-order LinkedHashMap: `get` REORDERS. A shadow that keeps
        // insertion order looks correct on `toString` until it is read.
        LinkedHashMap<String, Integer> lru = new LinkedHashMap<>(16, 0.75f, true);
        lru.put("x", 1);
        lru.put("y", 2);
        lru.put("z", 3);
        lru.get("x");
        line("LinkedHashMap.accessOrder", lru.keySet());
        line("LinkedHashMap.reversedIsSupported", lru.entrySet().size());
    }

    // -- Deque / Queue, both ends and both null policies ---------------------
    static void deques() {
        ArrayDeque<String> dq = new ArrayDeque<>();
        dq.addLast("b");
        dq.addFirst("a");
        dq.addLast("c");
        line("ArrayDeque.toString", dq);
        line("ArrayDeque.peekFirst", dq.peekFirst());
        line("ArrayDeque.peekLast", dq.peekLast());
        line("ArrayDeque.pollFirst", dq.pollFirst());
        line("ArrayDeque.pollLast", dq.pollLast());
        line("ArrayDeque.afterPolls", dq);
        line("ArrayDeque.contains", dq.contains("b"));

        LinkedList<String> ll = new LinkedList<>(List.of("p", "q", "r"));
        line("LinkedList.getFirst", ll.getFirst());
        line("LinkedList.getLast", ll.getLast());
        ll.addFirst("o");
        ll.addLast("s");
        line("LinkedList.toString", ll);
        line("LinkedList.removeFirst", ll.removeFirst());
        line("LinkedList.removeLast", ll.removeLast());
        line("LinkedList.indexOf", ll.indexOf("q"));
        line("LinkedList.descendingIterator", joinIter(ll.descendingIterator()));

        // `poll`/`peek` answer null on empty where `remove`/`element` throw.
        // Two methods, one shadow each, and only the throwing pair says so.
        ArrayDeque<String> empty = new ArrayDeque<>();
        line("ArrayDeque.pollEmpty", empty.poll());
        line("ArrayDeque.peekEmpty", empty.peek());
        line("ArrayDeque.removeEmpty", thrownBy(() -> empty.remove()));
        line("ArrayDeque.elementEmpty", thrownBy(() -> empty.element()));

        // BOUNDED (2026-08-11): this drain terminates only if `poll` actually
        // REMOVES the head. A poll that reads without unlinking spins here
        // forever and truncates every section after it — the same hazard as
        // the unbounded CME loop below, in a shape that does not look like a
        // test for an exception at all.
        PriorityQueue<Integer> pq = new PriorityQueue<>(List.of(4, 1, 7, 3));
        line("PriorityQueue.drainOrder", drainBounded(pq));
    }

    // -- Iterator / ListIterator contracts -----------------------------------
    // The census's abstract-target natives sit on `Iterator` itself, so the
    // shapes that matter are the ones a shadow gets wrong for free: `remove`
    // before `next`, `remove` twice, and a `ConcurrentModificationException`
    // that never fires.
    static void iteratorContracts() {
        List<String> l = new ArrayList<>(List.of("a", "b", "c", "d"));
        Iterator<String> it = l.iterator();
        line("Iterator.removeBeforeNext", thrownBy(it::remove));
        it.next();
        it.remove();
        line("Iterator.afterRemove", l);
        line("Iterator.removeTwice", thrownBy(it::remove));

        ListIterator<String> li = l.listIterator();
        li.next();
        li.set("B");
        li.add("B2");
        line("ListIterator.afterSetAdd", l);
        line("ListIterator.nextIndex", li.nextIndex());
        line("ListIterator.previousIndex", li.previousIndex());
        line("ListIterator.hasPrevious", li.hasPrevious());
        line("ListIterator.previous", li.previous());

        // BOUNDED on purpose. `for (x : l) l.add(x);` relies on the very
        // exception it is testing for to terminate: on a VM whose iterator does
        // not throw, the unbounded form grows the list until the heap is gone
        // and takes the rest of the probe with it — a differential that cannot
        // report a difference because it never reaches the next line.
        line("ArrayList.CME", thrownBy(() -> {
            int guard = 0;
            for (String s : l) {
                if (++guard > 100) {
                    throw new IllegalStateException("no-CME-after-100");
                }
                l.add(s);
            }
        }));
        line("HashMap.CME", thrownBy(() -> {
            Map<String, Integer> m = new LinkedHashMap<>(Map.of("k", 1));
            int guard = 0;
            for (Map.Entry<String, Integer> e : m.entrySet()) {
                if (++guard > 100) {
                    throw new IllegalStateException("no-CME-after-100");
                }
                m.put(e.getKey() + "!", 2);
            }
        }));
        line("Iterator.exhaustedNext", thrownBy(() -> {
            Iterator<String> e = new ArrayList<String>().iterator();
            e.next();
        }));
    }

    // -- java.util.Arrays ----------------------------------------------------
    static void arraysFamily() {
        int[] a = {5, 3, 9, 1};
        int[] b = a.clone();
        Arrays.sort(b);
        line("Arrays.sort", Arrays.toString(b));
        line("Arrays.binarySearch", Arrays.binarySearch(b, 5));
        line("Arrays.binarySearchMissing", Arrays.binarySearch(b, 4));
        line("Arrays.copyOf", Arrays.toString(Arrays.copyOf(b, 6)));
        line("Arrays.copyOfRange", Arrays.toString(Arrays.copyOfRange(b, 1, 3)));
        line("Arrays.equals", Arrays.equals(a, a.clone()));
        line("Arrays.hashCodeMatches", Arrays.hashCode(a) == Arrays.hashCode(a.clone()));
        int[] filled = new int[4];
        Arrays.fill(filled, 7);
        line("Arrays.fill", Arrays.toString(filled));
        line("Arrays.compare", Arrays.compare(new int[] {1, 2}, new int[] {1, 3}));
        line("Arrays.mismatch", Arrays.mismatch(new int[] {1, 2, 3}, new int[] {1, 9, 3}));
        String[][] deep = {{"x"}, {"y", "z"}};
        line("Arrays.deepToString", Arrays.deepToString(deep));
        line("Arrays.deepEquals", Arrays.deepEquals(deep, new String[][] {{"x"}, {"y", "z"}}));
        line("Arrays.asListWriteThrough", asListWriteThrough());
        line("Arrays.asListAddThrows", thrownBy(() -> Arrays.asList("q").add("r")));
        line("Arrays.stream.sum", Arrays.stream(a).sum());
        line("Arrays.sortComparator", sortedWithComparator());
        // Out-of-bounds is per-API on HotSpot, so ask each shape rather than
        // assuming one answer covers them.
        line("Arrays.copyOfRangeBad", thrownBy(() -> Arrays.copyOfRange(b, 3, 1)));
        line("Arrays.fillRangeBad", thrownBy(() -> Arrays.fill(filled, 3, 1, 0)));
    }

    static String asListWriteThrough() {
        String[] backing = {"a", "b"};
        List<String> view = Arrays.asList(backing);
        view.set(0, "A");
        return backing[0] + "," + view;
    }

    static String sortedWithComparator() {
        String[] s = {"bb", "a", "ccc"};
        Arrays.sort(s, Comparator.comparingInt(String::length).thenComparing(Comparator.naturalOrder()));
        return Arrays.toString(s);
    }

    // -- java.util.Collections ----------------------------------------------
    static void collectionsAlgorithms() {
        List<Integer> l = new ArrayList<>(List.of(3, 1, 4, 1, 5));
        Collections.sort(l);
        line("Collections.sort", l);
        line("Collections.max", Collections.max(l));
        line("Collections.min", Collections.min(l));
        line("Collections.frequency", Collections.frequency(l, 1));
        line("Collections.binarySearch", Collections.binarySearch(l, 4));
        Collections.reverse(l);
        line("Collections.reverse", l);
        Collections.swap(l, 0, 4);
        line("Collections.swap", l);
        line("Collections.nCopies", Collections.nCopies(3, "z"));
        line("Collections.emptyList", Collections.emptyList());
        line("Collections.singletonList", Collections.singletonList("only"));
        line("Collections.singletonMap", Collections.singletonMap("k", "v"));
        line("Collections.disjoint", Collections.disjoint(List.of(1, 2), List.of(3, 4)));
        List<Integer> rot = new ArrayList<>(List.of(1, 2, 3, 4));
        Collections.rotate(rot, 1);
        line("Collections.rotate", rot);
        Collections.fill(rot, 0);
        line("Collections.fill", rot);
        line("Collections.addAll", collectionsAddAll());
        line("Collections.unmodifiableMap.put", thrownBy(
                () -> Collections.unmodifiableMap(new LinkedHashMap<>(Map.of("a", 1))).put("b", 2)));
        line("Collections.unmodifiableSet.add", thrownBy(
                () -> Collections.unmodifiableSet(new LinkedHashSet<>(List.of("a"))).add("b")));
        line("Collections.singletonList.set", thrownBy(() -> Collections.singletonList("a").set(0, "b")));
        line("Collections.emptyList.add", thrownBy(() -> Collections.emptyList().add("x")));
        // A synchronized view must still BE the collection it wraps.
        List<String> sync = Collections.synchronizedList(new ArrayList<>(List.of("s")));
        sync.add("t");
        line("Collections.synchronizedList", sync);
        line("Collections.enumerationRoundTrip",
                joinEnumeration(Collections.enumeration(List.of("e1", "e2"))));
    }

    static String collectionsAddAll() {
        List<String> l = new ArrayList<>();
        Collections.addAll(l, "a", "b");
        return l.toString();
    }

    // -- Optional / Objects --------------------------------------------------
    static void optionalAndObjects() {
        Optional<String> some = Optional.of("v");
        Optional<String> none = Optional.empty();
        line("Optional.toStringSome", some);
        line("Optional.toStringNone", none);
        line("Optional.map", some.map(String::toUpperCase));
        line("Optional.filterOut", some.filter(s -> false));
        line("Optional.orElse", none.orElse("d"));
        line("Optional.orElseGet", none.orElseGet(() -> "g"));
        line("Optional.orElseThrow", thrownBy(none::orElseThrow));
        line("Optional.getOnEmpty", thrownBy(none::get));
        line("Optional.ofNullableNull", Optional.ofNullable(null));
        line("Optional.equalsSameValue", some.equals(Optional.of("v")));
        line("Optional.hashCodeMatchesValue", some.hashCode() == "v".hashCode());
        line("Optional.flatMap", some.flatMap(s -> Optional.of(s + "!")));
        line("Optional.ofNull", thrownBy(() -> Optional.of(null)));

        line("Objects.equalsNulls", Objects.equals(null, null));
        line("Objects.hashCodeNull", Objects.hashCode(null));
        line("Objects.toStringNull", Objects.toString(null, "dflt"));
        line("Objects.requireNonNullMsg", thrownBy(() -> Objects.requireNonNull(null, "boom")));
        line("Objects.hash", Objects.hash("a", 1));
        line("Objects.isNull", Objects.isNull(null));
        line("Objects.requireNonNullElse", Objects.requireNonNullElse(null, "e"));
        line("Objects.checkIndex", thrownBy(() -> Objects.checkIndex(5, 3)));
        line("Objects.compare", Objects.compare("a", "b", Comparator.naturalOrder()));
    }

    // -- java.lang.String ----------------------------------------------------
    // The biggest single registration cluster in the dead sweep is
    // `lang_string.rs`, so the surface it shadows deserves more than
    // `split`.
    static void stringSurface() {
        String s = "Hello, World";
        line("String.substring", s.substring(7));
        line("String.substringRange", s.substring(0, 5));
        line("String.indexOf", s.indexOf("o"));
        line("String.lastIndexOf", s.lastIndexOf("o"));
        line("String.indexOfFrom", s.indexOf("o", 5));
        line("String.replace", s.replace('l', 'L'));
        line("String.replaceAll", s.replaceAll("[aeiou]", "_"));
        line("String.replaceFirst", s.replaceFirst("l", "_"));
        line("String.splitJoin", String.join("|", s.split(", ")));
        line("String.splitLimit", Arrays.toString("a,b,,c,,".split(",")));
        line("String.splitNegLimit", Arrays.toString("a,b,,c,,".split(",", -1)));
        line("String.trim", "  pad  ".trim() + "#");
        line("String.strip", "  pad  ".strip() + "#");
        line("String.isBlank", "  ".isBlank());
        line("String.repeat", "ab".repeat(3));
        line("String.chars.sum", "abc".chars().sum());
        line("String.compareTo", "apple".compareTo("banana"));
        line("String.compareToIgnoreCase", "APPLE".compareToIgnoreCase("apple"));
        line("String.equalsIgnoreCase", "AbC".equalsIgnoreCase("aBc"));
        line("String.startsWithOffset", s.startsWith("World", 7));
        line("String.contentEquals", s.contentEquals(new StringBuilder("Hello, World")));
        line("String.toCharArray", Arrays.toString("hey".toCharArray()));
        line("String.valueOfCharArray", String.valueOf(new char[] {'x', 'y'}));
        line("String.formatted", "%s=%d".formatted("n", 3));
        line("String.lines", String.join("/", "a\nb\r\nc".lines().toArray(String[]::new)));
        line("String.stripLeading", "  q ".stripLeading() + "#");
        line("String.stripTrailing", " q  ".stripTrailing() + "#");
        line("String.indent", "x".indent(2).replace("\n", "\\n"));
        line("String.hashCode", "Hello, World".hashCode());
        line("String.internIdentity", "ab".intern() == ("a" + "b").intern());
        line("String.concatNull", "a" + (String) null);
        line("String.charAtBad", thrownBy(() -> "ab".charAt(9)));
        line("String.substringBad", thrownBy(() -> "ab".substring(3)));
        line("String.regionMatches", s.regionMatches(true, 7, "WORLD", 0, 5));
        line("String.codePointAt", "😀x".codePointAt(0));
        line("String.length.surrogate", "😀x".length());
        line("String.toUpperCaseRoot", "istanbul".toUpperCase(java.util.Locale.ROOT));
        line("String.matches", "a1b2".matches("([a-z]\\d)+"));
        line("String.CASE_INSENSITIVE_ORDER",
                String.CASE_INSENSITIVE_ORDER.compare("A", "a"));
    }

    // -- boxes, parsing, formatting -----------------------------------------
    static void boxesAndParsing() {
        line("Integer.parseInt", Integer.parseInt("-42"));
        line("Integer.parseIntRadix", Integer.parseInt("ff", 16));
        line("Integer.parseIntBad", thrownBy(() -> Integer.parseInt("4x2")));
        line("Integer.toBinaryString", Integer.toBinaryString(10));
        line("Integer.toHexString", Integer.toHexString(-1));
        line("Integer.toOctalString", Integer.toOctalString(64));
        line("Integer.MAX_VALUE", Integer.MAX_VALUE);
        line("Integer.compare", Integer.compare(3, 7));
        line("Integer.bitCount", Integer.bitCount(255));
        line("Integer.reverse", Integer.reverse(1));
        line("Integer.highestOneBit", Integer.highestOneBit(100));
        line("Integer.numberOfLeadingZeros", Integer.numberOfLeadingZeros(1));
        line("Integer.valueOfCacheIdentity", Integer.valueOf(127) == Integer.valueOf(127));
        line("Integer.hashCode", Integer.valueOf(99).hashCode());
        line("Long.parseLong", Long.parseLong("9007199254740993"));
        line("Long.toHexString", Long.toHexString(-1L));
        line("Long.hashCode", Long.hashCode(1L << 40));
        line("Double.parseDouble", Double.parseDouble("1.5e3"));
        line("Double.toString", Double.toString(0.1 + 0.2));
        line("Double.compareNaN", Double.compare(Double.NaN, Double.NaN));
        line("Double.isNaN", Double.isNaN(0.0 / 0.0));
        line("Double.doubleToLongBits", Double.doubleToLongBits(1.5));
        line("Float.toString", Float.toString(1.1f));
        line("Boolean.parseBoolean", Boolean.parseBoolean("TRUE"));
        line("Character.isLetter", Character.isLetter('é'));
        line("Character.toUpperCase", Character.toUpperCase('é'));
        line("Character.digit", Character.digit('f', 16));
        line("Character.getNumericValue", Character.getNumericValue('7'));
        line("Byte.parseByte", Byte.parseByte("-8"));
        line("Short.reverseBytes", Short.reverseBytes((short) 1));
        line("Math.floorDiv", Math.floorDiv(-7, 2));
        line("Math.floorMod", Math.floorMod(-7, 2));
        line("Math.abs", Math.abs(Integer.MIN_VALUE));
        line("Math.round", Math.round(-2.5));
        line("Math.addExact", thrownBy(() -> Math.addExact(Integer.MAX_VALUE, 1)));
        line("Math.sqrt", Math.sqrt(2.0));
        line("Math.pow", Math.pow(2.0, 10.0));
        line("String.formatIntegers", String.format(java.util.Locale.ROOT, "%05d|%+d|%x", 42, 42, 255));
        line("String.formatFloats", String.format(java.util.Locale.ROOT, "%.3f|%e|%g", 1.0 / 3, 1234.5, 0.0001));
        line("String.formatWidth", String.format(java.util.Locale.ROOT, "[%-6s][%6s]", "ab", "cd"));
    }

    // -- StringBuilder / StringJoiner ---------------------------------------
    static void joinersAndBuilders() {
        StringBuilder sb = new StringBuilder("abc");
        sb.append(1).append(2L).append('x').append(true).append(1.5).append(new char[] {'p', 'q'});
        line("StringBuilder.appendChain", sb);
        sb.insert(0, "Z");
        line("StringBuilder.insert", sb);
        sb.replace(0, 1, "Y");
        line("StringBuilder.replace", sb);
        sb.deleteCharAt(0);
        line("StringBuilder.deleteCharAt", sb);
        line("StringBuilder.indexOf", sb.indexOf("x"));
        line("StringBuilder.reverse", new StringBuilder("abc").reverse());
        line("StringBuilder.setLength", setLengthGrows());
        line("StringBuilder.capacityIndependentEquals", sb.toString().equals(sb.toString()));
        line("StringBuilder.deleteBad", thrownBy(() -> new StringBuilder("ab").delete(5, 6)));
        line("StringBuilder.appendNull", new StringBuilder().append((String) null).toString());
        line("StringBuilder.appendSub", new StringBuilder().append("abcdef", 1, 3).toString());

        StringJoiner sj = new StringJoiner(", ", "[", "]");
        line("StringJoiner.empty", sj.toString());
        sj.add("a").add("b");
        line("StringJoiner.filled", sj);
        StringJoiner sje = new StringJoiner(",", "<", ">");
        sje.setEmptyValue("EMPTY");
        line("StringJoiner.emptyValue", sje);
        line("StringJoiner.length", sj.length());
    }

    static String setLengthGrows() {
        StringBuilder b = new StringBuilder("ab");
        b.setLength(4);
        return b.toString().replace('\0', '.');
    }

    // -- streams -------------------------------------------------------------
    static void streamsSurface() {
        List<String> src = List.of("delta", "alpha", "charlie", "bravo");
        line("stream.filterMapSorted",
                src.stream().filter(s -> s.length() > 4).map(String::toUpperCase).sorted().toList());
        line("stream.count", src.stream().count());
        line("stream.reduce", src.stream().reduce("", (a, b) -> a + b.charAt(0)));
        line("stream.anyMatch", src.stream().anyMatch(s -> s.startsWith("a")));
        line("stream.allMatch", src.stream().allMatch(s -> s.length() > 3));
        line("stream.noneMatch", src.stream().noneMatch(String::isEmpty));
        line("stream.findFirst", src.stream().sorted().findFirst());
        line("stream.limitSkip", src.stream().sorted().skip(1).limit(2).toList());
        line("stream.flatMap",
                src.stream().sorted().flatMap(s -> s.chars().limit(1).mapToObj(c -> (char) c)).toList());
        line("stream.distinct", java.util.stream.Stream.of(1, 1, 2, 2, 3).distinct().toList());
        line("stream.collectJoining",
                src.stream().sorted().collect(java.util.stream.Collectors.joining("-", "<", ">")));
        line("stream.collectToMapSorted", new TreeMap<>(
                src.stream().collect(java.util.stream.Collectors.toMap(s -> s, String::length))));
        line("stream.groupingBySorted", new TreeMap<>(
                src.stream().collect(java.util.stream.Collectors.groupingBy(String::length))));
        line("stream.summaryStats", java.util.stream.IntStream.rangeClosed(1, 5).summaryStatistics());
        line("stream.iterateLimit",
                java.util.stream.Stream.iterate(1, x -> x * 2).limit(5).toList());
        line("stream.generateLimit",
                java.util.stream.Stream.generate(() -> "g").limit(2).toList());
        line("stream.mapToIntBoxed", src.stream().mapToInt(String::length).boxed().sorted().toList());
        line("stream.reuseThrows", thrownBy(() -> {
            java.util.stream.Stream<String> once = src.stream();
            once.count();
            once.count();
        }));
        line("stream.emptyReduce", java.util.stream.Stream.<String>empty().reduce((a, b) -> a));
        line("stream.sortedComparator",
                src.stream().sorted(Comparator.comparing(String::length).thenComparing(s -> s)).toList());
        line("stream.peekOrder", peekOrder());
    }

    static String peekOrder() {
        StringBuilder seen = new StringBuilder();
        List.of("a", "b", "c").stream().peek(seen::append).map(String::toUpperCase).toList();
        return seen.toString();
    }

    // -- Comparator combinators ---------------------------------------------
    static void comparators() {
        Comparator<String> byLen = Comparator.comparingInt(String::length);
        line("Comparator.compare", byLen.compare("aa", "b"));
        line("Comparator.reversed", byLen.reversed().compare("aa", "b"));
        line("Comparator.thenComparing",
                byLen.thenComparing(Comparator.<String>naturalOrder()).compare("ab", "aa"));
        line("Comparator.nullsFirst",
                Comparator.nullsFirst(Comparator.<String>naturalOrder()).compare(null, "a"));
        line("Comparator.nullsLast",
                Comparator.nullsLast(Comparator.<String>naturalOrder()).compare(null, "a"));
        line("Comparator.naturalOrderReverse", Comparator.<String>reverseOrder().compare("a", "b"));
        List<String> l = new ArrayList<>(List.of("bb", "a", "ccc"));
        l.sort(byLen.reversed());
        line("List.sortComparator", l);
        line("Comparator.comparingIdentity", byLen.reversed().reversed().compare("aa", "b"));
    }

    // -- BitSet / UUID / Base64 ---------------------------------------------
    static void bitSetAndUuidAndBase64() {
        BitSet bs = new BitSet();
        bs.set(1);
        bs.set(5);
        bs.set(3, 5);
        line("BitSet.toString", bs);
        line("BitSet.cardinality", bs.cardinality());
        line("BitSet.nextSetBit", bs.nextSetBit(2));
        line("BitSet.nextClearBit", bs.nextClearBit(1));
        line("BitSet.length", bs.length());
        BitSet other = new BitSet();
        other.set(3);
        bs.and(other);
        line("BitSet.and", bs);

        UUID u = UUID.fromString("123e4567-e89b-12d3-a456-426614174000");
        line("UUID.toString", u);
        line("UUID.version", u.version());
        line("UUID.variant", u.variant());
        line("UUID.mostSigBits", u.getMostSignificantBits());
        line("UUID.equalsRoundTrip", u.equals(UUID.fromString(u.toString())));
        line("UUID.hashCodeStable", u.hashCode() == UUID.fromString(u.toString()).hashCode());
        line("UUID.nameUUIDFromBytes", UUID.nameUUIDFromBytes("abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        line("UUID.badString", thrownBy(() -> UUID.fromString("nope")));

        byte[] raw = "hello world".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        String enc = java.util.Base64.getEncoder().encodeToString(raw);
        line("Base64.encode", enc);
        line("Base64.roundTrip",
                new String(java.util.Base64.getDecoder().decode(enc), java.nio.charset.StandardCharsets.UTF_8));
        line("Base64.urlEncode",
                java.util.Base64.getUrlEncoder().withoutPadding().encodeToString(new byte[] {(byte) 0xfb, (byte) 0xff}));
        line("Base64.decodeBad", thrownBy(() -> java.util.Base64.getDecoder().decode("!!!")));
    }

    // ========================================================================
    // ROUND 2 — 2026-08-11
    // ========================================================================

    // -- Map default methods -------------------------------------------------
    // `merge` / `compute*` are where the spec says a NULL RESULT REMOVES the
    // mapping. A shadow that stores the null instead leaves a key present
    // with a null value: `containsKey` says true, `get` says null, and every
    // caller that tests `get(k) != null` behaves as if the removal worked.
    // That is the quietest wrong answer in `java.util`. `getOrDefault` over
    // an explicitly-null value is the mirror image — it must answer the
    // stored null, not the default.
    static void mapDefaults() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        line("Map.getOrDefaultPresent", m.getOrDefault("a", 9));
        line("Map.getOrDefaultAbsent", m.getOrDefault("zz", 9));
        line("Map.putIfAbsentPresent", m.putIfAbsent("a", 5));
        line("Map.putIfAbsentAbsent", m.putIfAbsent("b", 2));
        line("Map.afterPutIfAbsent", m);

        line("Map.computeIfAbsentNew", m.computeIfAbsent("c", k -> 3));
        line("Map.computeIfAbsentExisting", m.computeIfAbsent("a", k -> 999));
        line("Map.computeIfAbsentNull", m.computeIfAbsent("d", k -> null));
        line("Map.computeIfAbsentNullCreatesNoEntry", m.containsKey("d"));
        line("Map.computeIfPresentAbsent", m.computeIfPresent("zz", (k, v) -> 1));
        line("Map.computeIfPresentNullRemoves", m.computeIfPresent("c", (k, v) -> null));
        line("Map.afterComputeIfPresentNull", m.containsKey("c"));
        line("Map.computeOnAbsent", m.compute("e", (k, v) -> (v == null ? 0 : v) + 10));
        line("Map.computeNullRemoves", m.compute("e", (k, v) -> null));
        line("Map.afterComputeNull", m.containsKey("e"));

        line("Map.mergeAbsentUsesValue", m.merge("f", 4, Integer::sum));
        line("Map.mergePresentCombines", m.merge("f", 5, Integer::sum));
        line("Map.mergeNullResultRemoves", m.merge("f", 1, (x, y) -> null));
        line("Map.afterMergeNull", m.containsKey("f"));
        line("Map.mergeNullValueThrows", thrownBy(() -> m.merge("g", null, Integer::sum)));

        line("Map.replacePresent", m.replace("a", 42));
        line("Map.replaceAbsent", m.replace("zz", 1));
        line("Map.replaceThreeArgMismatch", m.replace("a", 99, 100));
        line("Map.replaceThreeArgMatch", m.replace("a", 42, 43));
        line("Map.removeKeyValueMismatch", m.remove("a", 99));
        line("Map.removeKeyValueMatch", m.remove("a", 43));
        line("Map.replaceAll", replaceAllResult());
        line("Map.forEachOrder", forEachOrder());
        line("Map.finalContent", m);

        // A HashMap accepts a null key and null values, and every accessor
        // has to tell "absent" apart from "present and null".
        Map<String, String> nm = new HashMap<>();
        nm.put(null, "nullKeyValue");
        nm.put("k", null);
        line("HashMap.nullKeyGet", nm.get(null));
        line("HashMap.nullKeyContains", nm.containsKey(null));
        line("HashMap.nullValueGet", nm.get("k"));
        line("HashMap.nullValueContainsKey", nm.containsKey("k"));
        line("HashMap.getOrDefaultOverNullValue", nm.getOrDefault("k", "DEFAULT"));
        line("HashMap.containsValueNull", nm.containsValue(null));
        line("HashMap.size", nm.size());
        line("HashMap.removeNullKey", nm.remove(null));
        line("HashMap.sizeAfterNullKeyRemove", nm.size());
    }

    static String replaceAllResult() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        m.replaceAll((k, v) -> v * 10);
        return m.toString();
    }

    static String forEachOrder() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("z", 1);
        m.put("y", 2);
        m.put("x", 3);
        StringBuilder sb = new StringBuilder();
        m.forEach((k, v) -> sb.append(k).append(v).append(';'));
        return sb.toString();
    }

    // -- keySet / values / entrySet are VIEWS --------------------------------
    // Round 1 found `TreeMap`'s navigable views answering a snapshot. The
    // same question has never been asked of the three views every Map hands
    // out. A `keySet` that is a copy reads identically until something
    // removes through it or puts behind it.
    static void mapViewWriteThrough() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        Set<String> ks = m.keySet();
        Collection<Integer> vs = m.values();
        Set<Map.Entry<String, Integer>> es = m.entrySet();
        line("keySet.content", ks);
        line("values.content", vs);
        line("entrySet.content", es);
        line("keySet.removeWritesThrough", ks.remove("a") + ":" + m);
        line("values.removeWritesThrough", vs.remove(2) + ":" + m);
        m.put("d", 4);
        m.put("e", 5);
        line("keySet.removeIf", ks.removeIf(k -> k.equals("d")) + ":" + m);
        line("entrySet.removeIf", es.removeIf(e -> e.getValue() == 5) + ":" + m);
        // Live in the other direction too: a put AFTER the view was taken
        // must show up in the view that was handed out before it.
        m.put("z", 26);
        line("keySet.seesLaterPut", ks.contains("z"));
        line("values.seesLaterPut", vs.contains(26));
        line("keySet.sizeAfterLaterPut", ks.size());
        line("entrySet.sizeAfterLaterPut", es.size());
        line("keySet.addUnsupported", thrownBy(() -> m.keySet().add("nope")));
        line("values.addUnsupported", thrownBy(() -> m.values().add(99)));
        line("entrySet.setValueWritesThrough", entrySetSetValue());
        line("keySet.retainAllWritesThrough", retainAllThrough());
        line("values.clearWritesThrough", clearThrough());
        line("values.removeRemovesOneMapping", valuesDuplicates());
        line("entrySet.iteratorRemoveWritesThrough", mapIteratorRemove());
        line("keySet.equalsPlainSet", m.keySet().equals(new HashSet<>(m.keySet())));
        line("TreeMap.keySetIsSorted", new TreeMap<>(Map.of("c", 3, "a", 1, "b", 2)).keySet());
    }

    static String entrySetSetValue() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        for (Map.Entry<String, Integer> e : m.entrySet()) {
            e.setValue(e.getValue() * 100);
        }
        return m.toString();
    }

    static String retainAllThrough() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        boolean changed = m.keySet().retainAll(Set.of("b"));
        return changed + ":" + m;
    }

    static String clearThrough() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        m.values().clear();
        return m + ":" + m.size() + ":" + m.isEmpty();
    }

    static String valuesDuplicates() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 7);
        m.put("b", 7);
        Collection<Integer> vs = m.values();
        boolean removed = vs.remove(7);
        return removed + ":" + vs + ":" + m;
    }

    static String mapIteratorRemove() {
        Map<String, Integer> m = new LinkedHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        Iterator<Map.Entry<String, Integer>> it = m.entrySet().iterator();
        it.next();
        it.remove();
        return m + ":" + m.size();
    }

    // -- List.subList ---------------------------------------------------------
    // Round 1 asked one question of `subList` (does `set` write through) and
    // stopped. The rest of the contract is where a snapshot shows: `add` and
    // `clear` must change the BACKING list's size, a nested sub-view must
    // reach all the way down, and any structural change made behind the view
    // must make every later view operation throw. Each stale-view check is a
    // single call inside `thrownBy` — deliberately NOT a loop, because a
    // loop over a view that never throws is exactly the unbounded shape that
    // killed the first widened run.
    static void subListContracts() {
        List<String> base = new ArrayList<>(List.of("a", "b", "c", "d", "e"));
        List<String> sub = base.subList(1, 4);
        line("subList.content", sub);
        sub.set(0, "B");
        line("subList.setWritesThrough", base);
        sub.add("X");
        line("subList.addWritesThrough", base);
        line("subList.sizeAfterAdd", sub.size());
        line("subList.baseSizeAfterAdd", base.size());
        line("subList.removeWritesThrough", sub.remove("X") + ":" + base);
        List<String> nested = sub.subList(1, 3);
        line("subList.nestedContent", nested);
        nested.clear();
        line("subList.nestedClearWritesThroughToBase", base);
        line("subList.afterNestedClear", sub);
        line("subList.emptyRange", base.subList(1, 1));
        line("subList.emptyRangeIsEmpty", base.subList(1, 1).isEmpty());
        line("subList.reversedRange", thrownBy(() -> base.subList(3, 1)));
        line("subList.pastEnd", thrownBy(() -> base.subList(0, 99)));
        line("subList.negativeStart", thrownBy(() -> base.subList(-1, 1)));

        // Structural modification of the BACKING list invalidates the view.
        List<String> b2 = new ArrayList<>(List.of("p", "q", "r"));
        List<String> s2 = b2.subList(0, 2);
        b2.add("s");
        line("subList.staleSize", thrownBy(s2::size));
        line("subList.staleGet", thrownBy(() -> s2.get(0)));
        line("subList.staleToString", thrownBy(s2::toString));
        line("subList.staleIterator", thrownBy(() -> s2.iterator().next()));

        line("List.of.subList", List.of("1", "2", "3").subList(0, 2));
        line("List.of.subList.addThrows", thrownBy(() -> List.of("1", "2").subList(0, 1).add("x")));
        line("Arrays.asList.subList.setWritesThrough", asListSubListThrough());
        line("subList.sortWritesThrough", subListSort());
        line("subList.equalsPlainList", base.subList(0, 2).equals(new ArrayList<>(base.subList(0, 2))));
    }

    static String asListSubListThrough() {
        String[] backing = {"a", "b", "c"};
        List<String> sub = Arrays.asList(backing).subList(0, 2);
        sub.set(1, "B");
        return Arrays.toString(backing) + ":" + sub;
    }

    static String subListSort() {
        List<Integer> base = new ArrayList<>(List.of(9, 5, 1, 7, 3));
        base.subList(1, 4).sort(Comparator.naturalOrder());
        return base.toString();
    }

    // -- Collections.unmodifiable* / checked* / synchronized* -----------------
    // Two independent questions, and only asking both tells a real wrapper
    // from a defensive copy: do WRITES through it throw, and do READS see the
    // backing collection change underneath? A copy answers the first
    // correctly and the second wrongly, and nothing that only writes will
    // ever notice. `checked*` exists only to throw, so a `checked*` that
    // accepts the wrong type is a guard that is silently not there.
    @SuppressWarnings({"unchecked", "rawtypes"})
    static void wrapperViews() {
        List<String> backing = new ArrayList<>(List.of("a", "b"));
        List<String> un = Collections.unmodifiableList(backing);
        backing.add("c");
        line("unmodifiableList.seesBackingWrite", un);
        line("unmodifiableList.sizeAfterBackingWrite", un.size());
        line("unmodifiableList.set", thrownBy(() -> un.set(0, "z")));
        line("unmodifiableList.removeIf", thrownBy(() -> un.removeIf(s -> true)));
        line("unmodifiableList.sort", thrownBy(() -> un.sort(null)));
        line("unmodifiableList.replaceAll", thrownBy(() -> un.replaceAll(String::toUpperCase)));
        line("unmodifiableList.iteratorRemove", thrownBy(() -> {
            Iterator<String> i = un.iterator();
            i.next();
            i.remove();
        }));
        line("unmodifiableList.subListAdd", thrownBy(() -> un.subList(0, 1).add("z")));
        line("unmodifiableList.equalsBacking", un.equals(backing));
        line("unmodifiableList.contentAfterAll", un);

        Map<String, Integer> mb = new LinkedHashMap<>();
        mb.put("k", 1);
        Map<String, Integer> um = Collections.unmodifiableMap(mb);
        mb.put("k2", 2);
        line("unmodifiableMap.seesBackingWrite", um);
        line("unmodifiableMap.getAfterBackingWrite", um.get("k2"));
        line("unmodifiableMap.entrySetSetValue",
                thrownBy(() -> um.entrySet().iterator().next().setValue(9)));
        line("unmodifiableMap.keySetRemove", thrownBy(() -> um.keySet().remove("k")));
        line("unmodifiableMap.valuesClear", thrownBy(() -> um.values().clear()));
        line("unmodifiableMap.merge", thrownBy(() -> um.merge("k", 1, Integer::sum)));
        line("unmodifiableMap.computeIfAbsent", thrownBy(() -> um.computeIfAbsent("new", k -> 1)));
        line("unmodifiableMap.contentAfterAll", um);

        Set<String> us = Collections.unmodifiableSet(new LinkedHashSet<>(List.of("s1", "s2")));
        line("unmodifiableSet.content", us);
        line("unmodifiableSet.removeAll", thrownBy(() -> us.removeAll(List.of("s1"))));
        line("unmodifiableCollection.content",
                Collections.unmodifiableCollection(new ArrayList<>(List.of("c1"))));
        line("unmodifiableSortedMap.content",
                Collections.unmodifiableSortedMap(new TreeMap<>(Map.of("b", 2, "a", 1))));
        line("unmodifiableList.rewrapIsNewObject", Collections.unmodifiableList(un) == un);

        List<String> checked = Collections.checkedList(new ArrayList<>(), String.class);
        checked.add("fine");
        line("checkedList.goodAdd", checked);
        line("checkedList.badAdd", thrownDetail(() -> ((List) checked).add(Integer.valueOf(1))));
        line("checkedList.contentAfterBadAdd", checked);
        line("checkedList.badSet", thrownBy(() -> ((List) checked).set(0, Integer.valueOf(1))));
        Map<String, Integer> cm = Collections.checkedMap(new LinkedHashMap<>(), String.class, Integer.class);
        line("checkedMap.badValuePut", thrownBy(() -> ((Map) cm).put("k", "notAnInteger")));
        line("checkedMap.badKeyPut", thrownBy(() -> ((Map) cm).put(Integer.valueOf(1), Integer.valueOf(1))));
        line("checkedMap.contentAfterBadPuts", cm);
        Collection<String> cc = Collections.checkedCollection(new ArrayList<>(), String.class);
        line("checkedCollection.badAdd", thrownBy(() -> ((Collection) cc).add(Integer.valueOf(1))));

        Collection<String> csync = Collections.synchronizedCollection(new ArrayList<>(List.of("y")));
        csync.add("z");
        line("synchronizedCollection.content", csync);
        Map<String, Integer> sm = Collections.synchronizedMap(new LinkedHashMap<>());
        sm.put("s", 1);
        sm.put("t", 2);
        line("synchronizedMap.content", sm);
        line("synchronizedMap.sortedKeys", new TreeSet<>(sm.keySet()));
        line("synchronizedMap.getOrDefault", sm.getOrDefault("nope", -1));
        line("Collections.emptyMap.get", Collections.emptyMap().get("x"));
        line("Collections.emptyIterator.hasNext", Collections.emptyIterator().hasNext());
        line("Collections.emptyIterator.next", thrownBy(() -> Collections.emptyIterator().next()));
        line("Collections.emptySet.equalsEmpty", Collections.emptySet().equals(new HashSet<>()));
    }

    // -- NavigableMap / NavigableSet edges -----------------------------------
    // Round 1 established that the navigable views are broken (W7-1). This
    // maps the EXTENT rather than re-reporting it: the accessors on an EMPTY
    // map, where the spec splits null-answering from throwing; the
    // inclusive/exclusive bound overloads round 1 never called; the null and
    // incomparable-element policies, which are the ones a shadow gets wrong
    // by accepting; and `firstEntry`, which must hand back an IMMUTABLE
    // snapshot entry rather than a live one.
    static void navigableEdges() {
        TreeMap<String, Integer> empty = new TreeMap<>();
        line("TreeMap.emptyFirstEntry", empty.firstEntry());
        line("TreeMap.emptyLastEntry", empty.lastEntry());
        line("TreeMap.emptyFloorKey", empty.floorKey("a"));
        line("TreeMap.emptyCeilingEntry", empty.ceilingEntry("a"));
        line("TreeMap.emptyPollFirstEntry", empty.pollFirstEntry());
        line("TreeMap.emptyFirstKeyThrows", thrownBy(empty::firstKey));
        line("TreeMap.emptyLastKeyThrows", thrownBy(empty::lastKey));
        line("TreeMap.nullKeyPut", thrownBy(() -> empty.put(null, 1)));
        line("TreeMap.nullKeyGet", thrownBy(() -> empty.get(null)));
        line("TreeSet.emptyFirstThrows", thrownBy(() -> new TreeSet<String>().first()));
        line("TreeSet.emptyPollFirst", new TreeSet<String>().pollFirst());

        TreeMap<String, Integer> tm = new TreeMap<>(Map.of("a", 1, "b", 2, "c", 3, "d", 4));
        line("TreeMap.headMapInclusive", tm.headMap("c", true));
        line("TreeMap.headMapExclusive", tm.headMap("c", false));
        line("TreeMap.tailMapExclusive", tm.tailMap("b", false));
        line("TreeMap.subMapBothInclusive", tm.subMap("a", true, "c", true));
        line("TreeMap.subMapBothExclusive", tm.subMap("a", false, "c", false));
        line("TreeMap.subMapReversedBounds", thrownBy(() -> tm.subMap("d", "a")));
        line("TreeMap.floorEntry", tm.floorEntry("bb"));
        line("TreeMap.ceilingEntry", tm.ceilingEntry("bb"));
        line("TreeMap.higherEntryAtLast", tm.higherEntry("d"));
        line("TreeMap.lowerEntryAtFirst", tm.lowerEntry("a"));
        line("TreeMap.navigableKeySet", tm.navigableKeySet());
        line("TreeMap.descendingKeySetSize", tm.descendingKeySet().size());
        line("TreeMap.firstEntryIsImmutable", thrownBy(() -> tm.firstEntry().setValue(99)));
        line("TreeMap.entrySetEntryIsLive", treeMapEntrySetSetValue());
        line("TreeMap.descendingWriteThrough", descendingPutThrough());
        line("TreeMap.headMapPutOutOfRange", thrownBy(() -> tm.headMap("c").put("zz", 9)));
        line("TreeMap.contentAfterAll", tm);

        TreeSet<Integer> ts = new TreeSet<>(List.of(1, 3, 5, 7));
        line("TreeSet.headSetInclusive", ts.headSet(5, true));
        line("TreeSet.tailSetExclusive", ts.tailSet(3, false));
        line("TreeSet.subSetInclusiveExclusive", ts.subSet(1, true, 5, false));
        line("TreeSet.higherAtLast", ts.higher(7));
        line("TreeSet.lowerAtFirst", ts.lower(1));
        line("TreeSet.pollLast", ts.pollLast() + ":" + ts);
        line("TreeSet.descendingWriteThrough", descendingSetPollThrough());

        TreeSet<String> rev = new TreeSet<>(Comparator.reverseOrder());
        rev.addAll(List.of("a", "b", "c"));
        line("TreeSet.customComparatorOrder", rev);
        line("TreeSet.comparatorIsReported", rev.comparator() != null);
        line("TreeSet.customFirst", rev.first());
        line("TreeSet.headSetUnderCustomComparator", rev.headSet("b"));
        line("TreeSet.nullAddNaturalOrdering", thrownBy(() -> new TreeSet<String>().add(null)));
        line("TreeSet.incomparableFirstAdd", thrownBy(() -> new TreeSet<Object>().add(new Object())));
        line("TreeMap.comparatorNullForNaturalOrder", new TreeMap<String, Integer>().comparator());
    }

    static String treeMapEntrySetSetValue() {
        TreeMap<String, Integer> tm = new TreeMap<>();
        tm.put("a", 1);
        tm.entrySet().iterator().next().setValue(9);
        return tm.toString();
    }

    static String descendingPutThrough() {
        TreeMap<String, Integer> tm = new TreeMap<>();
        tm.put("a", 1);
        tm.put("b", 2);
        NavigableMap<String, Integer> dm = tm.descendingMap();
        dm.put("c", 3);
        Integer removed = dm.remove("a");
        return dm + ":" + removed + ":" + tm;
    }

    static String descendingSetPollThrough() {
        TreeSet<Integer> ts = new TreeSet<>(List.of(1, 2, 3));
        NavigableSet<Integer> ds = ts.descendingSet();
        Integer polled = ds.pollFirst();
        ds.add(9);
        return polled + ":" + ds + ":" + ts;
    }

    // -- Deque null policy, LIFO ends, occurrence removal --------------------
    // Round 1 drove `ArrayDeque` through one end at a time. The contracts it
    // did not ask are the refusals: `ArrayDeque` forbids null OUTRIGHT
    // because null is its "empty" sentinel, so a shadow that stores one
    // corrupts `poll` for every later caller. `push`/`pop` are the LIFO
    // aliases and go on the FRONT — the direction is invisible until
    // something reads the order back.
    static void dequeEdges() {
        ArrayDeque<String> dq = new ArrayDeque<>();
        line("ArrayDeque.addNull", thrownBy(() -> dq.add(null)));
        line("ArrayDeque.addFirstNull", thrownBy(() -> dq.addFirst(null)));
        line("ArrayDeque.offerNull", thrownBy(() -> dq.offer(null)));
        line("ArrayDeque.sizeAfterRefusedNulls", dq.size());
        dq.push("a");
        dq.push("b");
        line("ArrayDeque.pushIsAddFirst", dq);
        line("ArrayDeque.pop", dq.pop() + ":" + dq);
        dq.addAll(List.of("x", "y", "x", "z"));
        line("ArrayDeque.content", dq);
        line("ArrayDeque.removeFirstOccurrence", dq.removeFirstOccurrence("x") + ":" + dq);
        line("ArrayDeque.removeLastOccurrence", dq.removeLastOccurrence("x") + ":" + dq);
        line("ArrayDeque.descendingIterator", joinIter(dq.descendingIterator()));
        line("ArrayDeque.toArray", Arrays.toString(dq.toArray()));
        line("ArrayDeque.clearThenIsEmpty", clearArrayDeque());
        line("ArrayDeque.growsPastInitialCapacity", dequeGrowth());
        line("ArrayDeque.getFirstOnEmpty", thrownBy(() -> new ArrayDeque<String>().getFirst()));
        line("ArrayDeque.popOnEmpty", thrownBy(() -> new ArrayDeque<String>().pop()));

        LinkedList<String> ll = new LinkedList<>();
        line("LinkedList.acceptsNull", ll.add(null) + ":" + ll.size() + ":" + ll.getFirst());
        line("LinkedList.peekOnEmpty", new LinkedList<String>().peek());
        line("LinkedList.removeFirstOnEmpty", thrownBy(() -> new LinkedList<String>().removeFirst()));
        line("LinkedList.addAtIndex", linkedListAddAt());

        Deque<String> asStack = new ArrayDeque<>();
        asStack.push("1");
        asStack.push("2");
        Stack<String> legacyStack = new Stack<>();
        legacyStack.push("1");
        legacyStack.push("2");
        // Both are LIFO, but their toString ORDER is opposite: `Stack`
        // extends `Vector` and prints bottom-first.
        line("ArrayDequeAsStack.toString", asStack);
        line("Stack.toString", legacyStack);
        line("Stack.peekIsTop", legacyStack.peek());
        line("Stack.search", legacyStack.search("1"));
        line("Stack.popOnEmpty", thrownBy(() -> new Stack<String>().pop()));

        PriorityQueue<String> pq = new PriorityQueue<>(Comparator.reverseOrder());
        pq.addAll(List.of("b", "d", "a", "c"));
        line("PriorityQueue.peekIsHead", pq.peek());
        line("PriorityQueue.customComparatorDrain", drainBounded(pq));
        line("PriorityQueue.nullAdd", thrownBy(() -> new PriorityQueue<String>().add(null)));

        ConcurrentLinkedQueue<String> clq = new ConcurrentLinkedQueue<>(List.of("a", "b"));
        line("ConcurrentLinkedQueue.poll", clq.poll() + ":" + clq);
        line("ConcurrentLinkedQueue.nullOffer", thrownBy(() -> clq.offer(null)));
        line("ConcurrentLinkedQueue.pollEmpty", new ConcurrentLinkedQueue<String>().poll());
    }

    static String clearArrayDeque() {
        ArrayDeque<String> d = new ArrayDeque<>(List.of("a", "b"));
        d.clear();
        return d.size() + ":" + d.isEmpty() + ":" + d.poll();
    }

    /** A deque built with a small capacity must still take more than that. */
    static String dequeGrowth() {
        ArrayDeque<Integer> d = new ArrayDeque<>(2);
        for (int i = 0; i < 40; i++) {
            d.addLast(i);
        }
        return d.size() + ":" + d.peekFirst() + ":" + d.peekLast();
    }

    static String linkedListAddAt() {
        LinkedList<String> l = new LinkedList<>(List.of("a", "c"));
        l.add(1, "b");
        return l + ":" + l.get(1) + ":" + l.indexOf("c");
    }

    // -- EnumMap / EnumSet / Enum --------------------------------------------
    // These key on `ordinal()` and on `getDeclaringClass()`, not on hashCode
    // and not on `getClass()` — and an enum constant WITH A BODY has a
    // different `getClass()` from its enum type. A shadow that reaches for
    // `getClass()` works perfectly until the first constant-specific body.
    static void enumCollections() {
        EnumMap<Color, Integer> em = new EnumMap<>(Color.class);
        em.put(Color.BLUE, 3);
        em.put(Color.RED, 1);
        em.put(Color.VIOLET, 4);
        line("EnumMap.ordinalOrderRegardlessOfInsertion", em);
        line("EnumMap.keySet", em.keySet());
        line("EnumMap.values", em.values());
        line("EnumMap.get", em.get(Color.BLUE));
        line("EnumMap.getAbsent", em.get(Color.GREEN));
        line("EnumMap.nullKeyPut", thrownBy(() -> em.put(null, 0)));
        line("EnumMap.size", em.size());
        line("EnumMap.containsValue", em.containsValue(3));
        line("EnumMap.equalsPlainHashMap", em.equals(new HashMap<>(em)));
        line("EnumMap.removeThenSize", em.remove(Color.RED) + ":" + em.size());

        line("EnumSet.ofOrdinalOrder", EnumSet.of(Color.VIOLET, Color.RED));
        line("EnumSet.allOf", EnumSet.allOf(Color.class));
        line("EnumSet.noneOf", EnumSet.noneOf(Color.class));
        line("EnumSet.range", EnumSet.range(Color.GREEN, Color.VIOLET));
        line("EnumSet.complementOf", EnumSet.complementOf(EnumSet.of(Color.RED)));
        line("EnumSet.copyOfCollection", EnumSet.copyOf(List.of(Color.BLUE, Color.RED)));
        line("EnumSet.iterationIsOrdinal", joinIter(EnumSet.of(Color.VIOLET, Color.GREEN).iterator()));
        line("EnumSet.removeReturns", enumSetRemove());
        line("EnumSet.containsNonEnum", EnumSet.allOf(Color.class).contains("RED"));
        line("EnumSet.equalsPlainSet", EnumSet.of(Color.RED).equals(Set.of(Color.RED)));
        line("EnumSet.retainAll", enumSetRetain());

        line("Enum.valueOf", Color.valueOf("GREEN"));
        line("Enum.valueOfBadName", thrownDetail(() -> Color.valueOf("MAUVE")));
        line("Enum.valueOfNull", thrownBy(() -> Color.valueOf(null)));
        line("Enum.ordinal", Color.BLUE.ordinal());
        line("Enum.name", Color.BLUE.name());
        line("Enum.compareTo", Color.RED.compareTo(Color.BLUE));
        line("Enum.valuesLength", Color.values().length);
        line("Enum.valuesIsAFreshArray", Color.values() != Color.values());
        line("Enum.getDeclaringClass", Color.RED.getDeclaringClass().getName());
        line("Enum.equalsIsIdentity", Color.RED == Color.valueOf("RED"));
        line("Enum.switchDispatch", switchOnColor(Color.GREEN) + ":" + switchOnColor(Color.VIOLET));

        // Constant-specific bodies: `getClass()` is an anonymous subclass,
        // `getDeclaringClass()` is still the enum, and the enum collections
        // must key on the latter.
        line("Enum.constantBodyApply", Op.ADD.apply(2, 3) + ":" + Op.MUL.apply(2, 3));
        line("Enum.constantBodyClassIsEnumClass", Op.ADD.getClass().equals(Op.class));
        line("Enum.constantBodyDeclaringClass", Op.ADD.getDeclaringClass().getSimpleName());
        line("EnumSet.overConstantBodies", EnumSet.allOf(Op.class));
        line("EnumMap.overConstantBodies", new EnumMap<Op, Integer>(Map.of(Op.MUL, 1)));
        line("Enum.constantBodyName", Op.MUL.name() + ":" + Op.MUL.ordinal());
    }

    static String enumSetRemove() {
        EnumSet<Color> s = EnumSet.of(Color.RED, Color.BLUE);
        boolean a = s.remove(Color.RED);
        boolean b = s.remove(Color.GREEN);
        return a + ":" + b + ":" + s;
    }

    static String enumSetRetain() {
        EnumSet<Color> s = EnumSet.allOf(Color.class);
        boolean changed = s.retainAll(EnumSet.of(Color.GREEN, Color.RED));
        return changed + ":" + s;
    }

    static String switchOnColor(Color c) {
        switch (c) {
            case RED:
                return "r";
            case GREEN:
                return "g";
            default:
                return "other";
        }
    }

    enum Color { RED, GREEN, BLUE, VIOLET }

    enum Op {
        ADD {
            int apply(int a, int b) {
                return a + b;
            }
        },
        MUL {
            int apply(int a, int b) {
                return a * b;
            }
        };

        abstract int apply(int a, int b);
    }

    // -- concurrent collections and atomics -----------------------------------
    // `ConcurrentHashMap` REFUSES null keys and null values — that refusal is
    // the whole reason its `get` can be lock-free, and a shadow that accepts
    // one produces a map whose `get` cannot distinguish absent from present.
    // A bounded queue's `offer` must answer `false` when full rather than
    // growing. `CopyOnWriteArrayList`'s iterator must be a SNAPSHOT: it must
    // not see a concurrent add and must not throw — the exact opposite of
    // every other list, so a shared iterator implementation gets one of the
    // two families wrong.
    static void concurrentAndAtomic() {
        ConcurrentHashMap<String, Integer> chm = new ConcurrentHashMap<>();
        chm.put("a", 1);
        line("CHM.get", chm.get("a"));
        line("CHM.nullKeyPut", thrownBy(() -> chm.put(null, 1)));
        line("CHM.nullValuePut", thrownBy(() -> chm.put("k", null)));
        line("CHM.nullKeyGet", thrownBy(() -> chm.get(null)));
        line("CHM.nullValueMerge", thrownBy(() -> chm.merge("a", null, Integer::sum)));
        line("CHM.sizeAfterRefusedNulls", chm.size());
        line("CHM.putIfAbsentPresent", chm.putIfAbsent("a", 9));
        line("CHM.computeIfAbsent", chm.computeIfAbsent("b", k -> 2));
        line("CHM.computeNullRemoves", chm.compute("b", (k, v) -> null) + ":" + chm.containsKey("b"));
        line("CHM.merge", chm.merge("a", 5, Integer::sum));
        line("CHM.getOrDefault", chm.getOrDefault("nope", -1));
        line("CHM.sortedContent", new TreeMap<>(chm));
        line("CHM.reduceValues", chm.reduceValues(Long.MAX_VALUE, Integer::sum));
        line("CHM.keySetViewAdd", thrownBy(() -> chm.keySet().add("z")));
        line("CHM.newKeySet", concurrentKeySet());
        line("CHM.searchKeys", chm.searchKeys(Long.MAX_VALUE, k -> k.equals("a") ? "found" : null));

        CopyOnWriteArrayList<String> cow = new CopyOnWriteArrayList<>(List.of("a", "b"));
        line("COW.content", cow);
        line("COW.snapshotIteratorDoesNotSeeAdds", cowSnapshot());
        line("COW.iteratorRemoveUnsupported", thrownBy(() -> {
            Iterator<String> i = cow.iterator();
            i.next();
            i.remove();
        }));
        line("COW.addIfAbsentDuplicate", cow.addIfAbsent("a"));
        line("COW.addIfAbsentNew", cow.addIfAbsent("zz") + ":" + cow);
        line("COW.addAllAbsent", cow.addAllAbsent(List.of("a", "qq")) + ":" + cow);

        ArrayBlockingQueue<String> abq = new ArrayBlockingQueue<>(2);
        line("ABQ.offerFits", abq.offer("a") + ":" + abq.offer("b"));
        line("ABQ.offerWhenFullIsFalse", abq.offer("c"));
        line("ABQ.sizeAfterRefusedOffer", abq.size());
        line("ABQ.remainingCapacity", abq.remainingCapacity());
        line("ABQ.addWhenFullThrows", thrownBy(() -> abq.add("c")));
        line("ABQ.content", abq);
        line("ABQ.pollThenOffer", abq.poll() + ":" + abq.offer("c") + ":" + abq);
        line("ABQ.drainTo", drainToResult());
        line("ABQ.nullOffer", thrownBy(() -> abq.offer(null)));
        line("ABQ.zeroCapacity", thrownBy(() -> new ArrayBlockingQueue<String>(0)));

        AtomicInteger ai = new AtomicInteger(5);
        line("AtomicInteger.getAndIncrement", ai.getAndIncrement());
        line("AtomicInteger.incrementAndGet", ai.incrementAndGet());
        line("AtomicInteger.compareAndSetMismatch", ai.compareAndSet(99, 0) + ":" + ai.get());
        line("AtomicInteger.compareAndSetMatch", ai.compareAndSet(7, 10) + ":" + ai.get());
        line("AtomicInteger.getAndUpdate", ai.getAndUpdate(x -> x * 2) + ":" + ai.get());
        line("AtomicInteger.accumulateAndGet", ai.accumulateAndGet(3, Integer::sum));
        line("AtomicInteger.getAndSet", ai.getAndSet(-1) + ":" + ai.get());
        line("AtomicInteger.toString", ai.toString());
        line("AtomicInteger.intValueOverflow", new AtomicInteger(Integer.MAX_VALUE).incrementAndGet());
        line("AtomicLong.addAndGet", new AtomicLong(1L << 40).addAndGet(1));
        line("AtomicBoolean.compareAndSet", atomicBoolean());
        line("AtomicReference.updateAndGet", new AtomicReference<>("a").updateAndGet(s -> s + "!"));
        line("AtomicReference.compareAndSetIsIdentity", atomicReferenceIdentity());
        line("LongAdder.sum", longAdder());
    }

    static String concurrentKeySet() {
        Set<String> s = ConcurrentHashMap.newKeySet();
        s.add("a");
        boolean dup = s.add("a");
        return s + ":" + dup + ":" + s.size();
    }

    /**
     * The COW iterator is a snapshot, so this terminates on a correct VM after
     * exactly the snapshot's length. BOUNDED anyway: on a VM whose iterator is
     * live over the backing array, appending inside the loop never terminates
     * and would take the rest of the transcript with it.
     */
    static String cowSnapshot() {
        CopyOnWriteArrayList<String> cow = new CopyOnWriteArrayList<>(List.of("a", "b"));
        StringBuilder sb = new StringBuilder();
        Iterator<String> it = cow.iterator();
        int guard = 0;
        while (it.hasNext()) {
            if (++guard > 100) {
                sb.append("unbounded-after-100");
                break;
            }
            sb.append(it.next());
            cow.add("x");
        }
        return sb + ":" + cow.size();
    }

    static String drainToResult() {
        ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(4);
        q.add("a");
        q.add("b");
        List<String> sink = new ArrayList<>();
        int n = q.drainTo(sink);
        return n + ":" + sink + ":" + q.size();
    }

    static String atomicBoolean() {
        AtomicBoolean b = new AtomicBoolean();
        boolean first = b.compareAndSet(false, true);
        boolean second = b.compareAndSet(false, true);
        return first + ":" + second + ":" + b.get();
    }

    static String atomicReferenceIdentity() {
        String a = new String("v");
        String equalButNotSame = new String("v");
        AtomicReference<String> r = new AtomicReference<>(a);
        boolean byEqualValue = r.compareAndSet(equalButNotSame, "z");
        boolean bySameRef = r.compareAndSet(a, "z");
        return byEqualValue + ":" + bySameRef + ":" + r.get();
    }

    static String longAdder() {
        LongAdder la = new LongAdder();
        la.increment();
        la.add(41);
        return la.sum() + ":" + la.intValue() + ":" + la;
    }

    // -- BigDecimal / BigInteger ----------------------------------------------
    // `BigDecimal.equals` compares SCALE and `compareTo` does not, so
    // `1.10` and `1.1` are unequal but compare equal — a shadow that
    // collapses the two makes a `HashSet<BigDecimal>` silently deduplicate.
    // `divide` with no exact quotient MUST throw rather than pick a
    // precision, which is the same "fabricated success" shape recorded in
    // W2-7.
    static void bigNumbers() {
        BigDecimal a = new BigDecimal("1.10");
        BigDecimal b = new BigDecimal("1.1");
        line("BigDecimal.toString", a);
        line("BigDecimal.scale", a.scale() + ":" + b.scale());
        line("BigDecimal.precision", a.precision());
        line("BigDecimal.unscaledValue", a.unscaledValue());
        line("BigDecimal.equalsComparesScale", a.equals(b));
        line("BigDecimal.compareToIgnoresScale", a.compareTo(b));
        line("BigDecimal.hashCodeTracksScale", a.hashCode() == b.hashCode());
        line("BigDecimal.setDedupes", new HashSet<>(List.of(a, b)).size());
        line("BigDecimal.add", a.add(b));
        line("BigDecimal.subtract", a.subtract(b));
        line("BigDecimal.multiplyScaleAdds", a.multiply(b));
        line("BigDecimal.divideExact", new BigDecimal("10").divide(new BigDecimal("4")));
        line("BigDecimal.divideNonTerminating",
                thrownBy(() -> new BigDecimal("1").divide(new BigDecimal("3"))));
        line("BigDecimal.divideByZero",
                thrownBy(() -> BigDecimal.ONE.divide(BigDecimal.ZERO)));
        line("BigDecimal.divideRounded",
                new BigDecimal("1").divide(new BigDecimal("3"), 5, RoundingMode.HALF_UP));
        line("BigDecimal.setScaleHalfEven", new BigDecimal("2.5").setScale(0, RoundingMode.HALF_EVEN));
        line("BigDecimal.setScaleHalfEvenOdd", new BigDecimal("3.5").setScale(0, RoundingMode.HALF_EVEN));
        line("BigDecimal.setScaleHalfUp", new BigDecimal("2.5").setScale(0, RoundingMode.HALF_UP));
        line("BigDecimal.setScaleFloorNegative", new BigDecimal("-2.5").setScale(0, RoundingMode.FLOOR));
        line("BigDecimal.setScaleUnnecessary",
                thrownBy(() -> new BigDecimal("2.5").setScale(0, RoundingMode.UNNECESSARY)));
        line("BigDecimal.stripTrailingZeros", new BigDecimal("600.0").stripTrailingZeros());
        line("BigDecimal.stripThenPlainString", new BigDecimal("600.0").stripTrailingZeros().toPlainString());
        line("BigDecimal.negativeZeroStrip", new BigDecimal("0.000").stripTrailingZeros());
        line("BigDecimal.fromDoubleIsExact", new BigDecimal(0.1));
        line("BigDecimal.valueOfDoubleUsesToString", BigDecimal.valueOf(0.1));
        line("BigDecimal.movePointLeft", new BigDecimal("123").movePointLeft(2));
        line("BigDecimal.pow", new BigDecimal("1.5").pow(3));
        line("BigDecimal.intValueExactOnFraction",
                thrownBy(() -> new BigDecimal("1.5").intValueExact()));
        line("BigDecimal.intValueTruncates", new BigDecimal("1.9").intValue());
        line("BigDecimal.toEngineeringString", new BigDecimal("1E+9").toEngineeringString());
        line("BigDecimal.badString", thrownBy(() -> new BigDecimal("1.2.3")));

        line("BigInteger.pow", BigInteger.TWO.pow(100));
        line("BigInteger.modPow", BigInteger.valueOf(7).modPow(BigInteger.valueOf(128), BigInteger.valueOf(13)));
        line("BigInteger.gcd", BigInteger.valueOf(462).gcd(BigInteger.valueOf(1071)));
        line("BigInteger.toStringRadix", new BigInteger("255").toString(16));
        line("BigInteger.fromRadixString", new BigInteger("ff", 16));
        line("BigInteger.divideByZero", thrownBy(() -> BigInteger.ONE.divide(BigInteger.ZERO)));
        line("BigInteger.modNegative", BigInteger.valueOf(-7).mod(BigInteger.valueOf(3)));
        line("BigInteger.remainderNegative", BigInteger.valueOf(-7).remainder(BigInteger.valueOf(3)));
        line("BigInteger.shiftLeft", BigInteger.ONE.shiftLeft(70));
        line("BigInteger.bitLengthAndCount", BigInteger.valueOf(255).bitLength() + ":" + BigInteger.valueOf(255).bitCount());
        line("BigInteger.testBit", BigInteger.valueOf(8).testBit(3));
        line("BigInteger.signumNegate", BigInteger.valueOf(-5).signum() + ":" + BigInteger.valueOf(-5).negate());
        line("BigInteger.longValueExactOverflow",
                thrownBy(() -> BigInteger.TWO.pow(100).longValueExact()));
        line("BigInteger.longValueTruncates", BigInteger.TWO.pow(64).add(BigInteger.ONE).longValue());
        line("BigInteger.badString", thrownBy(() -> new BigInteger("12x")));
        line("BigInteger.compareTo", BigInteger.TEN.compareTo(BigInteger.TWO));
        line("BigInteger.equalsAcrossConstruction", BigInteger.TEN.equals(new BigInteger("10")));
    }

    // -- java.util.regex -------------------------------------------------------
    // Round 1 reached the regex engine only through `String.matches` and
    // `replaceAll`, which answer a boolean and a string. `Matcher` carries a
    // STATE MACHINE — group positions, a find cursor, an "no match yet" state
    // that must make `group()` throw — and that is where a shadow answers
    // quietly: an empty group, a zero offset, a `group()` before any match
    // that returns "" instead of throwing. Both find loops are bounded: a
    // `find` that returns true without advancing does not terminate.
    static void regexSurface() {
        Pattern p = Pattern.compile("(\\w+)@(\\w+)\\.com");
        line("Matcher.groupBeforeFindThrows", thrownBy(() -> p.matcher("x").group()));
        line("Matcher.startBeforeFindThrows", thrownBy(() -> p.matcher("x").start()));
        Matcher m = p.matcher("mail a@b.com and c@d.com here");
        line("Matcher.find1", m.find() + ":" + m.group() + ":" + m.group(1) + ":" + m.group(2)
                + ":" + m.start() + ":" + m.end());
        line("Matcher.find2", m.find() + ":" + m.group() + ":" + m.start());
        line("Matcher.findExhausted", m.find());
        line("Matcher.groupCount", m.groupCount());
        line("Matcher.groupAfterFailedFind", thrownBy(m::group));
        line("Matcher.resetRestartsFind", m.reset().find() + ":" + m.group());
        line("Matcher.groupIndexPastGroupCount", thrownBy(() -> {
            Matcher mm = p.matcher("a@b.com");
            mm.matches();
            mm.group(9);
        }));
        line("Matcher.findAllBounded", findAllBounded(p, "a@b.com c@d.com e@f.com"));
        line("Matcher.namedGroups", namedGroup());
        line("Matcher.unmatchedOptionalGroupIsNull", unmatchedGroup());
        line("Matcher.matchesVsFind",
                p.matcher("a@b.com").matches() + ":" + p.matcher("x a@b.com").matches());
        line("Matcher.lookingAt",
                Pattern.compile("ab").matcher("abc").lookingAt() + ":" + Pattern.compile("bc").matcher("abc").lookingAt());
        line("Matcher.hitEnd", matcherRegion());
        line("Matcher.replaceAllBackref", Pattern.compile("(a)(b)").matcher("abab").replaceAll("$2$1"));
        line("Matcher.replaceFirst", Pattern.compile("a").matcher("aaa").replaceFirst("X"));
        line("Matcher.appendReplacement", appendReplacementResult());
        line("Matcher.quoteReplacement",
                Pattern.compile("x").matcher("x").replaceAll(Matcher.quoteReplacement("$1")));
        line("Matcher.badReplacementRef",
                thrownBy(() -> Pattern.compile("(a)").matcher("a").replaceAll("$7")));
        line("Matcher.results", p.matcher("a@b.com c@d.com").results().count());

        line("Pattern.quoteDefeatsMetachars",
                Pattern.compile(Pattern.quote("a.c")).matcher("abc").find());
        line("Pattern.splitLimit2", Arrays.toString(Pattern.compile(",").split("a,b,,c", 2)));
        line("Pattern.splitDropsTrailingEmpties", Arrays.toString(Pattern.compile(",").split("a,b,,")));
        line("Pattern.splitKeepsTrailingEmpties", Arrays.toString(Pattern.compile(",").split("a,b,,", -1)));
        line("Pattern.splitZeroWidth", Arrays.toString("abc".split("")));
        line("Pattern.splitLeadingEmpty", Arrays.toString("-a-b".split("-")));
        line("Pattern.badSyntax", thrownBy(() -> Pattern.compile("a[")));
        line("Pattern.caseInsensitiveFlag",
                Pattern.compile("abc", Pattern.CASE_INSENSITIVE).matcher("ABC").matches());
        line("Pattern.dotallFlag",
                Pattern.compile("a.b", Pattern.DOTALL).matcher("a\nb").matches());
        line("Pattern.multilineFlag",
                Pattern.compile("^b", Pattern.MULTILINE).matcher("a\nb").find());
        line("Pattern.matchesStatic", Pattern.matches("\\d+", "123"));
        line("Pattern.asPredicate", Pattern.compile("\\d").asPredicate().test("a1"));
        line("Pattern.asMatchPredicate", Pattern.compile("\\d").asMatchPredicate().test("a1"));
        line("Pattern.toStringIsThePattern", Pattern.compile("a+b").toString());
        line("Pattern.backreference", Pattern.compile("(ab)\\1").matcher("abab").matches());
        line("Pattern.lookahead", Pattern.compile("a(?=b)").matcher("ab").find());
        line("Pattern.lookbehind",
                Arrays.toString(Pattern.compile("(?<=,)").split("a,b,c")));
        line("Pattern.unicodeClass", Pattern.compile("\\p{L}+").matcher("héllo").matches());
        line("Pattern.greedyVsReluctant", greedyVsReluctant());
    }

    /** BOUNDED: a zero-width match that does not advance never terminates. */
    static String findAllBounded(Pattern p, String input) {
        Matcher m = p.matcher(input);
        StringBuilder sb = new StringBuilder();
        int guard = 0;
        while (m.find()) {
            if (++guard > 100) {
                sb.append("unbounded-find-after-100");
                break;
            }
            sb.append(m.group()).append(';');
        }
        return sb.toString();
    }

    static String namedGroup() {
        Matcher m = Pattern.compile("(?<user>\\w+)@(?<host>\\w+)").matcher("bob@example");
        if (!m.find()) {
            return "no-match";
        }
        return m.group("user") + "/" + m.group("host") + "/" + m.start("host");
    }

    static String unmatchedGroup() {
        Matcher m = Pattern.compile("(a)(b)?").matcher("a");
        if (!m.find()) {
            return "no-match";
        }
        return m.group(1) + "/" + m.group(2) + "/" + m.start(2) + "/" + m.end(2);
    }

    static String matcherRegion() {
        Matcher m = Pattern.compile("abc").matcher("xxabcxx");
        m.region(2, 5);
        boolean matched = m.matches();
        return matched + "/" + m.regionStart() + "/" + m.regionEnd() + "/" + m.hitEnd();
    }

    static String appendReplacementResult() {
        Matcher m = Pattern.compile("cat").matcher("one cat two cats");
        StringBuffer sb = new StringBuffer();
        int guard = 0;
        while (m.find()) {
            if (++guard > 100) {
                sb.append("unbounded-after-100");
                break;
            }
            m.appendReplacement(sb, "dog");
        }
        m.appendTail(sb);
        return sb.toString();
    }

    static String greedyVsReluctant() {
        Matcher g = Pattern.compile("<(.+)>").matcher("<a><b>");
        Matcher r = Pattern.compile("<(.+?)>").matcher("<a><b>");
        return (g.find() ? g.group(1) : "?") + ":" + (r.find() ? r.group(1) : "?");
    }

    // -- java.util.Formatter conversions --------------------------------------
    // Round 1 asked for three float conversions and TWO of them were wrong
    // (`%e` and `%g`, W7-1). A seam with that hit rate deserves the rest of
    // its conversion table rather than three more samples. Everything here
    // pins an explicit Locale; `%n` is deliberately absent, because it
    // expands to the PLATFORM line separator and would manufacture a
    // divergence between a Windows oracle and a Linux run that says nothing
    // about the VM.
    static void formatConversions() {
        Locale r = Locale.ROOT;
        line("format.sNull", String.format(r, "[%s]", (Object) null));
        line("format.sUpper", String.format(r, "%S", "ab"));
        line("format.sPrecisionTruncates", String.format(r, "%.2s", "abcdef"));
        line("format.booleanOfNullAndObject", String.format(r, "%b|%b|%b", true, null, "x"));
        line("format.charFromCharAndInt", String.format(r, "%c|%c", 'x', 65));
        line("format.charFromSupplementary", String.format(r, "%c", 0x1F600));
        line("format.grouping", String.format(r, "%,d", 1234567));
        line("format.parenthesisedNegative", String.format(r, "%(d|%(d", -5, 5));
        line("format.zeroPadNegativeFloat", String.format(r, "%08.2f", -3.14159));
        line("format.zeroPadInt", String.format(r, "%08d|%-8d|", -42, -42));
        line("format.argumentIndex", String.format(r, "%1$s-%1$s-%2$s", "a", "b"));
        line("format.previousArgument", String.format(r, "%s-%<s", "a"));
        line("format.literalPercent", String.format(r, "100%%"));
        line("format.hexAndOctal", String.format(r, "%X|%#x|%o|%#o", 255, 255, 8, 8));
        line("format.hexOfNegative", String.format(r, "%x|%x", -1, -1L));
        line("format.scientific", String.format(r, "%.2e|%.0e|%E", 12345.6789, 0.5, 0.000123));
        line("format.general", String.format(r, "%g|%.3g|%g", 0.00001234, 123456.0, 100.0));
        line("format.hexFloat", String.format(r, "%a", 1.0));
        line("format.floatSpecials", String.format(r, "%f|%e|%.1f|%f", Double.NaN,
                Double.POSITIVE_INFINITY, -0.0, Double.NEGATIVE_INFINITY));
        line("format.floatRoundingHalfUp", String.format(r, "%.1f|%.1f|%.0f", 0.25, 0.35, 0.5));
        line("format.bigDecimalPrecision", String.format(r, "%.2f|%,.2f", new BigDecimal("2.345"),
                new BigDecimal("1234567.891")));
        line("format.bigInteger", String.format(r, "%d|%x", BigInteger.TWO.pow(70), BigInteger.valueOf(255)));
        line("format.widthOnNull", String.format(r, "[%10s]", (Object) null));
        line("format.plusFlag", String.format(r, "%+d|%+.2f| %d", 42, 1.5, 42));
        line("format.unknownConversion", thrownDetail(() -> String.format(r, "%q", 1)));
        line("format.missingArgument", thrownBy(() -> String.format(r, "%s %s", "only")));
        line("format.wrongArgumentType", thrownBy(() -> String.format(r, "%d", "notANumber")));
        line("format.illegalFlagCombination", thrownBy(() -> String.format(r, "%-08d", 1)));
        line("format.precisionOnInteger", thrownBy(() -> String.format(r, "%.2d", 1)));
        line("format.localeUS", String.format(Locale.US, "%,.2f", 1234.5));
        line("format.localeGermany", String.format(Locale.GERMANY, "%,.2f", 1234.5));
        line("format.localeFrance", String.format(Locale.FRANCE, "%,d", 1234567));
        line("format.formatterAppendable", formatterToBuilder());
    }

    static String formatterToBuilder() {
        StringBuilder sb = new StringBuilder();
        try (Formatter f = new Formatter(sb, Locale.ROOT)) {
            f.format("%s=%03d;", "k", 7);
            f.format("%.2f", 1.005);
        }
        return sb.toString();
    }

    // -- java.time -------------------------------------------------------------
    // Date arithmetic across a month end is the classic quiet wrong answer:
    // Jan 31 plus one month is Feb 29 in a leap year and Feb 28 otherwise,
    // and an implementation that adds 30 days answers something plausible
    // every time. Fixed offsets only, never a named region and never `now()`:
    // a tzdb version difference between the oracle host and the run host
    // would be a false divergence, and this probe's job is to produce true
    // ones.
    static void timeSurface() {
        LocalDate jan31 = LocalDate.of(2024, 1, 31);
        line("LocalDate.toString", jan31);
        line("LocalDate.plusMonthsClampsToShorterMonth", jan31.plusMonths(1));
        line("LocalDate.plusMonthsNonLeapYear", LocalDate.of(2023, 1, 31).plusMonths(1));
        line("LocalDate.plusMonthsIsNotThirtyDays", jan31.plusDays(30));
        line("LocalDate.roundTripIsNotIdentity", jan31.plusMonths(1).minusMonths(1));
        line("LocalDate.plusDaysAcrossYear", LocalDate.of(2023, 12, 31).plusDays(1));
        line("LocalDate.minusYearsFromLeapDay", LocalDate.of(2024, 2, 29).minusYears(1));
        line("LocalDate.isLeapYear", jan31.isLeapYear() + ":" + LocalDate.of(1900, 1, 1).isLeapYear()
                + ":" + LocalDate.of(2000, 1, 1).isLeapYear());
        line("LocalDate.lengthOfMonth", jan31.lengthOfMonth() + ":" + LocalDate.of(2023, 2, 1).lengthOfMonth());
        line("LocalDate.dayOfWeek", jan31.getDayOfWeek());
        line("LocalDate.dayOfYearAfterLeapDay", LocalDate.of(2024, 3, 1).getDayOfYear());
        line("LocalDate.month", jan31.getMonth() + ":" + jan31.getMonthValue());
        line("LocalDate.withDayOfMonthOutOfRange",
                thrownBy(() -> LocalDate.of(2024, 2, 1).withDayOfMonth(30)));
        line("LocalDate.ofBadMonth", thrownBy(() -> LocalDate.of(2024, 13, 1)));
        line("LocalDate.ofFeb30", thrownBy(() -> LocalDate.of(2023, 2, 29)));
        line("LocalDate.parseBadDay", thrownBy(() -> LocalDate.parse("2024-02-30")));
        line("LocalDate.parseUnpadded", thrownBy(() -> LocalDate.parse("2024-2-3")));
        line("LocalDate.compareAndEquals",
                jan31.compareTo(LocalDate.of(2024, 2, 1)) + ":" + jan31.equals(LocalDate.of(2024, 1, 31)));
        line("LocalDate.until", LocalDate.of(2024, 1, 31).until(LocalDate.of(2024, 3, 1)));
        line("ChronoUnit.daysBetween",
                ChronoUnit.DAYS.between(LocalDate.of(2024, 1, 31), LocalDate.of(2024, 3, 1)));
        line("ChronoUnit.monthsBetweenIsWhole",
                ChronoUnit.MONTHS.between(LocalDate.of(2024, 1, 31), LocalDate.of(2024, 2, 29)));
        line("YearMonth.atEndOfMonth", YearMonth.of(2024, 2).atEndOfMonth());

        line("Period.parse", Period.parse("P1Y2M3D"));
        line("Period.normalized", Period.of(1, 13, 0).normalized());
        line("Period.toTotalMonths", Period.of(1, 13, 0).toTotalMonths());
        line("Period.parseBad", thrownBy(() -> Period.parse("1Y")));
        line("Duration.parse", Duration.parse("PT1H30M10.5S"));
        line("Duration.toStringOfSeconds", Duration.ofSeconds(3661));
        line("Duration.toStringNegative", Duration.ofSeconds(-1));
        line("Duration.toStringZero", Duration.ZERO);
        line("Duration.plusAndToMillis", Duration.ofMinutes(1).plusSeconds(30).toMillis());
        line("Duration.dividedBy", Duration.ofHours(1).dividedBy(7));
        line("Duration.parseBad", thrownBy(() -> Duration.parse("1h")));
        line("Duration.between", Duration.between(Instant.ofEpochSecond(0), Instant.ofEpochSecond(90)));

        LocalDateTime ldt = LocalDateTime.of(2024, 1, 31, 13, 5, 0);
        line("LocalDateTime.toStringDropsZeroSeconds", ldt);
        line("LocalDateTime.withSeconds", ldt.withSecond(7));
        line("LocalTime.toStringDropsZeroSeconds", LocalTime.of(1, 2, 0));
        line("LocalTime.ofNanoOfDay", LocalTime.ofNanoOfDay(1L));
        line("Instant.epoch", Instant.ofEpochSecond(0));
        line("Instant.plusNanos", Instant.ofEpochSecond(0).plusNanos(1));
        line("Instant.toEpochMilli", Instant.ofEpochSecond(1, 500_000_000).toEpochMilli());
        line("ZonedDateTime.atFixedOffset", ldt.atZone(ZoneOffset.UTC));
        line("ZonedDateTime.offsetShift",
                ZonedDateTime.of(ldt, ZoneOffset.UTC).withZoneSameInstant(ZoneOffset.ofHours(2)));
        line("OffsetDateTime.toInstant", ldt.toInstant(ZoneOffset.ofHours(-5)));

        line("DateTimeFormatter.ISO_DATE", DateTimeFormatter.ISO_DATE.format(jan31));
        line("DateTimeFormatter.ISO_LOCAL_DATE_TIME", DateTimeFormatter.ISO_LOCAL_DATE_TIME.format(ldt));
        line("DateTimeFormatter.numericPattern",
                DateTimeFormatter.ofPattern("yyyy/MM/dd HH:mm:ss", Locale.ROOT).format(ldt));
        line("DateTimeFormatter.textPattern",
                DateTimeFormatter.ofPattern("EEEE, MMMM d, yyyy", Locale.US).format(jan31));
        line("DateTimeFormatter.shortTextPattern",
                DateTimeFormatter.ofPattern("EEE MMM d", Locale.US).format(jan31));
        line("DateTimeFormatter.twelveHourClock",
                DateTimeFormatter.ofPattern("hh:mm a", Locale.US).format(ldt));
        line("DateTimeFormatter.parseRoundTrip", formatterRoundTrip());
        line("DateTimeFormatter.parseWrongPattern",
                thrownBy(() -> LocalDate.parse("31/01/2024", DateTimeFormatter.ofPattern("yyyy-MM-dd", Locale.ROOT))));
        line("DateTimeFormatter.badPattern", thrownBy(() -> DateTimeFormatter.ofPattern("yyyy-MM-dd'", Locale.ROOT)));
        line("DateTimeFormatter.formatWrongTemporal",
                thrownBy(() -> DateTimeFormatter.ISO_LOCAL_TIME.format(jan31)));
    }

    static String formatterRoundTrip() {
        DateTimeFormatter f = DateTimeFormatter.ofPattern("dd/MM/yyyy", Locale.ROOT);
        String text = f.format(LocalDate.of(2024, 3, 9));
        return text + ":" + LocalDate.parse(text, f);
    }

    // -- java.text -------------------------------------------------------------
    // `DecimalFormat`'s DEFAULT rounding is HALF_EVEN, not HALF_UP: `2.345`
    // and `2.355` do not round the same way, and a shadow that rounds
    // half-up produces money that is off by a cent in half the cases and
    // right in the other half. Symbols and locale are pinned so the only
    // variable is the implementation.
    static void textFormatting() {
        DecimalFormatSymbols sym = DecimalFormatSymbols.getInstance(Locale.ROOT);
        DecimalFormat df = new DecimalFormat("#,##0.00", sym);
        line("DecimalFormat.basic", df.format(1234.5));
        line("DecimalFormat.negative", df.format(-1234.567));
        // A tie fed in as a DOUBLE is not a tie — 2.345 is stored as
        // 2.34499999..., so half-even and half-up answer the same thing and
        // the pair cannot tell the two modes apart. Feed the tie as an EXACT
        // BigDecimal as well, which is the only form where the default
        // (HALF_EVEN) and HALF_UP have to disagree.
        line("DecimalFormat.doubleTieRounding", df.format(2.345) + ":" + df.format(2.355));
        line("DecimalFormat.exactTieDefaultIsHalfEven",
                df.format(new BigDecimal("2.345")) + ":" + df.format(new BigDecimal("2.355")));
        line("DecimalFormat.exactTieHalfUp", halfUpFormat());
        line("DecimalFormat.roundingModeIsReported", df.getRoundingMode());
        line("DecimalFormat.optionalDigits", new DecimalFormat("#.##", sym).format(1.0) + ":"
                + new DecimalFormat("#.##", sym).format(1.005));
        line("DecimalFormat.percentPattern", new DecimalFormat("#0.0%", sym).format(0.1234));
        line("DecimalFormat.scientificPattern", new DecimalFormat("0.###E0", sym).format(12345.0));
        line("DecimalFormat.negativeSubpattern",
                new DecimalFormat("#,##0.00;(#,##0.00)", sym).format(-5.5));
        line("DecimalFormat.groupingSize", df.getGroupingSize() + ":" + df.isGroupingUsed());
        line("DecimalFormat.toPattern", df.toPattern());
        line("DecimalFormat.parse", valueOrThrow(() -> df.parse("1,234.50")));
        line("DecimalFormat.parseTrailingGarbage", valueOrThrow(() -> df.parse("12abc")));
        line("DecimalFormat.parseNotANumber", valueOrThrow(() -> df.parse("abc")));
        line("DecimalFormat.parseIntegerOnly", parseIntegerOnly());
        line("DecimalFormat.formatBigDecimalExactly", df.format(new BigDecimal("12345.675")));
        line("DecimalFormat.formatLong", df.format(Long.MIN_VALUE));

        line("NumberFormat.integerInstanceRounds",
                NumberFormat.getIntegerInstance(Locale.US).format(1234567.6));
        line("NumberFormat.percentInstance", NumberFormat.getPercentInstance(Locale.US).format(0.755));
        line("NumberFormat.currencyUS", NumberFormat.getCurrencyInstance(Locale.US).format(1234.5));
        line("NumberFormat.currencyNegativeUS", NumberFormat.getCurrencyInstance(Locale.US).format(-1234.5));
        line("NumberFormat.maxFractionDigits", maxFractionDigits());
        line("NumberFormat.defaultMaxFraction",
                NumberFormat.getInstance(Locale.US).format(1.23456789));

        line("MessageFormat.simple",
                new MessageFormat("{0} has {1} items", Locale.ROOT).format(new Object[] {"cart", 3}));
        line("MessageFormat.numberSubformat",
                new MessageFormat("{0,number,#.##}", Locale.ROOT).format(new Object[] {1.005}));
        line("MessageFormat.quotedBrace",
                new MessageFormat("'{'0'}' is {0}", Locale.ROOT).format(new Object[] {"x"}));
        line("MessageFormat.choiceSubformat",
                new MessageFormat("{0,choice,0#none|1#one|1<many}", Locale.ROOT).format(new Object[] {2}));
        line("MessageFormat.missingArgument",
                new MessageFormat("{0} {1}", Locale.ROOT).format(new Object[] {"a"}));
        line("MessageFormat.reorderedIndices",
                new MessageFormat("{1}{0}", Locale.ROOT).format(new Object[] {"a", "b"}));

        line("SimpleDateFormat.utc", sdfUtc());
        line("SimpleDateFormat.parseRoundTrip", valueOrThrow(ShadowDifferentialProbe::sdfParse));
        line("SimpleDateFormat.lenientAcceptsOverflow", sdfLenient());
        line("SimpleDateFormat.strictRejectsOverflow", sdfStrict());
    }

    static String halfUpFormat() {
        DecimalFormat f = new DecimalFormat("#,##0.00", DecimalFormatSymbols.getInstance(Locale.ROOT));
        f.setRoundingMode(RoundingMode.HALF_UP);
        return f.format(new BigDecimal("2.345")) + ":" + f.format(new BigDecimal("2.355"));
    }

    static String parseIntegerOnly() {
        DecimalFormat f = new DecimalFormat("#0.00", DecimalFormatSymbols.getInstance(Locale.ROOT));
        f.setParseIntegerOnly(true);
        return valueOrThrow(() -> f.parse("12.75"));
    }

    static String maxFractionDigits() {
        NumberFormat nf = NumberFormat.getInstance(Locale.US);
        nf.setMaximumFractionDigits(3);
        nf.setMinimumFractionDigits(2);
        return nf.format(1.0) + ":" + nf.format(1.23456);
    }

    static SimpleDateFormat utcFormat() {
        SimpleDateFormat sdf = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss z", Locale.US);
        sdf.setTimeZone(TimeZone.getTimeZone("UTC"));
        return sdf;
    }

    static String sdfUtc() {
        SimpleDateFormat sdf = utcFormat();
        return sdf.format(new Date(0L)) + ":" + sdf.format(new Date(1_000_000_000_000L));
    }

    static Object sdfParse() throws Exception {
        return utcFormat().parse("2024-02-29 12:00:00 UTC").getTime();
    }

    static String sdfLenient() {
        SimpleDateFormat sdf = new SimpleDateFormat("yyyy-MM-dd", Locale.US);
        sdf.setTimeZone(TimeZone.getTimeZone("UTC"));
        sdf.setLenient(true);
        return valueOrThrow(() -> sdf.format(sdf.parse("2023-02-30")));
    }

    static String sdfStrict() {
        SimpleDateFormat sdf = new SimpleDateFormat("yyyy-MM-dd", Locale.US);
        sdf.setTimeZone(TimeZone.getTimeZone("UTC"));
        sdf.setLenient(false);
        return valueOrThrow(() -> sdf.parse("2023-02-30"));
    }

    // -- seeded java.util.Random -----------------------------------------------
    // `Random`'s 48-bit LCG is SPECIFIED in its javadoc, down to the
    // constants, so a seeded sequence is an exact observable rather than a
    // sample — the only family here where a whole algorithm can be diffed in
    // one line. `Collections.shuffle` is specified in terms of it, so the
    // shuffled order is exact too. `SecureRandom` and `ThreadLocalRandom`
    // are deliberately absent: neither is reproducible, so neither can be
    // differentially tested this way.
    static void seededRandom() {
        Random r = new Random(42);
        line("Random.nextIntSequence", r.nextInt() + "," + r.nextInt() + "," + r.nextInt());
        line("Random.nextIntBoundSequence", boundedInts(new Random(42), 100));
        line("Random.nextIntPowerOfTwoBound", boundedInts(new Random(42), 64));
        line("Random.nextIntOriginBound", new Random(42).nextInt(10, 20));
        line("Random.nextLong", new Random(42).nextLong());
        line("Random.nextDouble", new Random(42).nextDouble());
        line("Random.nextFloat", new Random(42).nextFloat());
        line("Random.nextBooleanSequence", booleans(new Random(42)));
        line("Random.nextGaussian", new Random(42).nextGaussian());
        line("Random.nextBytes", randomBytes());
        line("Random.intsStream", new Random(42).ints(5, 0, 100).boxed().toList());
        line("Random.doublesStreamFirst", new Random(42).doubles(2).boxed().toList());
        line("Random.setSeedRestartsSequence", setSeedRestarts());
        line("Random.sameSeedSameSequence", new Random(7).nextInt() == new Random(7).nextInt());
        line("Random.differentSeedsCollide", new Random(7).nextInt() == new Random(8).nextInt());
        line("Random.nextIntZeroBound", thrownBy(() -> new Random(1).nextInt(0)));
        line("Random.nextIntNegativeBound", thrownBy(() -> new Random(1).nextInt(-5)));
        line("Collections.shuffleSeeded", shuffleSeeded());
        line("Collections.shuffleSeededTwiceIsStable",
                shuffleSeeded().equals(shuffleSeeded()));
    }

    static String boundedInts(Random r, int bound) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 6; i++) {
            sb.append(r.nextInt(bound)).append(',');
        }
        return sb.toString();
    }

    static String booleans(Random r) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 8; i++) {
            sb.append(r.nextBoolean() ? '1' : '0');
        }
        return sb.toString();
    }

    static String randomBytes() {
        byte[] b = new byte[8];
        new Random(42).nextBytes(b);
        return Arrays.toString(b);
    }

    static String setSeedRestarts() {
        Random r = new Random(1);
        int first = r.nextInt();
        r.nextInt();
        r.setSeed(1);
        return first + ":" + r.nextInt();
    }

    static String shuffleSeeded() {
        List<String> l = new ArrayList<>(List.of("a", "b", "c", "d", "e", "f"));
        Collections.shuffle(l, new Random(42));
        return l.toString();
    }

    // -- Throwable, and the exceptions the VM itself mints ----------------------
    // Everything above tests JDK bytecode. This section tests CratonVM's own
    // exception machinery: the message on a helpful NullPointerException is
    // GENERATED BY THE VM from the bytecode that failed, and so are the
    // division, array-store, and cast messages. Those are the ones a shadow
    // cannot get right by delegating. The suppressed-exception list is the
    // quiet one — a try-with-resources whose close also throws must carry the
    // close failure as SUPPRESSED, and an empty suppressed array loses the
    // failure entirely without anything looking wrong.
    static void throwableSurface() {
        Exception cause = new IllegalStateException("root cause");
        Exception e = new RuntimeException("wrapper", cause);
        line("Throwable.getMessage", e.getMessage());
        line("Throwable.getCauseMessage", e.getCause().getMessage());
        line("Throwable.toString", e.toString());
        line("Throwable.causeToString", e.getCause().toString());
        line("Throwable.getLocalizedMessage", e.getLocalizedMessage());
        line("Throwable.noArgMessageIsNull", new RuntimeException().getMessage());
        line("Throwable.noArgToString", new RuntimeException().toString());
        line("Throwable.causeOfNoCauseIsNull", new RuntimeException("x").getCause());
        line("Throwable.initCauseAfterCtorThrows", thrownBy(() -> e.initCause(new RuntimeException())));
        line("Throwable.initCauseOnce", initCauseOnce());
        shadowedThrowableFields();
        line("Throwable.initCauseTwiceThrows", initCauseTwice());
        line("Throwable.selfCauseThrows", thrownBy(() -> {
            RuntimeException x = new RuntimeException();
            x.initCause(x);
        }));
        line("Throwable.addSuppressedSelfThrows", thrownBy(() -> {
            RuntimeException x = new RuntimeException();
            x.addSuppressed(x);
        }));
        line("Throwable.suppressedFromTryWithResources", suppressedFromTryWithResources());
        line("Throwable.suppressedDefaultIsEmpty", new RuntimeException().getSuppressed().length);
        line("Throwable.suppressionDisabled", disabledSuppression());
        line("Throwable.stackTraceTopFrame", topFrame());
        line("Throwable.stackTraceNonEmpty", new RuntimeException().getStackTrace().length > 0);
        line("Throwable.setStackTraceIsHonoured", setStackTrace());
        line("Throwable.customSubclassMessage", thrownDetail(() -> {
            throw new IllegalArgumentException("custom", cause);
        }));

        // VM-minted messages. Compiled without `-g`, so a helpful NPE names
        // the failing expression and a synthetic local slot rather than a
        // source variable name — identical for both sides because both sides
        // run the same class file.
        line("VM.nullPointerHelpfulMessage", thrownDetail(() -> {
            String s = nullString();
            s.length();
        }));
        line("VM.nullFieldAccessMessage", thrownDetail(() -> {
            int[] a = nullIntArray();
            int unused = a[0];
        }));
        line("VM.nullArrayStoreMessage", thrownDetail(() -> {
            int[] a = nullIntArray();
            a[0] = 1;
        }));
        line("VM.divideByZero", thrownDetail(() -> {
            int z = zero();
            int unused = 1 / z;
        }));
        line("VM.modByZero", thrownDetail(() -> {
            int z = zero();
            int unused = 1 % z;
        }));
        line("VM.longDivideByZero", thrownDetail(() -> {
            long z = zero();
            long unused = 1L / z;
        }));
        line("VM.doubleDivideByZeroIsInfinity", 1.0 / zero());
        line("VM.classCast", thrownDetail(() -> {
            Object o = "s";
            Integer unused = (Integer) o;
        }));
        line("VM.arrayStore", thrownDetail(() -> {
            Object[] o = new String[1];
            o[0] = Integer.valueOf(1);
        }));
        line("VM.arrayIndexOutOfBounds", thrownDetail(() -> {
            int[] a = new int[2];
            int unused = a[5];
        }));
        line("VM.negativeArraySize", thrownDetail(() -> {
            int n = negativeOne();
            int[] unused = new int[n];
        }));
        line("VM.arrayLengthOfNull", thrownDetail(() -> {
            int[] a = nullIntArray();
            int unused = a.length;
        }));
        line("VM.checkcastToArray", thrownDetail(() -> {
            Object o = new int[1];
            String[] unused = (String[]) o;
        }));
        line("VM.integerOverflowWraps", Integer.MAX_VALUE + 1);
        line("VM.intMinValueNegated", -Integer.MIN_VALUE);
        line("VM.intDivideMinByMinusOne", Integer.MIN_VALUE / negativeOne());
    }

    static String nullString() {
        return null;
    }

    static int[] nullIntArray() {
        return null;
    }

    static int zero() {
        return 0;
    }

    static int negativeOne() {
        return -1;
    }

    /**
     * A `Throwable` subclass is allowed to DECLARE ITS OWN field named `cause`
     * or `detailMessage`, and real code does — H2 1.2's
     * `org.h2.jdbc.JdbcSQLException` has `private final Throwable cause`, which
     * it assigns immediately before calling `initCause(cause)`.
     *
     * Java resolves `getfield`/`putfield` against the class named in the
     * constant pool, so `Throwable`'s own bytecode always reaches `Throwable`'s
     * slot however many subclasses shadow the name. A VM that shadows those
     * methods with natives and addresses the field BY NAME on the RECEIVER
     * resolves the most-derived declaration instead — a different slot — and
     * the two halves of a read/write pair stop agreeing.
     *
     * That is what broke `org.h2.test.unit.TestUpgrade` on 2026-08-14: the
     * `cause = this` sentinel went to `Throwable`'s slot, `initCause` read the
     * subclass's, saw the value the constructor had just put there, and refused
     * the first and only call with
     * `IllegalStateException: Can't overwrite cause with a null`.
     *
     * Every line below is an observable HotSpot answers one way and a
     * name-keyed implementation answers another, INCLUDING the one that says
     * the subclass's own field is still its own — a fix that redirected the
     * write but not the read, or vice versa, fails here rather than silently
     * half-working.
     */
    static class ShadowsCause extends RuntimeException {
        // Same name as java.lang.Throwable.cause, and NOT the same field.
        private Throwable cause;

        ShadowsCause(String message, Throwable cause) {
            super(message);
            this.cause = cause;
        }

        ShadowsCause(String message) {
            super(message);
        }

        Throwable ownCauseField() {
            return cause;
        }
    }

    static class ShadowsCauseWithCtorCause extends RuntimeException {
        private Throwable cause;

        ShadowsCauseWithCtorCause(String message, Throwable ctorCause, Throwable own) {
            // Hands `ctorCause` to Throwable, then puts something else in the
            // shadowing field: the two slots now hold DIFFERENT objects, so
            // `getCause()` naming the wrong one is unambiguous.
            super(message, ctorCause);
            this.cause = own;
        }
    }

    static class ShadowsDetailMessage extends RuntimeException {
        private String detailMessage;

        ShadowsDetailMessage(String message, String own) {
            super(message);
            this.detailMessage = own;
        }

        String ownDetailMessageField() {
            return detailMessage;
        }
    }

    static void shadowedThrowableFields() {
        // A first initCause() on a receiver that shadows `cause` must SUCCEED —
        // the subclass field holding a value is not Throwable's field holding
        // one.
        ShadowsCause a = new ShadowsCause("m", new IllegalArgumentException("own"));
        line("Throwable.shadowedCauseInitCauseSucceeds",
                thrownBy(() -> a.initCause(new IllegalStateException("real"))));
        line("Throwable.shadowedCauseGetCauseAfterInit", String.valueOf(a.getCause()));
        // ... and must not have disturbed the subclass's own field.
        line("Throwable.shadowedCauseOwnFieldUntouched", String.valueOf(a.ownCauseField()));

        // initCause(null) is a legal FIRST call. This is the exact shape H2's
        // JdbcSQLException constructor uses.
        ShadowsCause b = new ShadowsCause("m", null);
        line("Throwable.shadowedCauseInitCauseNull", thrownBy(() -> b.initCause(null)));

        // A cause supplied through the constructor is what getCause() reports,
        // not the shadowing field.
        ShadowsCauseWithCtorCause c = new ShadowsCauseWithCtorCause(
                "m", new IllegalArgumentException("ctor"), new IllegalStateException("own"));
        line("Throwable.shadowedCauseCtorCauseWins", String.valueOf(c.getCause()));
        // ... and THAT receiver's cause really is set, so a later initCause
        // must be refused. Reading the wrong slot would answer this one right
        // by accident, which is why it sits beside the three above.
        line("Throwable.shadowedCauseSecondInitCauseThrows",
                thrownBy(() -> c.initCause(new IllegalStateException("late"))));

        // The same shadow, on the other field the natives address by name.
        ShadowsDetailMessage d = new ShadowsDetailMessage("real", "own");
        line("Throwable.shadowedDetailMessageGetMessage", String.valueOf(d.getMessage()));
        line("Throwable.shadowedDetailMessageOwnFieldUntouched",
                String.valueOf(d.ownDetailMessageField()));
    }

    static String initCauseOnce() {
        RuntimeException e = new RuntimeException("m");
        e.initCause(new IllegalArgumentException("c"));
        return e.getCause().toString();
    }

    static String initCauseTwice() {
        RuntimeException e = new RuntimeException("m");
        e.initCause(new IllegalArgumentException("c"));
        return thrownBy(() -> e.initCause(new IllegalArgumentException("c2"))) + ":" + e.getCause();
    }

    static String suppressedFromTryWithResources() {
        try {
            try (AutoCloseable ignored = () -> {
                throw new IllegalStateException("close-failed");
            }) {
                throw new RuntimeException("body-failed");
            }
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder(String.valueOf(t.getMessage()));
            Throwable[] sup = t.getSuppressed();
            sb.append('/').append(sup.length);
            for (Throwable s : sup) {
                sb.append('/').append(s.getClass().getSimpleName()).append(':').append(s.getMessage());
            }
            return sb.toString();
        }
    }

    static String disabledSuppression() {
        RuntimeException e = new RuntimeException("m", null, false, false) { };
        e.addSuppressed(new RuntimeException("s"));
        return e.getSuppressed().length + ":" + e.getStackTrace().length;
    }

    static String topFrame() {
        try {
            throw new RuntimeException("x");
        } catch (RuntimeException e) {
            StackTraceElement[] st = e.getStackTrace();
            if (st.length == 0) {
                return "empty-stack-trace";
            }
            return st[0].getClassName() + "." + st[0].getMethodName();
        }
    }

    static String setStackTrace() {
        RuntimeException e = new RuntimeException("x");
        e.setStackTrace(new StackTraceElement[] {
            new StackTraceElement("Fake", "method", "Fake.java", 7)
        });
        StackTraceElement[] st = e.getStackTrace();
        return st.length + ":" + (st.length > 0 ? st[0].toString() : "");
    }

    // -- helpers -------------------------------------------------------------

    /**
     * Run one section. A throw is one reported line, not a truncated run —
     * and, since W7-42, a section that ends without emitting everything it
     * declared says so by name.
     *
     * The two guarantees are independent and both are needed. The fence
     * catches a section that DIED; the ledger catches a section that
     * FINISHED while quietly emitting fewer lines than it owes, which is the
     * failure that actually happened and which the fence cannot see.
     */
    static void section(String name, Runnable body) {
        curSec = sectionIndex(name);
        if (curSec < 0) {
            // A section run without a manifest entry: every one of its
            // observables would be unaccounted for. Loud, and every emitted
            // key will also report itself undeclared.
            System.out.println("SECTION-UNDECLARED=" + name);
            curSeen = null;
        } else {
            SEC_RAN[curSec] = true;
            curSeen = new boolean[SEC_KEYS[curSec].length];
        }
        try {
            body.run();
        } catch (Throwable t) {
            // Deliberately NOT through `line(..)`: this is a marker, not one
            // of the declared observables, and routing it through the ledger
            // would report it as undeclared on exactly the runs that need
            // reading most.
            System.out.println("SECTION-DIED." + name + "=" + t.getClass().getName());
        } finally {
            endSection();
        }
    }

    /** Name every observable the section owed and did not emit. */
    static void endSection() {
        if (curSec >= 0) {
            String[] names = SEC_KEYS[curSec];
            for (int i = 0; i < names.length; i++) {
                if (!curSeen[i]) {
                    missingCount++;
                    System.out.println("MISSING-OBSERVABLE=" + names[i]);
                }
            }
        }
        curSec = -1;
        curSeen = null;
    }

    /**
     * The run's own accounting, printed last and ALWAYS — a healthy run's
     * totals diff clean, so they cost one identical line each and turn four
     * classes of instrument failure into a visible difference.
     *
     * `PROBE-MANIFEST-DIGEST` is the one that closes the hole this was
     * written for. `MISSING-OBSERVABLE` catches a statement deleted on one
     * side; the digest catches a statement AND its declaration deleted on one
     * side, which is otherwise invisible because nothing is left to notice.
     * Two sides that print different digests were not built from the same
     * probe, and no comparison between them means anything.
     */
    static void finish() {
        int declared = 0;
        for (int i = 0; i < secCount; i++) {
            declared += SEC_KEYS[i].length;
            if (!SEC_RAN[i]) {
                // A `section(..)` call deleted from `main` takes every one of
                // its observables with it and throws nothing. Before the
                // ledger, that was indistinguishable from a clean run.
                System.out.println("SECTION-NEVER-RAN=" + SEC_NAME[i]);
                String[] names = SEC_KEYS[i];
                for (int j = 0; j < names.length; j++) {
                    missingCount++;
                    System.out.println("MISSING-OBSERVABLE=" + names[j]);
                }
            }
        }
        System.out.println("PROBE-SECTIONS=" + secCount);
        System.out.println("PROBE-OBSERVABLES-DECLARED=" + declared);
        System.out.println("PROBE-OBSERVABLES-EMITTED=" + emittedCount);
        System.out.println("PROBE-MANIFEST-DIGEST=" + manifestDigest());
        System.out.println("PROBE-LEDGER=missing:" + missingCount
                + ",undeclared:" + undeclaredCount
                + ",duplicate:" + duplicateCount
                + ",multiline:" + multilineCount
                + ",unrenderable:" + unrenderableCount);
        System.out.println("PROBE-DONE");
        // A transcript whose tail was still sitting in a buffer at exit is
        // absence of exactly the kind this record is about.
        System.out.flush();
    }

    /**
     * A digest of the manifest — section names and observable names, in
     * declaration order.
     *
     * Plain `long` arithmetic over `charAt`, rendered to hex by hand. It
     * deliberately calls neither `MessageDigest`, `String.hashCode`,
     * `Long.toHexString` nor `String.format`: an instrument that leans on the
     * surface under test can manufacture its own agreement.
     */
    static String manifestDigest() {
        long h = 1125899906842597L;
        for (int i = 0; i < secCount; i++) {
            h = fold(h, SEC_NAME[i]);
            String[] names = SEC_KEYS[i];
            for (int j = 0; j < names.length; j++) {
                h = fold(h, names[j]);
            }
        }
        char[] hex = new char[16];
        for (int i = 15; i >= 0; i--) {
            int nib = (int) (h & 0xFL);
            hex[i] = (char) (nib < 10 ? '0' + nib : 'a' + (nib - 10));
            h >>>= 4;
        }
        return new String(hex);
    }

    static long fold(long h, String s) {
        for (int i = 0; i < s.length(); i++) {
            h = h * 31L + s.charAt(i);
        }
        return h * 31L + 0x1FL;
    }

    /** The thrown type's name, or `no-throw` — never a stack trace or a message. */
    static String thrownBy(Runnable r) {
        try {
            r.run();
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    /**
     * Type AND message. Used where the message is the observable rather than
     * decoration: a `checked*` collection's refusal names the offending type,
     * `Enum.valueOf` names the constant, and a helpful `NullPointerException`
     * is generated by the VM from the failing bytecode. `thrownBy` would
     * report all of those as a bare class name and diff clean.
     */
    static String thrownDetail(Runnable r) {
        try {
            r.run();
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    /** A supplier that may throw a CHECKED exception (`parse`, mostly). */
    interface Thrower {
        Object get() throws Exception;
    }

    /**
     * The VALUE if it comes back, the thrown type's name if it does not — so
     * a checked-exception API still prints its content rather than forcing a
     * try/catch that would report only a verdict.
     */
    static String valueOrThrow(Thrower t) {
        try {
            return String.valueOf(t.get());
        } catch (Throwable x) {
            return x.getClass().getName();
        }
    }

    /**
     * BOUNDED. A `hasNext` that never goes false hangs the whole probe, which
     * is the same failure as the unbounded CME loop: the transcript stops and
     * a stopped transcript reads like a short clean one. The cap only ever
     * changes the output of a VM that is already wrong.
     */
    static String joinIter(Iterator<?> it) {
        StringBuilder sb = new StringBuilder();
        int guard = 0;
        while (it.hasNext()) {
            if (++guard > 100) {
                sb.append("unbounded-iterator-after-100");
                break;
            }
            sb.append(it.next()).append(';');
        }
        return sb.toString();
    }

    /** BOUNDED for the same reason as {@link #joinIter}. */
    static String joinEnumeration(Enumeration<?> en) {
        StringBuilder sb = new StringBuilder();
        int guard = 0;
        while (en.hasMoreElements()) {
            if (++guard > 100) {
                sb.append("unbounded-enumeration-after-100");
                break;
            }
            sb.append(en.nextElement()).append(';');
        }
        return sb.toString();
    }

    /**
     * BOUNDED drain. `while (!q.isEmpty()) q.poll()` relies on `poll` actually
     * REMOVING to terminate — a queue whose poll reads the head without
     * unlinking it spins forever and takes the rest of the transcript with
     * it, exactly like the unbounded CME loop did.
     */
    static String drainBounded(Queue<?> q) {
        StringBuilder sb = new StringBuilder();
        int guard = 0;
        while (!q.isEmpty()) {
            if (++guard > 100) {
                sb.append("poll-did-not-drain-after-100");
                break;
            }
            sb.append(q.poll()).append(',');
        }
        return sb.toString();
    }
}
