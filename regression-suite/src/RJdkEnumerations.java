import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.Hashtable;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.NoSuchElementException;
import java.util.Properties;
import java.util.concurrent.ConcurrentHashMap;

/**
 * JDK-only corpus: the legacy {@code Enumeration} getters --
 * {@code Properties.propertyNames()/keys()/elements()},
 * {@code ConcurrentHashMap.keys()/elements()} and
 * {@code Hashtable.keys()/elements()}.
 *
 * WHY THIS EXISTS. Each of these is a native CratonVM registers in the
 * ESSENTIAL set, so it survives {@code --jdk-only}; each then asks for an
 * Enumeration CARRIER whose class name exists in no JDK
 * ({@code cratonvm/internal/SnapshotEnumeration},
 * {@code java/util/Enumeration$Impl}). Strict mode refuses the fabrication --
 * correctly -- and the refusal surfaced at the application call site as
 * {@code NoClassDefFoundError}. Measured 2026-08-12: {@code Properties}
 * recovered (its native builds a real carrier), {@code ConcurrentHashMap} and
 * {@code Hashtable} did not. HotSpot 25 does all of it cleanly.
 *
 * WHAT IS ASSERTED, AND WHY NOT LESS. Two defects survived this year behind
 * {@code != null} and a count, so nothing here is satisfied by a non-null
 * carrier or by an element tally:
 *
 *   * every enumeration is DRAINED and its contents compared as a sorted
 *     multiset against the corresponding {@code keySet()} / {@code values()},
 *     so an empty, short, duplicated or null-padded carrier fails;
 *   * the drain loop is CAPPED, so a {@code hasMoreElements()} that never goes
 *     false fails instead of hanging the suite;
 *   * {@code nextElement()} past the end must throw
 *     {@code NoSuchElementException} -- a carrier that returns {@code null}
 *     there fails;
 *   * an EMPTY container is enumerated too, because the empty case is the one
 *     a broken carrier most easily gets right by accident;
 *   * one non-String value per container, because a String-only carrier is a
 *     real shape a fix could regress into.
 *
 * {@code Properties} is the CONTROL: it passed before this lane and must still
 * pass after it.
 *
 * Determinism: hash-ordered containers are NEVER printed in iteration order --
 * every emitted view is sorted. No system-property CONTENT is printed (the
 * harness diffs CK lines between VMs).
 */
public class RJdkEnumerations {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Every Enumeration carrier class name this family has been seen to mint,
     * in both the binary and the internal spelling. Compared by EQUALITY, never
     * by a prefix: {@code java/util/Enumeration$Impl} is spelled inside a JDK
     * package namespace, so a {@code !startsWith("cratonvm")} screen reports it
     * clean. That is the same mistake {@code RJdkLambdas}'s
     * {@code contains("$$Lambda")} makes against
     * {@code java.util.function.Predicate$$Lambda$And}.
     */
    static final String[] FABRICATED_CARRIERS = {
        "cratonvm.internal.SnapshotEnumeration",
        "cratonvm/internal/SnapshotEnumeration",
        "java.util.Enumeration$Impl",
        "java/util/Enumeration$Impl",
    };

    /** True iff {@code e}'s class is not one of the known fabrications. */
    static boolean carrierIsReal(Enumeration<?> e) {
        if (e == null) {
            return false;
        }
        String n = e.getClass().getName();
        for (int i = 0; i < FABRICATED_CARRIERS.length; i++) {
            if (n.equals(FABRICATED_CARRIERS[i])) {
                return false;
            }
        }
        return true;
    }

    /**
     * Drain {@code e} into a sorted list of {@code String.valueOf} images.
     *
     * {@code cap} bounds the loop: a carrier whose {@code hasMoreElements()}
     * never goes false must fail a check, not hang the runner.
     *
     * <p>The carrier's CLASS is screened here rather than at each call site,
     * because this is the one choke point every enumeration in the file passes
     * through, and because in COMPATIBLE mode -- which is what {@code SUITE=all}
     * with no {@code CRATONVM_ARGS} runs -- the fabricated carrier WORKS. Every
     * value assertion in this file passes against it. Under {@code --jdk-only}
     * the fabrication is refused and the run dies before any of this; the
     * screen is what gives the vector something to say in the other mode.
     */
    static List<String> drain(Enumeration<?> e, int cap, String what) {
        check(carrierIsReal(e), what + ": the carrier is a fabricated compatibility class, "
                + (e == null ? "null" : e.getClass().getName()));
        List<String> out = new ArrayList<>();
        while (e.hasMoreElements()) {
            out.add(String.valueOf(e.nextElement()));
            if (out.size() > cap) {
                throw new AssertionError(what + ": hasMoreElements() never terminated");
            }
        }
        // Past the end the JDK contract is an exception, not a null.
        boolean threw = false;
        try {
            e.nextElement();
        } catch (NoSuchElementException expected) {
            threw = true;
        }
        check(threw, what + ": nextElement() past the end must throw NoSuchElementException");
        Collections.sort(out);
        return out;
    }

    /** Sorted {@code String.valueOf} images of a collection -- the oracle side. */
    static List<String> sortedImages(Iterable<?> src) {
        List<String> out = new ArrayList<>();
        for (Object o : src) {
            out.add(String.valueOf(o));
        }
        Collections.sort(out);
        return out;
    }

    /**
     * The CONTROL family. {@code Properties.keys()} enumerates this object's own
     * keys; {@code propertyNames()} must ALSO surface the {@code defaults}
     * chain, and those two answers differing is itself part of the contract.
     */
    static void properties() {
        Properties defaults = new Properties();
        defaults.setProperty("base.only", "b0");
        defaults.setProperty("shared", "from-defaults");

        Properties p = new Properties(defaults);
        for (int i = 0; i < 12; i++) {
            p.setProperty("p" + i, "v" + i);
        }
        p.setProperty("shared", "from-child");

        List<String> ownKeys = sortedImages(p.keySet());
        check(ownKeys.size() == 13, "Properties own key count: " + ownKeys.size());

        // keys() -- own keys only, exactly keySet().
        check(drain(p.keys(), 64, "Properties.keys()").equals(ownKeys),
                "Properties.keys() must equal keySet() as a multiset");

        // elements() -- own values only, exactly values().
        check(drain(p.elements(), 64, "Properties.elements()").equals(sortedImages(p.values())),
                "Properties.elements() must equal values() as a multiset");

        // propertyNames() -- own keys PLUS the defaults chain, de-duplicated,
        // so `shared` appears once and `base.only` appears at all.
        List<String> names = drain(p.propertyNames(), 64, "Properties.propertyNames()");
        List<String> expectedNames = new ArrayList<>(ownKeys);
        expectedNames.add("base.only");
        Collections.sort(expectedNames);
        check(names.equals(expectedNames),
                "Properties.propertyNames() must add the defaults chain: " + names);
        check(names.indexOf("shared") == names.lastIndexOf("shared"),
                "propertyNames() must not repeat a shadowed name");
        check(p.getProperty("shared").equals("from-child"), "child value shadows the default");

        // The empty case, on a Properties with no defaults at all.
        Properties empty = new Properties();
        check(drain(empty.keys(), 4, "empty Properties.keys()").isEmpty(), "empty keys()");
        check(drain(empty.elements(), 4, "empty Properties.elements()").isEmpty(),
                "empty elements()");
        check(drain(empty.propertyNames(), 4, "empty Properties.propertyNames()").isEmpty(),
                "empty propertyNames()");

        System.out.println("CK RJdkEnumerations properties names=" + names.size()
                + " own=" + ownKeys.size() + " first=" + names.get(0));
    }

    /**
     * The first RED family. A natively-backed {@code ConcurrentHashMap} keeps
     * its entries in a segmented layout, which is why {@code keys()} is
     * shadowed at all -- but the shadow fabricated its carrier.
     */
    static void concurrentHashMap() {
        ConcurrentHashMap<String, Object> m = new ConcurrentHashMap<>();
        for (int i = 0; i < 16; i++) {
            m.put("c" + i, "w" + i);
        }
        // A non-String value: the carrier must hold arbitrary objects.
        m.put("boxed", Integer.valueOf(4242));
        check(m.size() == 17, "CHM size: " + m.size());

        List<String> keys = drain(m.keys(), 96, "ConcurrentHashMap.keys()");
        check(keys.equals(sortedImages(m.keySet())),
                "CHM.keys() must equal keySet() as a multiset: " + keys.size());
        check(keys.contains("boxed"), "CHM.keys() must contain every key");

        List<String> vals = drain(m.elements(), 96, "ConcurrentHashMap.elements()");
        check(vals.equals(sortedImages(m.values())),
                "CHM.elements() must equal values() as a multiset: " + vals.size());
        check(vals.contains("4242"), "CHM.elements() must carry the non-String value");

        // Duplicate values must survive as duplicates -- a set-shaped carrier
        // would silently collapse them.
        ConcurrentHashMap<String, String> dup = new ConcurrentHashMap<>();
        dup.put("a", "same");
        dup.put("b", "same");
        dup.put("c", "other");
        List<String> dupVals = drain(dup.elements(), 16, "ConcurrentHashMap dup elements()");
        check(dupVals.size() == 3, "CHM.elements() must keep duplicates: " + dupVals);
        check(dupVals.equals(sortedImages(dup.values())), "CHM dup multiset: " + dupVals);

        ConcurrentHashMap<String, String> empty = new ConcurrentHashMap<>();
        check(drain(empty.keys(), 4, "empty CHM.keys()").isEmpty(), "empty CHM keys()");
        check(drain(empty.elements(), 4, "empty CHM.elements()").isEmpty(), "empty CHM elements()");

        System.out.println("CK RJdkEnumerations chm keys=" + keys.size()
                + " vals=" + vals.size() + " dup=" + dupVals);
    }

    /**
     * The second RED family, and it named a DIFFERENT fabricated carrier than
     * the CHM one -- two minters, one shape.
     */
    static void hashtable() {
        Hashtable<String, Object> h = new Hashtable<>();
        for (int i = 0; i < 10; i++) {
            h.put("h" + i, "t" + i);
        }
        h.put("boxed", Integer.valueOf(77));
        check(h.size() == 11, "Hashtable size: " + h.size());

        List<String> keys = drain(h.keys(), 64, "Hashtable.keys()");
        check(keys.equals(sortedImages(h.keySet())),
                "Hashtable.keys() must equal keySet() as a multiset: " + keys.size());
        check(keys.contains("boxed"), "Hashtable.keys() must contain every key");

        List<String> vals = drain(h.elements(), 64, "Hashtable.elements()");
        check(vals.equals(sortedImages(h.values())),
                "Hashtable.elements() must equal values() as a multiset: " + vals.size());
        check(vals.contains("77"), "Hashtable.elements() must carry the non-String value");

        // keys() and elements() are DIFFERENT enumerations. CratonVM's
        // Hashtable carrier stamps a keys/values discriminator into a third
        // field; whatever the carrier is, the two answers must not converge.
        check(!keys.equals(vals), "Hashtable.keys() and elements() must differ");

        // Two enumerations taken from the same table are independent cursors.
        Enumeration<String> e1 = h.keys();
        Enumeration<String> e2 = h.keys();
        check(e1.hasMoreElements() && e2.hasMoreElements(), "two live enumerations");
        e1.nextElement();
        check(drain(e2, 64, "second Hashtable.keys()").equals(keys),
                "a second enumeration must not share the first's cursor");

        Hashtable<String, String> empty = new Hashtable<>();
        check(drain(empty.keys(), 4, "empty Hashtable.keys()").isEmpty(), "empty Hashtable keys()");
        check(drain(empty.elements(), 4, "empty Hashtable.elements()").isEmpty(),
                "empty Hashtable elements()");

        System.out.println("CK RJdkEnumerations hashtable keys=" + keys.size()
                + " vals=" + vals.size() + " distinct=" + (!keys.equals(vals)));
    }

    /**
     * The consumer that made these getters matter in the first place:
     * {@code Collections.list(Enumeration)} materialises the carrier through
     * real JDK bytecode, so a carrier that walks correctly under a direct
     * drain but not under the JDK's own consumer fails here.
     */
    static void jdkConsumer() {
        Hashtable<String, String> h = new Hashtable<>();
        h.put("x", "1");
        h.put("y", "2");
        h.put("z", "3");
        List<String> viaList = new ArrayList<>(Collections.list(h.keys()));
        Collections.sort(viaList);
        check(viaList.equals(sortedImages(h.keySet())), "Collections.list(Hashtable.keys()): " + viaList);

        ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>();
        m.put("p", "9");
        m.put("q", "8");
        List<String> chmList = new ArrayList<>(Collections.list(m.elements()));
        Collections.sort(chmList);
        check(chmList.equals(sortedImages(m.values())), "Collections.list(CHM.elements()): " + chmList);

        Properties p = new Properties();
        p.setProperty("k", "v");
        List<?> propList = Collections.list(p.propertyNames());
        check(propList.size() == 1 && "k".equals(propList.get(0)),
                "Collections.list(Properties.propertyNames()): " + propList);

        // The enumerations feed an ordinary Map, unchanged -- the carrier is
        // the only thing under test, so the maps behind it must be untouched.
        Map<String, String> back = new java.util.TreeMap<>();
        for (Map.Entry<String, String> e : h.entrySet()) {
            back.put(e.getKey(), e.getValue());
        }
        check(back.toString().equals("{x=1, y=2, z=3}"), "backing table unchanged: " + back);

        System.out.println("CK RJdkEnumerations consumer list=" + viaList + " chm=" + chmList
                + " back=" + back);
    }

    /**
     * The non-enumerating half of the carrier screen.
     *
     * <p>A name blacklist only catches fabrications someone already wrote down.
     * The shape below catches the NEXT one: in the real java.base bytecode
     * {@code Hashtable$Enumerator} and {@code ConcurrentHashMap}'s
     * {@code KeyIterator}/{@code ValueIterator} implement {@code Enumeration}
     * AND {@code Iterator} over ONE cursor -- that dual role is the entire
     * reason those classes exist rather than a plain enumeration. A substituted
     * carrier, fabricated or merely "real but not the JDK's"
     * ({@code Collections.enumeration(snapshot)} is the tempting shim, and it
     * passes every value assertion in this file), is an {@code Enumeration} and
     * nothing else.
     *
     * <p>Stated plainly: this is an IMPLEMENTATION pin, not a JDK contract. It
     * is taken because a substituted carrier is exactly what this vector exists
     * to detect, and because under {@code --jdk-only} the real bytecode is what
     * must run, which makes the pin exact rather than hopeful.
     *
     * <p>EMPTY containers are excluded and the exclusion is load-bearing:
     * {@code new Hashtable<>().keys()} short-circuits to
     * {@code Collections.emptyEnumeration()}, whose carrier is NOT an
     * {@code Iterator}. Asserting this on the empty case would be a false red
     * on HotSpot itself.
     */
    static void carriersAreTheJdksOwnDualInterfaceEnumerators() {
        Hashtable<String, String> h = new Hashtable<>();
        for (int i = 0; i < 6; i++) {
            h.put("k" + i, "v" + i);
        }
        Enumeration<String> he = h.keys();
        check(he instanceof Iterator,
                "Hashtable.keys()'s carrier must be the JDK's own dual Enumeration/Iterator, got "
                        + he.getClass().getName());
        check(h.elements() instanceof Iterator,
                "Hashtable.elements()'s carrier must be the JDK's own dual Enumeration/Iterator, got "
                        + h.elements().getClass().getName());
        // ONE object, ONE cursor. Taking one element through the Enumeration
        // face and the rest through the Iterator face must yield the table
        // exactly once: a carrier that keeps two cursors repeats an element,
        // and one that restarts yields size()+1.
        List<String> mixed = new ArrayList<>();
        mixed.add(String.valueOf(he.nextElement()));
        Iterator<?> hi = (Iterator<?>) he;
        while (hi.hasNext()) {
            mixed.add(String.valueOf(hi.next()));
            if (mixed.size() > 64) {
                throw new AssertionError("Hashtable carrier: the Iterator face never terminated");
            }
        }
        Collections.sort(mixed);
        check(mixed.equals(sortedImages(h.keySet())),
                "the Enumeration and Iterator faces must share one cursor: " + mixed);

        ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>();
        for (int i = 0; i < 6; i++) {
            m.put("c" + i, "w" + i);
        }
        Enumeration<String> ce = m.keys();
        check(ce instanceof Iterator,
                "ConcurrentHashMap.keys()'s carrier must be the JDK's own dual"
                        + " Enumeration/Iterator, got " + ce.getClass().getName());
        check(m.elements() instanceof Iterator,
                "ConcurrentHashMap.elements()'s carrier must be the JDK's own dual"
                        + " Enumeration/Iterator, got " + m.elements().getClass().getName());
        List<String> cmixed = new ArrayList<>();
        cmixed.add(String.valueOf(ce.nextElement()));
        Iterator<?> ci = (Iterator<?>) ce;
        while (ci.hasNext()) {
            cmixed.add(String.valueOf(ci.next()));
            if (cmixed.size() > 64) {
                throw new AssertionError("CHM carrier: the Iterator face never terminated");
            }
        }
        Collections.sort(cmixed);
        check(cmixed.equals(sortedImages(m.keySet())),
                "the Enumeration and Iterator faces must share one cursor: " + cmixed);

        System.out.println("CK RJdkEnumerations carriers dual=4 sharedCursor=2");
    }

    public static void main(String[] args) {
        properties();
        concurrentHashMap();
        hashtable();
        jdkConsumer();
        carriersAreTheJdksOwnDualInterfaceEnumerators();
        System.out.println("CK RJdkEnumerations checks=" + checks);
        System.out.println("PASS RJdkEnumerations (" + checks + " checks)");
    }
}
