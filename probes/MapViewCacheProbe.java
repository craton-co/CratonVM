import java.util.*;

/**
 * The behavioural gate for the map-view cache
 * (`native-collections`'s "Live map views" note, and
 * `known-issues/perf/lazy-map-views-plan-and-blockers-20260822.md`).
 *
 * Every row is a property the cache could plausibly break, printed as
 * `name=value` so the CratonVM output can be diffed against HotSpot's
 * verbatim. Run BOTH and diff; identical output is the pass.
 *
 * The rows split into three groups:
 *
 *   ident.*   — `map.keySet() == map.keySet()`. HotSpot caches the view on the
 *               map, so this is `true` there; it used to be `false` here.
 *   live.*    — a view obtained BEFORE a mutation must observe it. This is
 *               what a generation guard gets wrong if a mutator fails to move
 *               the generation, and every mutator shape is exercised: put of a
 *               new key, put of an existing key (value replace), remove,
 *               clear, and a remove+put pair that leaves size unchanged.
 *   through.* — a mutation made THROUGH the view must reach the source, and
 *               the view must then agree with itself. This is the direction
 *               that edits the backing behind the source generation's back.
 *
 * `evalue.*` is the row that says why an entrySet view is NOT
 * generation-guarded: a value-replacing `put` changes what `getValue()` must
 * answer while moving no structural counter.
 *
 * THREE ROWS ARE EXPECTED TO DIFFER FROM HOTSPOT, and they are a scope
 * boundary rather than a defect:
 *
 *     ht.ident.keySet     ht.ident.entrySet     ht.ident.values
 *
 * The view cache refuses the `Hashtable`/`Properties` family outright
 * (`cached_live_view`'s `wants_synchronized_views` gate). Those accessors hand
 * back a `Collections$Synchronized*` wrapper, and `Properties` in particular
 * keeps half its keys in a Rust side-table that only its own `keySet()`
 * assembles correctly, so a cached instance there would pin whatever the
 * field-walking path produced. Every OTHER row on `ht` — liveness,
 * write-through, the copy constructors — must still match HotSpot exactly, and
 * does; only the identity guarantee is given up.
 *
 * Everything else is a hard gate: diff against HotSpot and expect those three
 * rows and nothing else.
 */
public class MapViewCacheProbe {

    static void row(String name, Object v) {
        System.out.println("MVC " + name + "=" + v);
    }

    static int addAllSize(Collection<?> c) {
        List<Object> t = new ArrayList<>();
        t.addAll(c);
        return t.size();
    }

    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) { l.add(String.valueOf(o)); }
        Collections.sort(l);
        return l.toString();
    }

    static void suite(String tag, Map<String, String> m) {
        m.clear();
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");

        // --- identity -------------------------------------------------------
        row(tag + ".ident.keySet", m.keySet() == m.keySet());
        row(tag + ".ident.entrySet", m.entrySet() == m.entrySet());
        row(tag + ".ident.values", m.values() == m.values());

        // --- liveness of a hoisted keySet view ------------------------------
        Set<String> ks = m.keySet();
        row(tag + ".live.initial", sorted(ks));
        row(tag + ".live.initial.size", ks.size());

        m.put("d", "4");                       // structural: new key
        row(tag + ".live.afterPut", sorted(ks));
        row(tag + ".live.afterPut.size", ks.size());
        row(tag + ".live.afterPut.contains", ks.contains("d"));

        m.put("d", "44");                      // NOT structural: value replace
        row(tag + ".live.afterReplace", sorted(ks));
        row(tag + ".live.afterReplace.size", ks.size());

        m.remove("a");                         // structural: removal
        row(tag + ".live.afterRemove", sorted(ks));
        row(tag + ".live.afterRemove.size", ks.size());
        row(tag + ".live.afterRemove.contains", ks.contains("a"));

        // The size-invariant pair the plan page calls out: a guard keyed on
        // size alone cannot see this.
        m.remove("b");
        m.put("z", "26");
        row(tag + ".live.afterSwap", sorted(ks));
        row(tag + ".live.afterSwap.size", ks.size());
        row(tag + ".live.afterSwap.hasB", ks.contains("b"));
        row(tag + ".live.afterSwap.hasZ", ks.contains("z"));

        // toArray / stream / iterator all go through the same resync door.
        row(tag + ".live.toArray", sorted(Arrays.asList(ks.toArray())));
        row(tag + ".live.streamCount", ks.stream().count());
        int walked = 0;
        for (String s : ks) { if (s != null) { walked++; } }
        row(tag + ".live.iterated", walked);
        row(tag + ".live.isEmpty", ks.isEmpty());

        m.clear();
        row(tag + ".live.afterClear", sorted(ks));
        row(tag + ".live.afterClear.size", ks.size());
        row(tag + ".live.afterClear.isEmpty", ks.isEmpty());

        // --- write-through --------------------------------------------------
        m.put("p", "1");
        m.put("q", "2");
        m.put("r", "3");
        Set<String> ks2 = m.keySet();
        ks2.remove("q");
        row(tag + ".through.remove.map", sorted(m.keySet()));
        row(tag + ".through.remove.view", sorted(ks2));
        row(tag + ".through.remove.mapSize", m.size());
        row(tag + ".through.remove.viewSize", ks2.size());
        row(tag + ".through.remove.mapHasQ", m.containsKey("q"));

        Iterator<String> it = ks2.iterator();
        while (it.hasNext()) {
            if (it.next().equals("p")) { it.remove(); }
        }
        row(tag + ".through.itrRemove.map", sorted(m.keySet()));
        row(tag + ".through.itrRemove.view", sorted(ks2));
        row(tag + ".through.itrRemove.mapSize", m.size());

        m.put("s", "4");
        row(tag + ".through.thenPut.view", sorted(ks2));
        row(tag + ".through.thenPut.viewSize", ks2.size());

        // --- entrySet value freshness ---------------------------------------
        m.clear();
        m.put("k", "v1");
        Set<Map.Entry<String, String>> es = m.entrySet();
        StringBuilder before = new StringBuilder();
        for (Map.Entry<String, String> e : es) { before.append(e.getKey()).append('=').append(e.getValue()); }
        row(tag + ".evalue.before", before.toString());
        m.put("k", "v2");                      // value replace, no structural change
        StringBuilder after = new StringBuilder();
        for (Map.Entry<String, String> e : es) { after.append(e.getKey()).append('=').append(e.getValue()); }
        row(tag + ".evalue.after", after.toString());

        // setValue through a live entry must reach the map.
        for (Map.Entry<String, String> e : es) { e.setValue("v3"); }
        row(tag + ".evalue.afterSetValue", m.get("k"));

        // --- values liveness ------------------------------------------------
        m.clear();
        m.put("a", "1");
        m.put("b", "2");
        Collection<String> vs = m.values();
        row(tag + ".values.initial", sorted(vs));
        m.put("c", "3");
        row(tag + ".values.afterPut", sorted(vs));
        m.put("a", "11");                      // value replace — no modCount bump
        row(tag + ".values.afterReplace", sorted(vs));
        row(tag + ".values.size", vs.size());
        m.remove("b");
        row(tag + ".values.afterRemove", sorted(vs));
        row(tag + ".values.contains", vs.contains("3"));
        // The COPY-CONSTRUCTOR direction, which does not go through the same
        // door as `size()`/`toString()`: `new ArrayList<>(c)` reaches
        // `collect_collection_elements`, and for a synchronized wrapper
        // (Hashtable) that unwraps to the ArrayList-SHAPED values carrier and
        // reads its element array directly. That read had no resync, so a
        // CACHED values view answered from a stale array while `size()` on the
        // same object was right -- see `ht.copyList.size` in
        // MapViewBehaviourProbe.
        row(tag + ".values.copyList", new ArrayList<>(vs).size());
        row(tag + ".values.copySet", new HashSet<>(vs).size());
        row(tag + ".values.toArrayLen", vs.toArray().length);
        row(tag + ".values.addAllTarget", addAllSize(vs));

        // --- keySet of an EMPTY map, then filled ----------------------------
        Map<String, String> fresh = m instanceof LinkedHashMap
                ? new LinkedHashMap<>() : new HashMap<>();
        Set<String> emptyView = fresh.keySet();
        row(tag + ".empty.size0", emptyView.size());
        fresh.put("x", "1");
        row(tag + ".empty.size1", emptyView.size());
        row(tag + ".empty.contains", emptyView.contains("x"));
    }

    public static void main(String[] args) {
        suite("hm", new HashMap<>());
        suite("lhm", new LinkedHashMap<>());
        suite("ht", new Hashtable<>());
        System.out.flush();
    }
}
