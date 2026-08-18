import java.util.*;
/** Is map.values().iterator() a LIVE view or a snapshot? HotSpot's is live. */
public class ValuesIterLive {
    static int fails = 0;
    static void ck(String n, Object got, Object want) {
        boolean ok = got == null ? want == null : got.equals(want);
        if (!ok) fails++;
        System.out.println("CK " + n + " got=" + got + " want=" + want + (ok ? "" : "  MISMATCH"));
    }
    public static void main(String[] a) {
        // 1. structural modification during iteration must throw CME
        Map<String,String> m = new HashMap<>();
        m.put("a","1"); m.put("b","2");
        boolean cme = false;
        try {
            Iterator<String> it = m.values().iterator();
            it.next();
            m.put("c","3");
            it.next();
            it.next();
        } catch (ConcurrentModificationException e) { cme = true; }
        catch (NoSuchElementException e) { /* a snapshot exhausts instead */ }
        ck("values.iterThrowsCME", cme, Boolean.TRUE);
        // 2. Iterator.remove() must write through to the MAP
        Map<String,String> m2 = new HashMap<>();
        m2.put("a","1"); m2.put("b","2");
        Iterator<String> it2 = m2.values().iterator();
        it2.next(); it2.remove();
        ck("values.iterRemoveWritesThrough", m2.size(), 1);
        // 3. same for keySet, the sibling door
        Map<String,String> m3 = new HashMap<>();
        m3.put("a","1"); m3.put("b","2");
        Iterator<String> it3 = m3.keySet().iterator();
        it3.next(); it3.remove();
        ck("keySet.iterRemoveWritesThrough", m3.size(), 1);
        // 4. entrySet too
        Map<String,String> m4 = new HashMap<>();
        m4.put("a","1"); m4.put("b","2");
        Iterator<Map.Entry<String,String>> it4 = m4.entrySet().iterator();
        it4.next(); it4.remove();
        ck("entrySet.iterRemoveWritesThrough", m4.size(), 1);
        // 5. the same three on ConcurrentHashMap, whose iterators are weakly consistent
        Map<String,String> c = new java.util.concurrent.ConcurrentHashMap<>();
        c.put("a","1"); c.put("b","2");
        Iterator<String> ic = c.values().iterator();
        ic.next(); ic.remove();
        ck("chm.valuesIterRemoveWritesThrough", c.size(), 1);
        System.out.println("CK fails=" + fails);
        System.out.println((fails == 0 ? "PASS " : "FAIL ") + "ValuesIterLive");
    }
}
