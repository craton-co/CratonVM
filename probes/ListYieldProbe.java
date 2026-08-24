import java.util.*;

/**
 * Behavioural parity for `java.util.ArrayList` under the real-JDK-bytecode
 * yield, i.e. the change Term 2 of the ConfigurationPropertySources page names
 * as its next step (`ArrayList.get`'s native costs 6.3x its own bytecode).
 *
 * The interesting rows are not `get` on a plain list -- those are trivially
 * right either way. They are the receivers that are ArrayList-SHAPED but are
 * NOT plain ArrayLists, because `native_al_get` routes those specially
 * (`vc_route`, `unmod_receiver_backing`, subList) and real JDK bytecode would
 * read `elementData`/`size` straight through:
 *
 *   * a live `values()` view, which must still see later mutations;
 *   * `Collections.unmodifiableList`, which must still refuse writes;
 *   * `subList`, whose indices are offset from the backing;
 *   * `Arrays.asList`, a fixed-size list that is not an ArrayList at all.
 *
 * Plus the bounds and null behaviour, where a native and a real body most
 * easily disagree on exception type and message.
 */
public class ListYieldProbe {
    static void p(String k, Object v) { System.out.println("LYP " + k + "=" + v); }
    static void ex(String k, Runnable r) {
        try { r.run(); p(k, "no-throw"); }
        catch (Throwable t) { p(k, t.getClass().getName() + "|" + t.getMessage()); }
    }

    public static void main(String[] args) {
        List<String> l = new ArrayList<>(List.of("a","b","c","d"));
        p("get0", l.get(0)); p("get3", l.get(3)); p("size", l.size());
        ex("getNeg",  () -> l.get(-1));
        ex("getHigh", () -> l.get(4));
        p("indexOf", l.indexOf("c"));
        p("contains", l.contains("b"));
        p("toString", l.toString());
        p("hash", l.hashCode() == List.of("a","b","c","d").hashCode());
        p("equalsList", l.equals(List.of("a","b","c","d")));

        // grow / remove / set
        l.add("e"); p("afterAdd", l.toString());
        l.set(0, "A"); p("afterSet", l.toString());
        l.remove("b"); p("afterRemove", l.toString());
        l.add(1, "Z"); p("afterInsert", l.toString());
        p("removeIdx", l.remove(2)); p("afterRemoveIdx", l.toString());

        // subList: indices are OFFSET from the backing
        List<String> sub = l.subList(1, 3);
        p("sub.size", sub.size()); p("sub.get0", sub.get(0)); p("sub.toString", sub.toString());
        sub.set(0, "S"); p("sub.writeThrough", l.toString());
        ex("sub.getHigh", () -> sub.get(5));

        // unmodifiable must still refuse writes
        List<String> un = Collections.unmodifiableList(l);
        p("un.get0", un.get(0)); p("un.size", un.size());
        ex("un.add", () -> un.add("x"));
        ex("un.set", () -> un.set(0, "x"));

        // Arrays.asList: fixed size, set allowed, add refused
        List<String> fixed = Arrays.asList("p","q","r");
        p("fx.get1", fixed.get(1)); p("fx.size", fixed.size());
        fixed.set(1, "Q"); p("fx.afterSet", fixed.toString());
        ex("fx.add", () -> fixed.add("s"));

        // a LIVE values() view is ArrayList-SHAPED — the stale-read hazard
        Map<String,String> m = new LinkedHashMap<>();
        m.put("k1","v1"); m.put("k2","v2");
        Collection<String> vals = m.values();
        p("vals.size0", vals.size()); p("vals.str0", vals.toString());
        m.put("k3","v3");
        p("vals.sizeAfterPut", vals.size());
        p("vals.strAfterPut", vals.toString());
        m.remove("k1");
        p("vals.sizeAfterRm", vals.size());
        p("vals.strAfterRm", vals.toString());
        p("vals.copyAfter", new ArrayList<>(vals).toString());
        p("vals.toArrLen", vals.toArray().length);

        // iterator + fail-fast
        List<String> fl = new ArrayList<>(List.of("m","n","o"));
        StringBuilder sb = new StringBuilder();
        for (String s : fl) { sb.append(s); }
        p("iterOrder", sb.toString());
        ex("failFast", () -> { for (String s : fl) { fl.add("boom"); } });

        // empty + null element
        List<String> e = new ArrayList<>();
        p("empty.size", e.size()); p("empty.isEmpty", e.isEmpty());
        ex("empty.get0", () -> e.get(0));
        e.add(null); p("nullElem", e.get(0)); p("nullContains", e.contains(null));
    }
}
