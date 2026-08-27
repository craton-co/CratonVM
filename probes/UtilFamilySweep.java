import java.util.*;
import java.math.BigInteger;

/** Several retirement-surface families at once, diffed against HotSpot.
 *
 *  One binary, no rebuild: each family is a deterministic sequence whose VALUES
 *  are printed, so two VMs can be diffed on stdout. Chosen from the bridge-kind
 *  retirement surface measured 2026-08-26 --- Properties (37 rows), BigInteger
 *  (39), ArrayDeque (33), LinkedList (31), TreeMap (41) --- deliberately
 *  EXCLUDING the StringBuilder/StringBuffer cluster, which has an open
 *  known-issue record with a diagnosed mechanism.
 *
 *  Every value is escaped so the diff cannot depend on either VM's stdout
 *  encoding: that artefact produced 16 phantom differences in FilePathSweep. */
public class UtilFamilySweep {
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
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Exception; }

    static void properties() {
        Properties d = new Properties();
        d.setProperty("inherited", "from-default");
        d.setProperty("shadowed", "default-value");
        Properties q = new Properties(d);
        q.setProperty("own", "own-value");
        q.setProperty("shadowed", "own-wins");
        p("prop getProperty own", q.getProperty("own"));
        p("prop getProperty inherited", q.getProperty("inherited"));
        p("prop getProperty shadowed", q.getProperty("shadowed"));
        p("prop getProperty absent", q.getProperty("absent"));
        p("prop getProperty absent+dflt", q.getProperty("absent", "fallback"));
        // the JDK distinguishes these three deliberately
        p("prop size (own only)", q.size());
        p("prop containsKey inherited", q.containsKey("inherited"));
        p("prop get inherited", q.get("inherited"));
        p("prop keySet size", q.keySet().size());
        p("prop stringPropertyNames size", q.stringPropertyNames().size());
        p("prop stringPropertyNames sorted", new TreeSet<>(q.stringPropertyNames()));
        p("prop propertyNames sorted", new TreeSet<>(Collections.list(q.propertyNames())));
        // a non-String value is visible to get() but NOT to getProperty()
        q.put("nonstring", Integer.valueOf(7));
        p("prop get nonstring", q.get("nonstring"));
        p("prop getProperty nonstring", q.getProperty("nonstring"));
        p("prop stringPropertyNames excludes nonstring",
          !q.stringPropertyNames().contains("nonstring"));
        p("prop isEmpty", q.isEmpty());
        p("prop remove own", q.remove("own"));
        p("prop after remove", q.getProperty("own"));
        p("prop toString-ish sorted", new TreeMap<>(new HashMap<>(q)).toString());
    }

    static void bigInteger() {
        BigInteger a = new BigInteger("123456789012345678901234567890");
        BigInteger b = BigInteger.valueOf(-987654321L);
        p("bi add", a.add(b));
        p("bi subtract", a.subtract(b));
        p("bi multiply", a.multiply(b));
        p("bi divide", a.divide(b));
        p("bi remainder", a.remainder(b));
        p("bi mod", a.mod(BigInteger.valueOf(97)));
        p("bi gcd", a.gcd(b.abs()));
        p("bi pow3", b.pow(3));
        p("bi negate", a.negate());
        p("bi abs", b.abs());
        p("bi signum", a.signum() + "," + b.signum());
        p("bi compareTo", a.compareTo(b));
        p("bi bitLength", a.bitLength());
        p("bi bitCount", a.bitCount());
        p("bi shiftLeft7", a.shiftLeft(7));
        p("bi shiftRight7", a.shiftRight(7));
        p("bi and", a.and(BigInteger.valueOf(0xFFFF)));
        p("bi or", a.or(BigInteger.valueOf(0xFFFF)));
        p("bi xor", a.xor(BigInteger.valueOf(0xFFFF)));
        p("bi not", a.not());
        p("bi testBit3", a.testBit(3));
        p("bi setBit99", a.setBit(99));
        p("bi toString(16)", a.toString(16));
        p("bi toString(36)", a.toString(36));
        p("bi intValue", a.intValue());
        p("bi longValue", a.longValue());
        p("bi doubleValue", Double.doubleToRawLongBits(a.doubleValue()));
        p("bi modPow", a.modPow(BigInteger.valueOf(65537), BigInteger.valueOf(1000000007L)));
        p("bi modInverse", BigInteger.valueOf(17).modInverse(BigInteger.valueOf(1000000007L)));
        p("bi isProbablePrime", BigInteger.valueOf(1000000007L).isProbablePrime(30));
        p("bi min/max", a.min(b) + "/" + a.max(b));
        t("bi divide by zero", () -> a.divide(BigInteger.ZERO));
        t("bi modInverse non-coprime", () -> BigInteger.valueOf(4).modInverse(BigInteger.valueOf(8)));
        t("bi negative pow", () -> a.pow(-1));
    }

    static void deque() {
        ArrayDeque<String> d = new ArrayDeque<>();
        for (String s : new String[]{"a", "b", "c"}) d.addLast(s);
        d.addFirst("z");
        p("deque toString", d);
        p("deque peekFirst/Last", d.peekFirst() + "/" + d.peekLast());
        p("deque pollFirst", d.pollFirst());
        p("deque pollLast", d.pollLast());
        p("deque size", d.size());
        p("deque contains b", d.contains("b"));
        p("deque toArray", Arrays.toString(d.toArray()));
        d.addAll(Arrays.asList("x", "y"));
        p("deque after addAll", d);
        Iterator<String> it = d.descendingIterator();
        StringBuilder sb = new StringBuilder();
        while (it.hasNext()) sb.append(it.next()).append(',');
        p("deque descending", sb);
        p("deque removeFirstOccurrence", d.removeFirstOccurrence("x"));
        p("deque after remove", d);
        t("deque removeFirst on empty", () -> new ArrayDeque<String>().removeFirst());
        t("deque addNull", () -> new ArrayDeque<String>().add(null));
    }

    static void linkedList() {
        LinkedList<String> l = new LinkedList<>(Arrays.asList("a", "b", "c"));
        l.addFirst("z"); l.addLast("w");
        p("ll toString", l);
        p("ll get2", l.get(2));
        p("ll indexOf c", l.indexOf("c"));
        p("ll removeFirst", l.removeFirst());
        p("ll removeLast", l.removeLast());
        p("ll subList", l.subList(0, 2));
        l.add(1, "ins");
        p("ll after add(idx)", l);
        p("ll set", l.set(0, "S") + " -> " + l);
        p("ll listIterator back", backwards(l));
        p("ll toArray", Arrays.toString(l.toArray()));
        p("ll equals ArrayList", l.equals(new ArrayList<>(l)));
        p("ll hashCode == ArrayList", l.hashCode() == new ArrayList<>(l).hashCode());
        t("ll get oob", () -> l.get(99));
        t("ll removeFirst empty", () -> new LinkedList<String>().removeFirst());
    }
    static String backwards(LinkedList<String> l) {
        ListIterator<String> it = l.listIterator(l.size());
        StringBuilder sb = new StringBuilder();
        while (it.hasPrevious()) sb.append(it.previous()).append(',');
        return sb.toString();
    }

    static void treeMap() {
        TreeMap<String, Integer> m = new TreeMap<>();
        for (String k : new String[]{"d", "b", "f", "a", "c"}) m.put(k, k.charAt(0) - 'a');
        p("tm toString", m);
        p("tm firstKey/lastKey", m.firstKey() + "/" + m.lastKey());
        p("tm headMap c", m.headMap("c"));
        p("tm tailMap c", m.tailMap("c"));
        p("tm subMap b..e", m.subMap("b", "e"));
        p("tm ceilingKey e", m.ceilingKey("e"));
        p("tm floorKey e", m.floorKey("e"));
        p("tm higherKey d", m.higherKey("d"));
        p("tm lowerKey d", m.lowerKey("d"));
        p("tm firstEntry", m.firstEntry());
        p("tm pollFirstEntry", m.pollFirstEntry());
        p("tm after poll", m);
        p("tm descendingMap", m.descendingMap());
        p("tm navigableKeySet", m.navigableKeySet());
        p("tm descendingKeySet", m.descendingKeySet());
        p("tm size/isEmpty", m.size() + "/" + m.isEmpty());
        p("tm containsValue 3", m.containsValue(3));
        p("tm values", m.values());
        p("tm entrySet", m.entrySet());
        t("tm null key", () -> m.put(null, 1));
        t("tm firstKey empty", () -> new TreeMap<String, Integer>().firstKey());
    }

    public static void main(String[] a) {
        properties();
        bigInteger();
        deque();
        linkedList();
        treeMap();
        System.out.println("DONE UtilFamilySweep");
    }
}
