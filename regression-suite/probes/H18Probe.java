import java.util.*;

/**
 * H18-3's own probe (docs/known-issues/jdk-only/
 * H18-3-the-price-of-the-randomaccess-row-and-the-tier-that-did-not-get-the-fix-20260821.md
 * §1), filed here — its own §1 says "Not filed under regression-suite/probes/
 * ... outside this lane's ownership. Printed here so the next lane can paste
 * it." This is that paste, plus a new Section E pricing
 * {@code Collections.copy(dest, src)} for H18-3 N3: does the fast path read
 * *src*'s {@code RandomAccess}-ness (not just dest's), which decides whether
 * the blast radius named by H0-2/H15-2 is one method or two.
 *
 * Not scheduled in run.sh's SUITE_SET (this directory's convention for a
 * reproduction/verification artifact, same as OpcodeVsReflectionProbe.java) —
 * compile with the oracle's javac, run on all three arms (Compatible,
 * --jdk-only, HotSpot), diff stdout only.
 */
public class H18Probe {
    static String io(Object o, Class<?> t) {
        boolean opcode;
        if (t == AbstractMap.class)             opcode = o instanceof AbstractMap;
        else if (t == AbstractCollection.class) opcode = o instanceof AbstractCollection;
        else if (t == AbstractList.class)       opcode = o instanceof AbstractList;
        else if (t == AbstractSet.class)        opcode = o instanceof AbstractSet;
        else if (t == RandomAccess.class)       opcode = o instanceof RandomAccess;
        else if (t == SortedSet.class)          opcode = o instanceof SortedSet;
        else if (t == NavigableSet.class)       opcode = o instanceof NavigableSet;
        else if (t == Set.class)                opcode = o instanceof Set;
        else if (t == Collection.class)         opcode = o instanceof Collection;
        else throw new IllegalArgumentException("add an arm for " + t);
        boolean refl = t.isInstance(o);
        return (opcode ? "T" : "F") + "/" + (refl ? "T" : "F") + (opcode != refl ? "*" : " ");
    }
    static void rowA(String tag, Object o) {
        System.out.printf("A %-30s AbsMap=%s AbsColl=%s RandAcc=%s  getClass=%s%n",
            tag, io(o, AbstractMap.class), io(o, AbstractCollection.class),
            io(o, RandomAccess.class), o.getClass().getName());
    }
    static String cast(Object o, int which) {
        try {
            switch (which) {
                case 0: { AbstractMap<?,?> m = (AbstractMap<?,?>) o;  return "cast-ok:" + (m != null); }
                case 1: { AbstractCollection<?> c = (AbstractCollection<?>) o; return "cast-ok:" + (c != null); }
                default:{ RandomAccess r = (RandomAccess) o;          return "cast-ok:" + (r != null); }
            }
        } catch (ClassCastException e) { return "CCE: " + e.getMessage(); }
    }

    // -- Section F: instanceof RandomAccess cold vs after the caller tiers up.
    // Same shape as RJitMapTierDiff: an invocation-counted driver loop crosses
    // the C1 threshold through the ordinary front door, `moved` publishes the
    // iteration (if any) where the answer first changed. H18-3 §3's own
    // verification agenda step 6 asked for exactly this and it did not exist
    // anywhere in the tree before this file.
    static boolean fIsRandomAccess(List<Integer> l) {
        return l instanceof RandomAccess;
    }
    static long fBinarySearch(List<Integer> l, int n, int reps) {
        long acc = 0;
        for (int r = 0; r < reps; r++) acc += Collections.binarySearch(l, (r * 7919) % (2 * n));
        return acc;
    }
    static void sectionF() {
        System.out.println("\n== F: instanceof RandomAccess and binarySearch, COLD then JIT-WARM ==");
        int n = 2000;
        List<Integer> plain = new ArrayList<>(n);
        for (int i = 0; i < n; i++) plain.add(i * 2);
        List<Integer> wrapped = Collections.unmodifiableList(plain);

        boolean cold = fIsRandomAccess(wrapped);
        int movedAt = -1;
        boolean hot = cold;
        int iters = 3000;
        for (int i = 0; i < iters; i++) {
            boolean a = fIsRandomAccess(wrapped);
            if (movedAt < 0 && a != cold) movedAt = i;
            hot = a;
        }
        System.out.println("F instanceof RandomAccess  cold=" + cold + " hot=" + hot + " moved=" + movedAt);

        // Warm bs() itself (a small hot method, same shape as H0-2's own
        // finding: the branch lives inside a "small, hot, obviously-compilable
        // method").
        long sink = 0;
        for (int w = 0; w < 3; w++) { sink += fBinarySearch(plain, n, 40); sink += fBinarySearch(wrapped, n, 40); }
        long tPlain = 0, tWrap = 0;
        for (int w = 0; w < 5000; w++) {
            long t0 = System.nanoTime(); sink += fBinarySearch(plain, n, 40);   tPlain += System.nanoTime() - t0;
            long t1 = System.nanoTime(); sink += fBinarySearch(wrapped, n, 40); tWrap  += System.nanoTime() - t1;
        }
        double ratio = (double) tWrap / (double) Math.max(1, tPlain);
        System.out.printf("F binarySearch JIT-WARM plain=%dus wrapped=%dus ratio=%.2fx (sink=%d)%n",
            tPlain / 1000, tWrap / 1000, ratio, sink);
    }

    // -- Section E: does Collections.copy's fast path read SRC's RandomAccess,
    // not only dest's? H18-3 N3. `List.of(...)` (an immutable, non-RandomAccess
    // stamp in Compatible mode) as src, a plain ArrayList as dest.
    static void sectionE() {
        System.out.println("\n== E: Collections.copy(dest, src) -- does it read src's RandomAccess? ==");
        int n = 60000, reps = 3000;
        List<Integer> srcPlain = new ArrayList<>(n);
        for (int i = 0; i < n; i++) srcPlain.add(i);
        List<Integer> srcWrapped = Collections.unmodifiableList(srcPlain);
        System.out.println("E srcWrapped instanceof RandomAccess = " + (srcWrapped instanceof RandomAccess));

        List<Integer> destA = new ArrayList<>(Collections.nCopies(n, 0));
        List<Integer> destB = new ArrayList<>(Collections.nCopies(n, 0));

        long sink = 0;
        for (int w = 0; w < 3; w++) {
            Collections.copy(destA, srcPlain);
            Collections.copy(destB, srcWrapped);
        }
        long tPlain = 0, tWrap = 0;
        for (int w = 0; w < reps; w++) {
            long t0 = System.nanoTime(); Collections.copy(destA, srcPlain);   tPlain += System.nanoTime() - t0;
            long t1 = System.nanoTime(); Collections.copy(destB, srcWrapped); tWrap  += System.nanoTime() - t1;
        }
        sink = destA.get(0) + destB.get(0);
        double ratio = (double) tWrap / (double) Math.max(1, tPlain);
        System.out.printf("E copy(dest, src)  plain-src=%dms wrapped-src=%dms ratio=%.1fx (sink=%d)%n",
            tPlain / 1_000_000, tWrap / 1_000_000, ratio, sink);
        System.out.println("E correctness: destA.equals(destB) = " + destA.equals(destB));
    }

    public static void main(String[] a) {
        System.out.println("== A: instanceof/isInstance ('*' = the two doors disagree) ==");
        rowA("Map.of()", Map.of());
        rowA("Map.of(k,v)", Map.of("k","v"));
        rowA("Map.copyOf", Map.copyOf(new HashMap<>(Map.of("k","v"))));
        rowA("List.of()", List.of());
        rowA("List.of(1,2,3)", List.of(1,2,3));
        rowA("Set.of(x)", Set.of("x"));
        rowA("Collections.unmodifiableList", Collections.unmodifiableList(new ArrayList<>(List.of(1))));

        System.out.println("\n== B: CHECKCAST -- the verdict, and what the VM's message calls the receiver ==");
        System.out.println("B Map.of(k,v)    -> AbstractMap        : " + cast(Map.of("k","v"), 0));
        System.out.println("B List.of(1,2,3) -> AbstractCollection : " + cast(List.of(1,2,3), 1));
        System.out.println("B List.of(1,2,3) -> RandomAccess       : " + cast(List.of(1,2,3), 2));
        List<Integer> ul = Collections.unmodifiableList(new ArrayList<>(List.of(1)));
        System.out.println("B unmodList      -> RandomAccess       : " + cast(ul, 2));
        System.out.println("B NEGCTRL unmodList -> AbstractCollection (CCE on BOTH) : " + cast(ul, 1));
        System.out.println("B NEGCTRL HashMap   -> AbstractMap     (ok  on BOTH) : " + cast(new HashMap<>(), 0));

        System.out.println("\n== C: the families H0-2 N3 says were never probed ==");
        SortedSet<String> ss = Collections.unmodifiableSortedSet(new TreeSet<>(Set.of("a","b")));
        NavigableSet<String> ns = Collections.unmodifiableNavigableSet(new TreeSet<>(Set.of("a","b")));
        Collection<String> uc = Collections.unmodifiableCollection(new ArrayList<>(List.of("a")));
        Set<String> ks = Collections.unmodifiableMap(new HashMap<>(Map.of("k","v"))).keySet();
        Set<?>     es = Collections.unmodifiableMap(new HashMap<>(Map.of("k","v"))).entrySet();
        System.out.printf("C sortedSet    AbsColl=%s AbsSet=%s Sorted=%s Nav=%s getClass=%s%n",
            io(ss,AbstractCollection.class), io(ss,AbstractSet.class), io(ss,SortedSet.class),
            io(ss,NavigableSet.class), ss.getClass().getName());
        System.out.printf("C navSet       AbsColl=%s AbsSet=%s Sorted=%s Nav=%s getClass=%s%n",
            io(ns,AbstractCollection.class), io(ns,AbstractSet.class), io(ns,SortedSet.class),
            io(ns,NavigableSet.class), ns.getClass().getName());
        System.out.printf("C collection   AbsColl=%s Coll=%s getClass=%s%n",
            io(uc,AbstractCollection.class), io(uc,Collection.class), uc.getClass().getName());
        System.out.printf("C keySet       AbsColl=%s AbsSet=%s Set=%s getClass=%s%n",
            io(ks,AbstractCollection.class), io(ks,AbstractSet.class), io(ks,Set.class), ks.getClass().getName());
        System.out.printf("C entrySet     AbsColl=%s AbsSet=%s Set=%s getClass=%s%n",
            io(es,AbstractCollection.class), io(es,AbstractSet.class), io(es,Set.class), es.getClass().getName());

        System.out.println("\n== D: the price of the RandomAccess row, A/B WITHIN one VM (cold) ==");
        int n = 60000, reps = 40;
        List<Integer> plain = new ArrayList<>(n);
        for (int i = 0; i < n; i++) plain.add(i * 2);
        List<Integer> wrapped = Collections.unmodifiableList(plain);
        System.out.println("D wrapped instanceof RandomAccess = " + (wrapped instanceof RandomAccess));
        long sink = 0;
        for (int w = 0; w < 3; w++) { sink += bs(plain, n, reps); sink += bs(wrapped, n, reps); }
        long tPlain = 0, tWrap = 0;
        for (int w = 0; w < 5; w++) {
            long t0 = System.nanoTime(); sink += bs(plain, n, reps);   tPlain += System.nanoTime() - t0;
            long t1 = System.nanoTime(); sink += bs(wrapped, n, reps); tWrap  += System.nanoTime() - t1;
        }
        System.out.printf("D binarySearch plain=%d ms  wrapped=%d ms  ratio=%.1fx  (sink=%d)%n",
            tPlain/1_000_000, tWrap/1_000_000, (double) tWrap / (double) tPlain, sink);

        sectionE();
        sectionF();
    }
    static long bs(List<Integer> l, int n, int reps) {
        long acc = 0;
        for (int r = 0; r < reps; r++) acc += Collections.binarySearch(l, (r * 7919) % (2 * n));
        return acc;
    }
}
