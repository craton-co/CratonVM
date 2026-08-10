import java.util.*;

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

    static void line(String k, Object v) {
        System.out.println(k + "=" + v);
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

        // The sections below were added 2026-08-10. Everything above is
        // `java.util`'s factories and views, and the honest reading of "they
        // match" was "the ones anybody looked at match" — the census counts
        // ~1,600 inherited shadows and this probe reached a few dozen of them.
        // Each section below is a family the census names and nothing had
        // exercised against the bytecode it shadows.
        // Each section is FENCED. A section that throws must not take the rest
        // of the transcript with it: a differential that stops early cannot
        // report a difference on any line after the one that killed it, and a
        // truncated transcript reads exactly like a short clean run — the
        // specific way the first two strict census runs lied. The fence prints
        // a marker instead, so the divergence is one line rather than a
        // hundred missing ones.
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

        System.out.println("PROBE-DONE");
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

        PriorityQueue<Integer> pq = new PriorityQueue<>(List.of(4, 1, 7, 3));
        StringBuilder drained = new StringBuilder();
        while (!pq.isEmpty()) {
            drained.append(pq.poll()).append(',');
        }
        line("PriorityQueue.drainOrder", drained);
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

    // -- helpers -------------------------------------------------------------

    /** Run one section; a throw is one reported line, not a truncated run. */
    static void section(String name, Runnable body) {
        try {
            body.run();
        } catch (Throwable t) {
            line("SECTION-DIED." + name, t.getClass().getName());
        }
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

    static String joinIter(Iterator<?> it) {
        StringBuilder sb = new StringBuilder();
        while (it.hasNext()) {
            sb.append(it.next()).append(';');
        }
        return sb.toString();
    }

    static String joinEnumeration(Enumeration<?> en) {
        StringBuilder sb = new StringBuilder();
        while (en.hasMoreElements()) {
            sb.append(en.nextElement()).append(';');
        }
        return sb.toString();
    }
}
