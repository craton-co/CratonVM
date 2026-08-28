import java.util.*;

/** L3 tail / the rest of `java.util.Collections` — 25 owning bridge rows, of
 *  which four (`emptyList`, `emptySet`, `emptyEnumeration`, `sort`) were probed
 *  on 2026-08-28 and the other twenty-one never have been. Plus the four
 *  `Collections$Empty*` singletons, whose whole surface is refusals.
 *
 *  This class is almost entirely static factories and in-place algorithms, so
 *  its contract edges are of three kinds and nothing else:
 *
 *    * WHAT IT REFUSES — `max`/`min` on an empty collection is
 *      `NoSuchElementException`, `nCopies(-1, x)` is
 *      `IllegalArgumentException`, `swap` past the end is
 *      `IndexOutOfBoundsException`, and every one of the `unmodifiable*` and
 *      `singleton*` views answers `UnsupportedOperationException` to every
 *      mutator — including the ones reached through an ITERATOR, which is the
 *      door a wrapper that only overrides the obvious methods leaves open;
 *    * WHETHER IT IS A VIEW OR A COPY — `unmodifiableList(l)` must SEE a later
 *      `l.add(..)`. A defensive copy passes every immutability test and fails
 *      this one;
 *    * WHETHER IT WROTE THROUGH — `reverse`, `fill`, `swap` and `sort` mutate
 *      their argument in place, so an implementation that rebuilds a list and
 *      forgets to store it reports success and changes nothing.
 *
 *  DETERMINISM: `shuffle` is the one non-deterministic member; only its SIZE
 *  and its sorted content are printed, never its order.
 */
public class CollectionsShadowSweep {
    static int rows = 0;
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    interface ThrowingRun { void run() throws Throwable; }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface Call { Object call() throws Throwable; }
    static void tv(String tag, Call r) {
        try { p(tag, "ok " + String.valueOf(r.call())); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    static List<String> l4() { return new ArrayList<>(Arrays.asList("b", "d", "a", "c")); }
    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }

    // ------------------------------------------------------------------
    // 1. the in-place algorithms
    // ------------------------------------------------------------------
    static void algorithms() {
        List<String> l = l4();
        Collections.sort(l);
        p("sort", l.toString());
        Collections.sort(l, Collections.reverseOrder());
        p("sort with comparator", l.toString());
        Collections.sort(l, null);
        p("sort(null comparator) is natural", l.toString());
        t("sort null list", () -> Collections.sort(null));
        t("sort null list with comparator", () -> Collections.sort(null, null));
        List<Object> nc = new ArrayList<>(Arrays.asList(new Object(), new Object()));
        t("sort non-Comparable", () -> Collections.sort((List) nc));

        List<String> r = l4();
        Collections.reverse(r);
        p("reverse", r.toString());
        t("reverse null", () -> Collections.reverse(null));
        Collections.reverse(new ArrayList<String>());
        p("reverse empty", "ok");

        List<String> f = l4();
        Collections.fill(f, "x");
        p("fill", f.toString());
        p("fill size unchanged", f.size());
        t("fill null list", () -> Collections.fill(null, "x"));
        Collections.fill(f, null);
        p("fill with null", f.toString());

        List<String> s = l4();
        Collections.swap(s, 0, 3);
        p("swap", s.toString());
        Collections.swap(s, 1, 1);
        p("swap same index", s.toString());
        t("swap out of range", () -> Collections.swap(s, 0, 9));
        t("swap negative", () -> Collections.swap(s, -1, 0));
        t("swap null", () -> Collections.swap(null, 0, 1));

        p("frequency", Collections.frequency(Arrays.asList("a", "b", "a"), "a"));
        p("frequency absent", Collections.frequency(Arrays.asList("a"), "z"));
        p("frequency null element", Collections.frequency(Arrays.asList("a", null), null));
        t("frequency null collection", () -> Collections.frequency(null, "a"));

        p("max", Collections.max(Arrays.asList("b", "d", "a")));
        p("min", Collections.min(Arrays.asList("b", "d", "a")));
        t("max empty", () -> Collections.max(new ArrayList<String>()));
        t("min empty", () -> Collections.min(new ArrayList<String>()));
        t("max null", () -> Collections.max(null));
        t("max non-Comparable", () -> Collections.max((Collection) nc));
        p("max singleton", Collections.max(Arrays.asList("a")));
        p("max with comparator", Collections.max(Arrays.asList("b", "d", "a"),
                                                 Collections.reverseOrder()));

        List<String> ac = new ArrayList<>();
        p("addAll returns true", Collections.addAll(ac, "a", "b"));
        p("addAll contents", ac.toString());
        p("addAll empty returns false", Collections.addAll(ac));
        t("addAll null collection", () -> Collections.addAll(null, "a"));
        t("addAll null array", () -> Collections.addAll(ac, (String[]) null));
        t("addAll to an immutable list", () -> Collections.addAll(Collections.emptyList(), "a"));

        p("nCopies", Collections.nCopies(3, "x").toString());
        p("nCopies zero", Collections.nCopies(0, "x").toString());
        t("nCopies negative", () -> Collections.nCopies(-1, "x"));
        t("nCopies is immutable", () -> Collections.nCopies(2, "x").add("y"));
        t("nCopies set", () -> Collections.nCopies(2, "x").set(0, "y"));
        p("nCopies get", Collections.nCopies(2, "x").get(1));
        t("nCopies get out of range", () -> Collections.nCopies(2, "x").get(2));
        p("nCopies with null element", Collections.nCopies(2, null).toString());

        List<String> sh = l4();
        Collections.shuffle(sh);
        p("shuffle size", sh.size());
        p("shuffle content", sorted(sh));
        t("shuffle null", () -> Collections.shuffle(null));
    }

    // ------------------------------------------------------------------
    // 2. the immutable singletons and factories
    // ------------------------------------------------------------------
    static void immutables() {
        List<String> el = Collections.emptyList();
        p("emptyList", el.toString());
        p("emptyList size", el.size());
        p("emptyList is a singleton", Collections.emptyList() == Collections.emptyList());
        t("emptyList add", () -> Collections.<String>emptyList().add("a"));
        t("emptyList remove", () -> Collections.emptyList().remove(0));
        t("emptyList get", () -> Collections.emptyList().get(0));
        p("emptyList equals a new ArrayList", el.equals(new ArrayList<String>()));
        p("emptyList hashCode", el.hashCode());

        Set<String> es = Collections.emptySet();
        p("emptySet", es.toString());
        t("emptySet add", () -> Collections.<String>emptySet().add("a"));
        p("emptySet equals a new HashSet", es.equals(new HashSet<String>()));

        Map<String, String> em = Collections.emptyMap();
        p("emptyMap", em.toString());
        p("emptyMap get", em.get("a"));
        t("emptyMap put", () -> Collections.<String, String>emptyMap().put("a", "b"));
        p("emptyMap equals a new HashMap", em.equals(new HashMap<String, String>()));

        Iterator<String> ei = Collections.emptyIterator();
        p("emptyIterator hasNext", ei.hasNext());
        t("emptyIterator next", () -> ei.next());
        t("emptyIterator remove", () -> ei.remove());
        ListIterator<String> eli = Collections.emptyListIterator();
        p("emptyListIterator hasNext", eli.hasNext());
        p("emptyListIterator hasPrevious", eli.hasPrevious());
        p("emptyListIterator nextIndex", eli.nextIndex());
        p("emptyListIterator previousIndex", eli.previousIndex());
        t("emptyListIterator next", () -> eli.next());
        t("emptyListIterator previous", () -> eli.previous());
        t("emptyListIterator add", () -> Collections.emptyListIterator().add("a"));
        t("emptyListIterator set", () -> Collections.<String>emptyListIterator().set("a"));
        Enumeration<String> ee = Collections.emptyEnumeration();
        p("emptyEnumeration hasMoreElements", ee.hasMoreElements());
        t("emptyEnumeration nextElement", () -> ee.nextElement());

        List<String> sl = Collections.singletonList("a");
        p("singletonList", sl.toString());
        p("singletonList size", sl.size());
        p("singletonList get", sl.get(0));
        t("singletonList get 1", () -> Collections.singletonList("a").get(1));
        t("singletonList add", () -> Collections.singletonList("a").add("b"));
        t("singletonList set", () -> Collections.singletonList("a").set(0, "b"));
        t("singletonList remove", () -> Collections.singletonList("a").remove(0));
        t("singletonList iterator remove", () -> {
            Iterator<String> i = Collections.singletonList("a").iterator();
            i.next(); i.remove();
        });
        p("singletonList with null", Collections.singletonList(null).toString());
        p("singletonList equals", sl.equals(Arrays.asList("a")));

        Set<String> ss = Collections.singleton("a");
        p("singleton", ss.toString());
        t("singleton add", () -> Collections.singleton("a").add("b"));
        t("singleton remove", () -> Collections.singleton("a").remove("a"));
        p("singleton contains", ss.contains("a"));
        p("singleton equals a HashSet", ss.equals(new HashSet<>(Arrays.asList("a"))));

        Map<String, String> sm = Collections.singletonMap("k", "v");
        p("singletonMap", sm.toString());
        p("singletonMap get", sm.get("k"));
        p("singletonMap size", sm.size());
        t("singletonMap put", () -> Collections.singletonMap("k", "v").put("a", "b"));
        t("singletonMap remove", () -> Collections.singletonMap("k", "v").remove("k"));
        p("singletonMap keySet", sm.keySet().toString());
        p("singletonMap equals", sm.equals(new HashMap<>(sm)));
    }

    // ------------------------------------------------------------------
    // 3. the wrappers — views, not copies
    // ------------------------------------------------------------------
    static void wrappers() {
        List<String> src = l4();
        List<String> ul = Collections.unmodifiableList(src);
        p("unmodifiableList content", ul.toString());
        t("unmodifiableList add", () -> ul.add("z"));
        t("unmodifiableList set", () -> ul.set(0, "z"));
        t("unmodifiableList remove", () -> ul.remove(0));
        t("unmodifiableList clear", () -> ul.clear());
        t("unmodifiableList sort", () -> ul.sort(null));
        t("unmodifiableList removeIf", () -> ul.removeIf(x -> true));
        t("unmodifiableList replaceAll", () -> ul.replaceAll(x -> x));
        t("unmodifiableList iterator remove", () -> {
            Iterator<String> i = ul.iterator(); i.next(); i.remove();
        });
        t("unmodifiableList listIterator set", () -> {
            ListIterator<String> i = ul.listIterator(); i.next(); i.set("z");
        });
        // A VIEW, not a copy: the source's later write must be visible.
        src.add("NEW");
        p("unmodifiableList is a view", ul.toString());
        p("unmodifiableList size after source add", ul.size());
        p("unmodifiableList get", ul.get(0));
        p("unmodifiableList equals the source", ul.equals(src));
        p("unmodifiableList subList", ul.subList(0, 2).toString());
        t("unmodifiableList subList set", () -> ul.subList(0, 2).set(0, "z"));
        t("unmodifiableList null", () -> Collections.unmodifiableList(null));

        Set<String> usrc = new LinkedHashSet<>(Arrays.asList("a", "b"));
        Set<String> us = Collections.unmodifiableSet(usrc);
        t("unmodifiableSet add", () -> us.add("z"));
        t("unmodifiableSet remove", () -> us.remove("a"));
        usrc.add("c");
        p("unmodifiableSet is a view", sorted(us));
        p("unmodifiableSet contains", us.contains("a"));

        Map<String, String> msrc = new LinkedHashMap<>();
        msrc.put("k", "v");
        Map<String, String> um = Collections.unmodifiableMap(msrc);
        t("unmodifiableMap put", () -> um.put("a", "b"));
        t("unmodifiableMap remove", () -> um.remove("k"));
        t("unmodifiableMap clear", () -> um.clear());
        t("unmodifiableMap putAll", () -> um.putAll(new HashMap<>()));
        t("unmodifiableMap computeIfAbsent", () -> um.computeIfAbsent("z", x -> "y"));
        t("unmodifiableMap merge", () -> um.merge("k", "y", (a, b) -> a));
        t("unmodifiableMap replaceAll", () -> um.replaceAll((a, b) -> b));
        t("unmodifiableMap entrySet setValue", () -> {
            for (Map.Entry<String, String> e : um.entrySet()) e.setValue("z");
        });
        t("unmodifiableMap keySet remove", () -> um.keySet().remove("k"));
        t("unmodifiableMap values remove", () -> um.values().remove("v"));
        msrc.put("k2", "v2");
        p("unmodifiableMap is a view", sorted(um.keySet()));
        p("unmodifiableMap get", um.get("k"));

        Collection<String> uc = Collections.unmodifiableCollection(new ArrayList<>(Arrays.asList("a")));
        t("unmodifiableCollection add", () -> uc.add("z"));
        p("unmodifiableCollection contains", uc.contains("a"));

        SortedSet<String> sss = new TreeSet<>(Arrays.asList("b", "a"));
        SortedSet<String> uss = Collections.unmodifiableSortedSet(sss);
        p("unmodifiableSortedSet order", uss.toString());
        p("unmodifiableSortedSet first", uss.first());
        t("unmodifiableSortedSet add", () -> uss.add("z"));
        p("unmodifiableSortedSet headSet", uss.headSet("b").toString());
        t("unmodifiableSortedSet headSet add", () -> uss.headSet("b").add("z"));

        NavigableSet<String> nss = new TreeSet<>(Arrays.asList("b", "a"));
        NavigableSet<String> uns = Collections.unmodifiableNavigableSet(nss);
        p("unmodifiableNavigableSet ceiling", uns.ceiling("a"));
        t("unmodifiableNavigableSet pollFirst", () -> uns.pollFirst());

        // The synchronized wrappers delegate; they are NOT immutable.
        List<String> syl = Collections.synchronizedList(new ArrayList<>(Arrays.asList("a")));
        p("synchronizedList add", syl.add("b"));
        p("synchronizedList content", syl.toString());
        p("synchronizedList get", syl.get(0));
        p("synchronizedList size", syl.size());
        p("synchronizedList remove", syl.remove("a"));
        p("synchronizedList after remove", syl.toString());
        p("synchronizedList equals a plain list", syl.equals(Arrays.asList("b")));
        t("synchronizedList null", () -> Collections.synchronizedList(null));
        Set<String> sys = Collections.synchronizedSet(new LinkedHashSet<>(Arrays.asList("a")));
        p("synchronizedSet add", sys.add("b"));
        p("synchronizedSet content", sorted(sys));
        Map<String, String> sym = Collections.synchronizedMap(new LinkedHashMap<>());
        p("synchronizedMap put", sym.put("k", "v"));
        p("synchronizedMap get", sym.get("k"));
        p("synchronizedMap size", sym.size());
        p("synchronizedMap keySet", sorted(sym.keySet()));
        Collection<String> syc = Collections.synchronizedCollection(
            new ArrayList<>(Arrays.asList("a")));
        p("synchronizedCollection add", syc.add("b"));
        p("synchronizedCollection size", syc.size());
        StringBuilder it = new StringBuilder();
        for (String x : syc) it.append(x);
        p("synchronizedCollection iterates", it.toString());

        Set<String> sfm = Collections.newSetFromMap(new LinkedHashMap<String, Boolean>());
        p("newSetFromMap add", sfm.add("a"));
        p("newSetFromMap add duplicate", sfm.add("a"));
        p("newSetFromMap contains", sfm.contains("a"));
        p("newSetFromMap size", sfm.size());
        p("newSetFromMap remove", sfm.remove("a"));
        Map<String, Boolean> nonEmpty = new HashMap<>();
        nonEmpty.put("x", Boolean.TRUE);
        t("newSetFromMap of a non-empty map", () -> Collections.newSetFromMap(nonEmpty));
        t("newSetFromMap null", () -> Collections.newSetFromMap(null));

        p("reverseOrder", Collections.reverseOrder().compare("a", "b"));
        p("reverseOrder of a comparator", Collections.reverseOrder(
            Collections.reverseOrder()).compare("a", "b"));
        p("disjoint true", Collections.disjoint(Arrays.asList("a"), Arrays.asList("b")));
        p("disjoint false", Collections.disjoint(Arrays.asList("a"), Arrays.asList("a")));
        p("binarySearch", Collections.binarySearch(Arrays.asList("a", "b", "c"), "b"));
        p("binarySearch absent", Collections.binarySearch(Arrays.asList("a", "c"), "b"));
        p("list from enumeration", Collections.list(
            new Vector<>(Arrays.asList("a", "b")).elements()).toString());
        p("enumeration from collection", Collections.enumeration(
            Arrays.asList("a")).nextElement());
        p("unmodifiableList of a singletonList",
            Collections.unmodifiableList(Collections.singletonList("a")).toString());
    }

    public static void main(String[] args) {
        algorithms();
        immutables();
        wrappers();
        System.out.println("ROWS " + rows);
        System.out.println("DONE CollectionsShadowSweep");
    }
}
