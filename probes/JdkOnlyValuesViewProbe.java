import java.util.ArrayList;
import java.util.Collection;
import java.util.HashMap;
import java.util.Iterator;
import java.util.List;
import java.util.Map;

/**
 * G60-1 N1 — is {@code java/util/ArrayList.get(I)} / {@code size()} retirable?
 *
 * <p>{@code native-api/src/retired_shadow.rs} retires five {@code ArrayList}
 * triples and holds these two back, and G60-1 §5 N1 says the table "records no
 * reason". The reason it does record is at
 * {@code vm/src/runtime/interpreter/native_override.rs:2448-2463}: a
 * {@code Map.values()} view is answered as a {@code java/util/ArrayList} with
 * its source map stashed in a trailing capacity slot, so {@code size}/{@code get}/
 * {@code iterator} ARE the implementation of the view, and retiring them freezes
 * every such view at its creation time. This probe is that claim as an
 * experiment rather than a reading.
 *
 * <p>Read it against {@code CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayList},
 * which yields on the same §1.4 predicate a retirement uses and therefore
 * simulates the retirement with no rebuild. The dial cannot go finer than a
 * class name, so it also yields the five triples that ARE retired — an upper
 * bound, exactly as P2-COLLECTIONS-SHADOWS-20260812.md §4 reads its arm A.
 *
 * <p>Every line is a fixed string plus a value, so the three arms diff directly.
 * A view line that disagrees with HotSpot is the freeze; an {@code al.*} line
 * that disagrees is a plain {@code ArrayList} defect and a different finding.
 */
public final class JdkOnlyValuesViewProbe {

    static int checks;
    static int fails;

    static void ck(String name, Object got, Object want) {
        checks++;
        boolean ok = got == null ? want == null : got.equals(want);
        if (!ok) {
            fails++;
        }
        System.out.println("CK " + name + " got=" + got + " want=" + want + (ok ? "" : "  MISMATCH"));
    }

    public static void main(String[] args) {
        whatCarriesAValuesView();
        plainArrayList();
        valuesViewTracksItsMap();
        keySetAndEntrySetForContrast();
        valuesViewIteration();
        valuesViewOfCopiedMap();
        capturedBeforeAnyEntry();
        System.out.println("CK checks=" + checks);
        System.out.println("CK fails=" + fails);
        System.out.println((fails == 0 ? "PASS " : "FAIL ") + "JdkOnlyValuesViewProbe");
    }

    /**
     * The question the hold reason turns on, asked directly: WHAT class carries
     * a values view here?
     *
     * <p>The recorded reason for holding {@code get}/{@code size} back is that
     * {@code Map.values()} is answered as a {@code java/util/ArrayList} with its
     * source map stashed in a trailing capacity slot, so those two methods are
     * the view's implementation. If strict mode returns the real
     * {@code HashMap$Values} instead, the entanglement is a Compatible-mode
     * property and says nothing about a strict-mode retirement — which is a
     * different verdict from "the hold is wrong", and only this line can tell
     * them apart.
     */
    static void whatCarriesAValuesView() {
        Map<String, String> hm = new HashMap<>();
        hm.put("a", "1");
        System.out.println("CK carrier.hashMapValues=" + hm.values().getClass().getName());
        System.out.println("CK carrier.hashMapKeySet=" + hm.keySet().getClass().getName());
        Map<String, String> chm = new java.util.concurrent.ConcurrentHashMap<>();
        chm.put("a", "1");
        System.out.println("CK carrier.concurrentHashMapValues="
                + chm.values().getClass().getName());
        Map<String, String> tm = new java.util.TreeMap<>();
        tm.put("a", "1");
        System.out.println("CK carrier.treeMapValues=" + tm.values().getClass().getName());
        // The remaining receivers `native_map_values` is registered on. Listing
        // them all is what makes "no family answers values() with an ArrayList"
        // a complete claim rather than a sample: HashMap, Map (the interface
        // door), Hashtable, Properties, EnumMap, IdentityHashMap and
        // WeakHashMap are every registration of that function in the tree.
        Map<String, String> lhm = new java.util.LinkedHashMap<>();
        lhm.put("a", "1");
        System.out.println("CK carrier.linkedHashMapValues=" + lhm.values().getClass().getName());
        java.util.Hashtable<String, String> ht = new java.util.Hashtable<>();
        ht.put("a", "1");
        System.out.println("CK carrier.hashtableValues=" + ht.values().getClass().getName());
        java.util.Properties pr = new java.util.Properties();
        pr.setProperty("a", "1");
        System.out.println("CK carrier.propertiesValues=" + pr.values().getClass().getName());
        Map<Thread.State, String> em = new java.util.EnumMap<>(Thread.State.class);
        em.put(Thread.State.NEW, "1");
        System.out.println("CK carrier.enumMapValues=" + em.values().getClass().getName());
        Map<String, String> ihm = new java.util.IdentityHashMap<>();
        ihm.put("a", "1");
        System.out.println("CK carrier.identityHashMapValues="
                + ihm.values().getClass().getName());
        Map<String, String> whm = new java.util.WeakHashMap<>();
        whm.put("a", "1");
        System.out.println("CK carrier.weakHashMapValues=" + whm.values().getClass().getName());
        // Through the interface door, which is a separate registration.
        Map<String, String> viaIface = new HashMap<>();
        viaIface.put("a", "1");
        System.out.println("CK carrier.viaMapInterface=" + valuesOf(viaIface).getClass().getName());
    }

    /** Calls {@code values()} through {@code java.util.Map}, not the class. */
    static Collection<String> valuesOf(Map<String, String> m) {
        return m.values();
    }

    /** The control arm: get/size on a list nobody views through. */
    static void plainArrayList() {
        ArrayList<String> al = new ArrayList<>();
        for (int i = 0; i < 5; i++) {
            al.add("e" + i);
        }
        ck("al.size", al.size(), 5);
        ck("al.get0", al.get(0), "e0");
        ck("al.get4", al.get(4), "e4");
        al.add("e5");
        ck("al.sizeAfterAdd", al.size(), 6);
        ck("al.get5", al.get(5), "e5");
        al.remove(0);
        ck("al.sizeAfterRemove", al.size(), 5);
        ck("al.get0AfterRemove", al.get(0), "e1");
        List<String> sub = new ArrayList<>(al);
        ck("al.copySize", sub.size(), 5);
        ck("al.copyGet0", sub.get(0), "e1");
    }

    /**
     * THE measurement. A {@code values()} view is live: every mutation of the
     * map must be visible through it, without re-fetching the view.
     */
    static void valuesViewTracksItsMap() {
        Map<String, String> m = new HashMap<>();
        m.put("a", "1");
        m.put("b", "2");
        Collection<String> v = m.values();
        ck("values.sizeInitial", v.size(), 2);
        ck("values.hasOne", v.contains("1"), Boolean.TRUE);

        m.put("c", "3");
        ck("values.sizeAfterPut", v.size(), 3);
        ck("values.seesNewValue", v.contains("3"), Boolean.TRUE);

        m.remove("a");
        ck("values.sizeAfterRemove", v.size(), 2);
        ck("values.loseRemovedValue", v.contains("1"), Boolean.FALSE);

        m.put("b", "22");
        ck("values.seesReplacedValue", v.contains("22"), Boolean.TRUE);
        ck("values.loseReplacedValue", v.contains("2"), Boolean.FALSE);

        m.clear();
        ck("values.sizeAfterClear", v.size(), 0);
        ck("values.emptyAfterClear", v.isEmpty(), Boolean.TRUE);
    }

    /**
     * Contrast, not decoration: {@code keySet()} and {@code entrySet()} are
     * views of the same map through a DIFFERENT carrier, so a freeze that shows
     * up only in {@code values()} localises the defect to the
     * {@code ArrayList}-shaped view rather than to map mutation generally.
     */
    static void keySetAndEntrySetForContrast() {
        Map<String, String> m = new HashMap<>();
        m.put("a", "1");
        int ks0 = m.keySet().size();
        int es0 = m.entrySet().size();
        java.util.Set<String> ks = m.keySet();
        java.util.Set<Map.Entry<String, String>> es = m.entrySet();
        m.put("b", "2");
        ck("keySet.sizeBefore", ks0, 1);
        ck("entrySet.sizeBefore", es0, 1);
        ck("keySet.sizeAfterPut", ks.size(), 2);
        ck("entrySet.sizeAfterPut", es.size(), 2);
    }

    /** Iterating a view taken BEFORE the mutation, then again after. */
    static void valuesViewIteration() {
        Map<String, String> m = new HashMap<>();
        m.put("a", "1");
        m.put("b", "2");
        Collection<String> v = m.values();
        int n0 = 0;
        for (Iterator<String> it = v.iterator(); it.hasNext(); ) {
            it.next();
            n0++;
        }
        ck("values.iterCountBefore", n0, 2);

        m.put("c", "3");
        int n1 = 0;
        int sum = 0;
        for (Iterator<String> it = v.iterator(); it.hasNext(); ) {
            sum += Integer.parseInt(it.next());
            n1++;
        }
        ck("values.iterCountAfter", n1, 3);
        ck("values.iterSumAfter", sum, 6);

        Object[] arr = v.toArray();
        ck("values.toArrayLen", arr.length, 3);
    }

    /**
     * A {@code values()} snapshot copied into a real {@code ArrayList} must NOT
     * track the map — the opposite property, and the one that fails if the view
     * and the copy share a carrier.
     */
    static void valuesViewOfCopiedMap() {
        Map<String, String> m = new HashMap<>();
        m.put("a", "1");
        m.put("b", "2");
        List<String> snapshot = new ArrayList<>(m.values());
        ck("snapshot.size", snapshot.size(), 2);
        m.put("c", "3");
        ck("snapshot.sizeIsFrozen", snapshot.size(), 2);
        ck("snapshot.doesNotSeeNewValue", snapshot.contains("3"), Boolean.FALSE);
        ck("values.stillLive", m.values().size(), 3);
    }

    /**
     * H2 {@code TestAlter.testAlterTableDropIdentityColumn}'s exact shape, which
     * is the one measured failure the hold reason cites:
     * {@code Schema.getAllSequences()} captures {@code ConcurrentHashMap.values()}
     * ONCE, before any sequence exists, and reads it later.
     *
     * <p>Written so that {@code size()} is the FIRST view method called after the
     * mutation. Every other section of this probe touches {@code contains} or
     * {@code iterator} first, and if those re-sync a stashed view then a later
     * {@code size()} reads an already-correct field and cannot distinguish a
     * retired native from a live one. This section can.
     */
    static void capturedBeforeAnyEntry() {
        Map<String, String> chm = new java.util.concurrent.ConcurrentHashMap<>();
        Collection<String> cv = chm.values();
        ck("captured.chmEmptyAtCapture", cv.size(), 0);
        chm.put("s1", "one");
        ck("captured.chmSizeIsFirstRead", cv.size(), 1);
        chm.put("s2", "two");
        ck("captured.chmSizeAgain", cv.size(), 2);
        ck("captured.chmGetThroughIterator", cv.iterator().hasNext(), Boolean.TRUE);

        Map<String, String> hm = new HashMap<>();
        Collection<String> hv = hm.values();
        ck("captured.hmEmptyAtCapture", hv.size(), 0);
        hm.put("k", "v");
        ck("captured.hmSizeIsFirstRead", hv.size(), 1);
        ck("captured.hmIsEmptyAfterPut", hv.isEmpty(), Boolean.FALSE);
        hm.remove("k");
        ck("captured.hmSizeAfterRemove", hv.size(), 0);

        // A values() view read through get(int) — only reachable when the view
        // really is a List, so it is a `List` cast guarded by instanceof rather
        // than an assumption about the carrier.
        Map<String, String> lm = new HashMap<>();
        lm.put("a", "1");
        Collection<String> lv = lm.values();
        boolean isList = lv instanceof List;
        ck("captured.valuesIsAList", isList, Boolean.FALSE);
        if (isList) {
            lm.put("b", "2");
            ck("captured.listGet1", ((List<String>) lv).get(1) != null, Boolean.TRUE);
        }
    }
}
