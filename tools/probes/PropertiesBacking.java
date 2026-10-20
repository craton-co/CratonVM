import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.io.StringReader;
import java.io.StringWriter;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Properties;
import java.util.Set;
import java.util.TreeSet;

/**
 * The whole declared surface of `java.util.Properties`, against a HotSpot
 * oracle.
 *
 * # Why this is its own file
 *
 * `Properties` is the one collection in this VM whose entries do NOT live where
 * its fields say they do. A String->String pair goes into a Rust side-table
 * (`native-builtins/src/properties_sidetable.rs`); anything else goes into the
 * real `map` `ConcurrentHashMap`; and the `table` this class inherits from
 * `Hashtable` holds NOTHING -- `map_carrier_class_for_receiver` records the
 * measurement, `occupied=0` with `size=2`. HotSpot's own `new Properties()`
 * leaves that field null, and as of 2026-09-12 so does this VM's, which is what
 * this file exists to make safe.
 *
 * The failure mode of getting that wrong is SILENCE, not an exception: a method
 * that consults the wrong store answers "absent" or "empty" and no stack trace
 * says so. The surface is also split across two crates and re-registered three
 * times during `vm_init`, so which methods are served by which implementation
 * is a property of registration ORDER rather than of any one function. Both
 * facts point the same way -- cover the whole declared surface, not the method
 * a change happens to touch.
 *
 * Every method `javap -p java.util.Properties` lists is exercised here except
 * the four that are out of scope by construction: `loadFromXML`/`storeToXML`
 * (a separate parser subsystem), `rehash` (protected, and a no-op on a class
 * whose entries are not in the table it rehashes), and
 * `writeHashtable`/`readHashtable` (package-private serialization hooks, which
 * `RPropertiesClone` and the serialization corpus reach instead).
 *
 * Runs identically on HotSpot, which is the oracle rather than a description of
 * current behaviour. Exits non-zero on any mismatch, so it is usable as a gate.
 */
public class PropertiesBacking {
    static int failures = 0;

    public static void main(String[] args) {
        section("construction", PropertiesBacking::construction);
        section("string accessors", PropertiesBacking::stringAccessors);
        section("defaults chain", PropertiesBacking::defaultsChain);
        section("map surface", PropertiesBacking::mapSurface);
        section("non-string entries", PropertiesBacking::nonStringEntries);
        section("views", PropertiesBacking::views);
        section("enumerations", PropertiesBacking::enumerations);
        section("functional", PropertiesBacking::functional);
        section("compute and merge", PropertiesBacking::computeAndMerge);
        section("load and store", PropertiesBacking::loadAndStore);
        section("clone and equality", PropertiesBacking::cloneAndEquality);
        section("list", PropertiesBacking::list);
        section("empty reads", PropertiesBacking::emptyReads);

        System.out.println(failures == 0 ? "PASS PropertiesBacking"
                                         : "FAIL PropertiesBacking (" + failures + ")");
        System.out.println("PROPS_END");
        if (failures != 0) System.exit(1);
    }

    /** A throw inside one section is a recorded failure, not the end of the run. */
    static void section(String name, Runnable body) {
        try {
            body.run();
        } catch (Throwable t) {
            failures++;
            System.out.println("  ERROR " + name + ": " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        }
    }

    static void check(String what, String expected, String actual) {
        if (!expected.equals(actual)) {
            failures++;
            System.out.println("  MISMATCH " + what + ": expected <" + expected
                    + "> got <" + actual + ">");
        }
    }

    static Properties of(String... kv) {
        Properties p = new Properties();
        for (int i = 0; i < kv.length; i += 2) p.setProperty(kv[i], kv[i + 1]);
        return p;
    }

    /** `Properties` iterates in no defined order, so compare sorted. */
    static String sorted(java.util.Collection<?> c) {
        List<String> out = new ArrayList<>();
        for (Object o : c) out.add(String.valueOf(o));
        Collections.sort(out);
        return out.toString();
    }

    /** All three constructors. */
    static void construction() {
        Properties empty = new Properties();
        check("new() size", "0", String.valueOf(empty.size()));
        check("new() isEmpty", "true", String.valueOf(empty.isEmpty()));
        // `table` is null on HotSpot after this constructor, and the class
        // still has to behave: the first write must materialise whatever it
        // needs rather than being dropped.
        empty.setProperty("a", "1");
        check("new() then set", "1", empty.getProperty("a"));
        check("new() then size", "1", String.valueOf(empty.size()));

        // `Properties(int)` is a capacity hint and nothing else.
        Properties sized = new Properties(64);
        check("new(int) size", "0", String.valueOf(sized.size()));
        sized.setProperty("b", "2");
        check("new(int) then get", "2", sized.getProperty("b"));

        Properties withDefaults = new Properties(of("d", "dv"));
        check("new(Properties) size", "0", String.valueOf(withDefaults.size()));
        check("new(Properties) reads default", "dv", withDefaults.getProperty("d"));
    }

    /** `setProperty` / `getProperty` / `getProperty(String,String)`. */
    static void stringAccessors() {
        Properties p = new Properties();
        check("setProperty returns null", "null", String.valueOf(p.setProperty("k", "v")));
        check("setProperty returns prev", "v", String.valueOf(p.setProperty("k", "w")));
        check("getProperty", "w", p.getProperty("k"));
        check("getProperty absent", "null", String.valueOf(p.getProperty("nope")));
        check("getProperty default used", "fb", p.getProperty("nope", "fb"));
        check("getProperty default unused", "w", p.getProperty("k", "fb"));

        // An empty-String key is a real key and is the boundary the side-table
        // treats differently from every other String.
        p.setProperty("", "blank");
        check("empty key get", "blank", p.getProperty(""));
        check("empty key size", "2", String.valueOf(p.size()));
        check("empty key containsKey", "true", String.valueOf(p.containsKey("")));
        // Removing it must actually remove it. The empty String is the one key
        // this VM stores somewhere other than where every other String key
        // goes, so `remove` has a second path to get right here.
        check("empty key remove", "blank", String.valueOf(p.remove("")));
        check("empty key size after remove", "1", String.valueOf(p.size()));
        check("empty key gone", "false", String.valueOf(p.containsKey("")));
    }

    /** The `defaults` fallback chain, including a two-deep one. */
    static void defaultsChain() {
        Properties base = of("shared", "base", "onlyBase", "b");
        Properties mid = new Properties(base);
        mid.setProperty("shared", "mid");
        mid.setProperty("onlyMid", "m");
        Properties top = new Properties(mid);
        top.setProperty("onlyTop", "t");

        check("own wins", "t", top.getProperty("onlyTop"));
        check("one level down", "mid", top.getProperty("shared"));
        check("two levels down", "b", top.getProperty("onlyBase"));
        check("absent through chain", "null", String.valueOf(top.getProperty("nope")));

        // `size` / `containsKey` / `get` are Map operations and do NOT consult
        // defaults -- only `getProperty` and the two name enumerations do.
        check("size excludes defaults", "1", String.valueOf(top.size()));
        check("get excludes defaults", "null", String.valueOf(top.get("shared")));
        check("containsKey excludes defaults", "false", String.valueOf(top.containsKey("shared")));

        // `propertyNames` and `stringPropertyNames` DO walk the chain.
        check("stringPropertyNames walks chain", "[onlyBase, onlyMid, onlyTop, shared]",
                sorted(top.stringPropertyNames()));
        List<String> names = new ArrayList<>();
        for (Enumeration<?> e = top.propertyNames(); e.hasMoreElements(); ) {
            names.add(String.valueOf(e.nextElement()));
        }
        Collections.sort(names);
        check("propertyNames walks chain", "[onlyBase, onlyMid, onlyTop, shared]", names.toString());
    }

    /** The `Map` / `Hashtable` half: `put` / `get` / `remove` / `clear` / `putAll`. */
    static void mapSurface() {
        Properties p = new Properties();
        check("put returns null", "null", String.valueOf(p.put("a", "1")));
        check("put returns prev", "1", String.valueOf(p.put("a", "2")));
        check("get", "2", String.valueOf(p.get("a")));
        check("getOrDefault present", "2", String.valueOf(p.getOrDefault("a", "z")));
        check("getOrDefault absent", "z", String.valueOf(p.getOrDefault("q", "z")));
        check("containsKey", "true", String.valueOf(p.containsKey("a")));
        check("containsValue", "true", String.valueOf(p.containsValue("2")));
        check("contains", "true", String.valueOf(p.contains("2")));
        check("containsValue absent", "false", String.valueOf(p.containsValue("nope")));

        check("putIfAbsent present", "2", String.valueOf(p.putIfAbsent("a", "3")));
        check("putIfAbsent absent", "null", String.valueOf(p.putIfAbsent("b", "9")));
        check("putIfAbsent stored", "9", String.valueOf(p.get("b")));

        check("replace(k,v)", "9", String.valueOf(p.replace("b", "10")));
        check("replace(k,v) absent", "null", String.valueOf(p.replace("zz", "0")));
        check("replace(k,old,new) hit", "true", String.valueOf(p.replace("b", "10", "11")));
        check("replace(k,old,new) miss", "false", String.valueOf(p.replace("b", "10", "12")));
        check("replace result", "11", String.valueOf(p.get("b")));

        check("remove(k,v) miss", "false", String.valueOf(p.remove("b", "nope")));
        check("remove(k,v) hit", "true", String.valueOf(p.remove("b", "11")));
        check("remove(k) hit", "2", String.valueOf(p.remove("a")));
        check("remove(k) miss", "null", String.valueOf(p.remove("a")));
        check("size after removes", "0", String.valueOf(p.size()));

        Map<String, String> src = new HashMap<>();
        src.put("x", "1");
        src.put("y", "2");
        p.putAll(src);
        check("putAll size", "2", String.valueOf(p.size()));
        check("putAll get", "1", String.valueOf(p.get("x")));

        p.clear();
        check("clear size", "0", String.valueOf(p.size()));
        check("clear isEmpty", "true", String.valueOf(p.isEmpty()));
        check("clear get", "null", String.valueOf(p.get("x")));
    }

    /**
     * `put(Object,Object)` is inherited from `Hashtable` and accepts ANY types;
     * only `setProperty`/`getProperty` are String-typed. This is the axis on
     * which the two backing stores diverge, so it is asserted on its own.
     */
    static void nonStringEntries() {
        Properties p = new Properties();
        p.setProperty("s", "str");
        p.put("i", Integer.valueOf(7));
        p.put(Integer.valueOf(3), "byIntKey");

        check("size mixes both stores", "3", String.valueOf(p.size()));
        check("non-String value", "7", String.valueOf(p.get("i")));
        check("non-String key", "byIntKey", String.valueOf(p.get(Integer.valueOf(3))));
        check("containsKey non-String", "true", String.valueOf(p.containsKey(Integer.valueOf(3))));
        check("containsValue non-String", "true",
                String.valueOf(p.containsValue(Integer.valueOf(7))));

        // `getProperty` is String-typed and returns null for a non-String
        // VALUE rather than calling toString on it.
        check("getProperty of non-String value", "null", String.valueOf(p.getProperty("i")));
        // ...and `stringPropertyNames` reports only the pairs that are String
        // on BOTH sides.
        check("stringPropertyNames filters", "[s]", sorted(p.stringPropertyNames()));

        check("keySet sees both", "[3, i, s]", sorted(p.keySet()));
        check("values sees both", "[7, byIntKey, str]", sorted(p.values()));

        check("remove non-String key", "byIntKey", String.valueOf(p.remove(Integer.valueOf(3))));
        check("size after", "2", String.valueOf(p.size()));
    }

    /** `keySet` / `values` / `entrySet`. */
    static void views() {
        Properties p = of("a", "1", "b", "2", "c", "3");
        check("keySet", "[a, b, c]", sorted(p.keySet()));
        check("values", "[1, 2, 3]", sorted(p.values()));
        check("keySet size", "3", String.valueOf(p.keySet().size()));
        check("values size", "3", String.valueOf(p.values().size()));

        Set<Map.Entry<Object, Object>> es = p.entrySet();
        check("entrySet size", "3", String.valueOf(es.size()));
        List<String> pairs = new ArrayList<>();
        for (Map.Entry<Object, Object> e : es) pairs.add(e.getKey() + "=" + e.getValue());
        Collections.sort(pairs);
        check("entrySet contents", "[a=1, b=2, c=3]", pairs.toString());

        check("keySet contains", "true", String.valueOf(p.keySet().contains("a")));
        check("values contains", "true", String.valueOf(p.values().contains("2")));

        // `toString` is the Hashtable one and must agree with the views.
        String s = p.toString();
        check("toString brackets", "true",
                String.valueOf(s.startsWith("{") && s.endsWith("}")));
        check("toString has all three", "true",
                String.valueOf(s.contains("a=1") && s.contains("b=2") && s.contains("c=3")));
    }

    /** `keys` / `elements` / `propertyNames`, the `Dictionary` half. */
    static void enumerations() {
        Properties p = of("a", "1", "b", "2");

        List<String> ks = new ArrayList<>();
        for (Enumeration<Object> e = p.keys(); e.hasMoreElements(); ) {
            ks.add(String.valueOf(e.nextElement()));
        }
        Collections.sort(ks);
        check("keys", "[a, b]", ks.toString());

        List<String> vs = new ArrayList<>();
        for (Enumeration<Object> e = p.elements(); e.hasMoreElements(); ) {
            vs.add(String.valueOf(e.nextElement()));
        }
        Collections.sort(vs);
        check("elements", "[1, 2]", vs.toString());

        check("keys on empty", "false",
                String.valueOf(new Properties().keys().hasMoreElements()));
        check("elements on empty", "false",
                String.valueOf(new Properties().elements().hasMoreElements()));
    }

    /** `forEach` / `replaceAll`. */
    static void functional() {
        Properties p = of("a", "1", "b", "2");

        TreeSet<String> seen = new TreeSet<>();
        p.forEach((k, v) -> seen.add(k + "=" + v));
        check("forEach", "[a=1, b=2]", seen.toString());

        p.replaceAll((k, v) -> v + "!");
        check("replaceAll a", "1!", String.valueOf(p.get("a")));
        check("replaceAll b", "2!", String.valueOf(p.get("b")));
        check("replaceAll size", "2", String.valueOf(p.size()));
        // A replaced value must still be visible to the STRING accessor, which
        // is the reader that goes through the other store.
        check("replaceAll via getProperty", "1!", p.getProperty("a"));

        check("forEach on empty", "0", String.valueOf(countForEach(new Properties())));
    }

    static int countForEach(Properties p) {
        int[] n = {0};
        p.forEach((k, v) -> n[0]++);
        return n[0];
    }

    /** `computeIfAbsent` / `computeIfPresent` / `compute` / `merge`. */
    static void computeAndMerge() {
        Properties p = new Properties();

        check("computeIfAbsent creates", "1", String.valueOf(p.computeIfAbsent("a", k -> "1")));
        check("computeIfAbsent keeps", "1", String.valueOf(p.computeIfAbsent("a", k -> "2")));
        check("computeIfAbsent stored", "1", String.valueOf(p.get("a")));

        check("computeIfPresent updates", "1x",
                String.valueOf(p.computeIfPresent("a", (k, v) -> v + "x")));
        check("computeIfPresent skips", "null",
                String.valueOf(p.computeIfPresent("zz", (k, v) -> "no")));
        check("computeIfPresent removes on null", "null",
                String.valueOf(p.computeIfPresent("a", (k, v) -> null)));
        check("size after removing compute", "0", String.valueOf(p.size()));

        check("compute creates", "n", String.valueOf(p.compute("b", (k, v) -> "n")));
        check("compute sees prev", "n+", String.valueOf(p.compute("b", (k, v) -> v + "+")));
        check("compute removes on null", "null", String.valueOf(p.compute("b", (k, v) -> null)));
        check("size after compute remove", "0", String.valueOf(p.size()));

        check("merge absent", "m", String.valueOf(p.merge("c", "m", (a, b) -> String.valueOf(a) + b)));
        check("merge present", "mz", String.valueOf(p.merge("c", "z", (a, b) -> String.valueOf(a) + b)));
        check("merge removes on null", "null", String.valueOf(p.merge("c", "q", (a, b) -> null)));
        check("size after merge remove", "0", String.valueOf(p.size()));
    }

    /** `load` / `store` / `save`, both stream and reader/writer overloads. */
    static void loadAndStore() {
        String text = "a=1\nb : 2\n# comment\n! also comment\nc 3\nd=multi\\\n  line\n";

        Properties fromStream = new Properties();
        try {
            fromStream.load(new ByteArrayInputStream(text.getBytes(StandardCharsets.ISO_8859_1)));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        check("load(stream) size", "4", String.valueOf(fromStream.size()));
        check("load a", "1", fromStream.getProperty("a"));
        check("load b (colon)", "2", fromStream.getProperty("b"));
        check("load c (space)", "3", fromStream.getProperty("c"));
        check("load d (continuation)", "multiline", fromStream.getProperty("d"));

        Properties fromReader = new Properties();
        try {
            fromReader.load(new StringReader(text));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        check("load(reader) agrees", "4", String.valueOf(fromReader.size()));
        check("load(reader) d", "multiline", fromReader.getProperty("d"));

        // Round-trip through both writers. The header line and the timestamp
        // line differ per run, so assert on the reloaded CONTENT.
        Properties src = of("x", "1", "y", "two words", "z", "with=equals");
        StringWriter sw = new StringWriter();
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        ByteArrayOutputStream saved = new ByteArrayOutputStream();
        try {
            src.store(sw, "header");
            src.store(bos, "header");
            src.save(saved, "header");
        } catch (Exception e) {
            throw new RuntimeException(e);
        }

        check("store(writer) round-trip", "{x=1, y=two words, z=with=equals}",
                reloadSorted(sw.toString()));
        check("store(stream) round-trip", "{x=1, y=two words, z=with=equals}",
                reloadSorted(new String(bos.toByteArray(), StandardCharsets.ISO_8859_1)));
        check("save round-trip", "{x=1, y=two words, z=with=equals}",
                reloadSorted(new String(saved.toByteArray(), StandardCharsets.ISO_8859_1)));

        // A `load` onto a Properties that already has entries MERGES.
        Properties merged = of("keep", "yes");
        try {
            merged.load(new InputStreamReader(
                    new ByteArrayInputStream("added=1\n".getBytes(StandardCharsets.ISO_8859_1)),
                    StandardCharsets.ISO_8859_1));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        check("load merges", "2", String.valueOf(merged.size()));
        check("load kept existing", "yes", merged.getProperty("keep"));
        check("load added new", "1", merged.getProperty("added"));
    }

    /** Reload serialized text and print it in a stable order. */
    static String reloadSorted(String text) {
        Properties back = new Properties();
        try {
            back.load(new StringReader(text));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        TreeSet<String> pairs = new TreeSet<>();
        for (String k : back.stringPropertyNames()) pairs.add(k + "=" + back.getProperty(k));
        return "{" + String.join(", ", pairs) + "}";
    }

    /** `clone` / `equals` / `hashCode`. */
    static void cloneAndEquality() {
        Properties p = of("a", "1", "b", "2");
        Properties c = (Properties) p.clone();
        check("clone size", "2", String.valueOf(c.size()));
        check("clone get", "1", c.getProperty("a"));
        c.setProperty("c", "3");
        check("clone is independent", "2", String.valueOf(p.size()));
        check("clone kept its own", "3", String.valueOf(c.size()));

        // The defaults reference is SHARED by clone(), not copied.
        Properties withDef = new Properties(of("d", "dv"));
        withDef.setProperty("own", "o");
        Properties cd = (Properties) withDef.clone();
        check("clone keeps defaults", "dv", cd.getProperty("d"));

        check("equals self", "true", String.valueOf(p.equals(p)));
        check("equals same content", "true", String.valueOf(p.equals(of("a", "1", "b", "2"))));
        check("equals different", "false", String.valueOf(p.equals(of("a", "9"))));
        check("equals null", "false", String.valueOf(p.equals(null)));
        check("hashCode agrees with equals", "true",
                String.valueOf(p.hashCode() == of("a", "1", "b", "2").hashCode()));

        // `Properties` is a `Map`, so it must equal a plain Map of the same
        // entries -- the AbstractMap contract Hashtable implements.
        Map<Object, Object> plain = new HashMap<>();
        plain.put("a", "1");
        plain.put("b", "2");
        check("equals a plain Map", "true", String.valueOf(p.equals(plain)));
    }

    /** `list(PrintStream)` / `list(PrintWriter)`. */
    static void list() {
        Properties p = of("a", "1");
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        p.list(new PrintStream(bos));
        String out = new String(bos.toByteArray(), StandardCharsets.ISO_8859_1);
        check("list(PrintStream) has entry", "true", String.valueOf(out.contains("a=1")));

        StringWriter sw = new StringWriter();
        p.list(new java.io.PrintWriter(sw, true));
        check("list(PrintWriter) has entry", "true", String.valueOf(sw.toString().contains("a=1")));
    }

    /**
     * Every read on a `Properties` whose `table` was never allocated. This is
     * the section that would have failed if deferring that array had broken
     * anything, so it asserts the reads rather than assuming them.
     */
    static void emptyReads() {
        Properties p = new Properties();
        check("size", "0", String.valueOf(p.size()));
        check("isEmpty", "true", String.valueOf(p.isEmpty()));
        check("get", "null", String.valueOf(p.get("a")));
        check("getProperty", "null", String.valueOf(p.getProperty("a")));
        check("getProperty default", "d", p.getProperty("a", "d"));
        check("containsKey", "false", String.valueOf(p.containsKey("a")));
        check("containsValue", "false", String.valueOf(p.containsValue("a")));
        check("contains", "false", String.valueOf(p.contains("a")));
        check("remove", "null", String.valueOf(p.remove("a")));
        check("keySet", "[]", sorted(p.keySet()));
        check("values", "[]", sorted(p.values()));
        check("entrySet size", "0", String.valueOf(p.entrySet().size()));
        check("stringPropertyNames", "[]", sorted(p.stringPropertyNames()));
        check("toString", "{}", p.toString());
        check("hashCode", "0", String.valueOf(p.hashCode()));
        check("equals another empty", "true", String.valueOf(p.equals(new Properties())));
        check("clone of empty", "0", String.valueOf(((Properties) p.clone()).size()));
        p.clear();
        check("clear on empty", "0", String.valueOf(p.size()));
        p.replaceAll((k, v) -> v);
        check("replaceAll on empty", "0", String.valueOf(p.size()));
        check("putAll empty map", "0", String.valueOf(sizeAfterPutAllEmpty(p)));

        // ...and the first write onto that same object still lands.
        p.put("late", "yes");
        check("write after empty reads", "yes", String.valueOf(p.get("late")));
        check("size after late write", "1", String.valueOf(p.size()));
    }

    static int sizeAfterPutAllEmpty(Properties p) {
        p.putAll(new HashMap<Object, Object>());
        return p.size();
    }
}
