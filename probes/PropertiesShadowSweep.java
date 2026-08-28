import java.io.*;
import java.util.*;

/** L3 / `java.util.Properties` — 92 bridge-with-code registrations, the single
 *  largest untouched family on the Phase-2 worklist.
 *
 *  `native-builtins/src/properties_sidetable.rs` exists because a synthetic
 *  `Properties` has a null inner `map` field, so ~30 methods are overridden to
 *  read a side table instead of the real `ConcurrentHashMap`. That is a
 *  re-implementation of the whole public surface of a JDK class, written from
 *  memory, and this probe asks its CONTRACT EDGES rather than its middle:
 *
 *    * the `defaults` chain, which applies to `getProperty`/`propertyNames`/
 *      `stringPropertyNames` and NOT to `get`/`keys`/`size`/`containsKey`;
 *    * the String filter — `getProperty` answers null for a non-String value
 *      even though `get` returns it, and `stringPropertyNames` drops any entry
 *      whose key OR value is not a String;
 *    * the `Hashtable` null axis — every one of put/get/containsKey/
 *      setProperty/getProperty refuses null with NPE;
 *    * `load`/`store` round trips: separators, escapes, `\\uXXXX`, continuation
 *      lines, comments, and the malformed-escape refusal.
 *
 *  DETERMINISM: nothing here prints an identity hash, an address, or a raw
 *  iteration order of a hash container. Every collection is sorted before
 *  printing; `store` output has its `#` comment lines (one of which is a
 *  timestamp) stripped and the remainder sorted.
 */
public class PropertiesShadowSweep {
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
    static void tv(String tag, Callable r) {
        try { p(tag, "ok " + String.valueOf(r.call())); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface Callable { Object call() throws Throwable; }

    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }
    static String sortedEnum(Enumeration<?> e) {
        List<String> l = new ArrayList<>();
        while (e.hasMoreElements()) l.add(String.valueOf(e.nextElement()));
        Collections.sort(l);
        return l.toString();
    }

    // ------------------------------------------------------------------
    // 1. the defaults chain: what consults it and what does not
    // ------------------------------------------------------------------
    static void defaultsChain() {
        Properties grand = new Properties();
        grand.setProperty("g", "gv");
        grand.setProperty("shared", "from-grand");
        Properties parent = new Properties(grand);
        parent.setProperty("p", "pv");
        parent.setProperty("shared", "from-parent");
        Properties child = new Properties(parent);
        child.setProperty("c", "cv");

        // getProperty walks the chain, two levels deep.
        p("getProperty own", child.getProperty("c"));
        p("getProperty parent", child.getProperty("p"));
        p("getProperty grandparent", child.getProperty("g"));
        p("getProperty shadowed by parent", child.getProperty("shared"));
        p("getProperty absent", child.getProperty("nope"));
        p("getProperty 2-arg default used", child.getProperty("nope", "DEF"));
        p("getProperty 2-arg default unused", child.getProperty("g", "DEF"));

        // get() does NOT walk the chain. This is the single most-confused pair
        // in the class and a side table keyed per object is where they merge.
        p("get own", child.get("c"));
        p("get parent (must be null)", child.get("p"));
        p("get grandparent (must be null)", child.get("g"));
        p("getOrDefault parent (must use default)", child.getOrDefault("p", "DEF"));

        // size/containsKey/isEmpty/contains are Hashtable's, own entries only.
        p("size own only", child.size());
        p("isEmpty", child.isEmpty());
        p("containsKey own", child.containsKey("c"));
        p("containsKey parent (must be false)", child.containsKey("p"));
        p("containsValue parent (must be false)", child.containsValue("pv"));
        p("contains parent (must be false)", child.contains("pv"));

        // propertyNames() DOES walk the chain; keys()/keySet() do not.
        p("propertyNames walks chain", sortedEnum(child.propertyNames()));
        p("keys own only", sortedEnum(child.keys()));
        p("keySet own only", sorted(child.keySet()));
        p("stringPropertyNames walks chain", sorted(child.stringPropertyNames()));
        p("elements own only", sortedEnum(child.elements()));
        p("values own only", sorted(child.values()));

        // A null defaults argument is legal and means "no chain".
        Properties nd = new Properties((Properties) null);
        nd.setProperty("a", "1");
        p("null defaults getProperty own", nd.getProperty("a"));
        p("null defaults getProperty absent", nd.getProperty("b"));
        p("null defaults propertyNames", sortedEnum(nd.propertyNames()));

        // A defaults chain containing a NON-String value: getProperty must
        // still answer null for it (the String filter applies at every level).
        Properties dnv = new Properties();
        dnv.put("num", Integer.valueOf(5));
        Properties over = new Properties(dnv);
        p("getProperty non-String in defaults", over.getProperty("num"));
        p("propertyNames includes non-String default key", sortedEnum(over.propertyNames()));
        p("stringPropertyNames drops non-String default value", sorted(over.stringPropertyNames()));
    }

    // ------------------------------------------------------------------
    // 2. the String filter, and get vs getProperty on the same entry
    // ------------------------------------------------------------------
    static void stringFilter() {
        Properties p1 = new Properties();
        p1.put("strkey", "strval");
        p1.put("intval", Integer.valueOf(42));
        p1.put(Integer.valueOf(7), "intkey");
        p1.put(Integer.valueOf(8), Integer.valueOf(9));

        p("get String value", p1.get("strkey"));
        p("getProperty String value", p1.getProperty("strkey"));
        p("get non-String value", p1.get("intval"));
        // The whole point: get returns the Integer, getProperty returns null.
        p("getProperty non-String value", p1.getProperty("intval"));
        p("getProperty non-String value with default", p1.getProperty("intval", "DEF"));
        p("get non-String key", p1.get(Integer.valueOf(7)));
        p("containsKey non-String key", p1.containsKey(Integer.valueOf(7)));
        p("size mixed", p1.size());
        p("keys mixed", sortedEnum(p1.keys()));
        tv("propertyNames mixed", () -> sortedEnum(p1.propertyNames()));
        // stringPropertyNames keeps ONLY entries whose key and value are both
        // Strings: 1 of the 4 above.
        tv("stringPropertyNames filters both sides", () -> sorted(p1.stringPropertyNames()));

        // setProperty returns the PREVIOUS value as an Object, including a
        // non-String previous value.
        p("setProperty returns null first time", p1.setProperty("fresh", "a"));
        p("setProperty returns old String", p1.setProperty("fresh", "b"));
        p("setProperty returns old non-String", p1.setProperty("intval", "now-a-string"));
        p("getProperty after setProperty over non-String", p1.getProperty("intval"));

        // put returns the previous value too.
        p("put returns null first time", p1.put("pk", "1"));
        p("put returns old", p1.put("pk", "2"));
        p("putIfAbsent present returns old", p1.putIfAbsent("pk", "3"));
        p("get after putIfAbsent present", p1.get("pk"));
        p("putIfAbsent absent returns null", p1.putIfAbsent("pk2", "9"));
        p("get after putIfAbsent absent", p1.get("pk2"));

        p("remove returns old", p1.remove("pk"));
        p("remove absent returns null", p1.remove("pk"));
        p("containsKey after remove", p1.containsKey("pk"));
    }

    // ------------------------------------------------------------------
    // 3. the Hashtable null axis — Properties refuses null on both sides
    // ------------------------------------------------------------------
    static void nullAxis() {
        Properties p1 = new Properties();
        p1.setProperty("a", "1");

        t("put null key", () -> p1.put(null, "v"));
        t("put null value", () -> p1.put("k", null));
        t("put null both", () -> p1.put(null, null));
        t("setProperty null key", () -> p1.setProperty(null, "v"));
        t("setProperty null value", () -> p1.setProperty("k", null));
        t("get null", () -> p1.get(null));
        t("getProperty null", () -> p1.getProperty(null));
        t("getProperty null 2-arg", () -> p1.getProperty(null, "d"));
        t("containsKey null", () -> p1.containsKey(null));
        t("contains null", () -> p1.contains(null));
        t("containsValue null", () -> p1.containsValue(null));
        t("remove null", () -> p1.remove(null));
        t("getOrDefault null key", () -> p1.getOrDefault(null, "d"));
        t("putIfAbsent null key", () -> p1.putIfAbsent(null, "v"));
        t("putIfAbsent null value", () -> p1.putIfAbsent("k2", null));
        t("putAll null", () -> p1.putAll(null));
        t("load null stream", () -> p1.load((InputStream) null));
        t("load null reader", () -> p1.load((Reader) null));
        t("store null stream", () -> p1.store((OutputStream) null, "c"));
        t("store null writer", () -> p1.store((Writer) null, "c"));
        t("computeIfAbsent null function", () -> p1.computeIfAbsent("a", null));
        t("computeIfAbsent null key", () -> p1.computeIfAbsent(null, k -> "v"));
        t("forEach null action", () -> p1.forEach(null));
        p("state unchanged after the null axis", sorted(p1.keySet()));
        p("size unchanged after the null axis", p1.size());
    }

    // ------------------------------------------------------------------
    // 4. constructor validation
    // ------------------------------------------------------------------
    static void ctors() {
        t("new Properties(-1)", () -> new Properties(-1));
        t("new Properties(0)", () -> new Properties(0));
        tv("new Properties(0) is empty", () -> new Properties(0).size());
        Properties d = new Properties();
        d.setProperty("d", "dv");
        Properties c = new Properties(d);
        p("ctor(defaults) chain works", c.getProperty("d"));
        p("ctor(defaults) size excludes defaults", c.size());

        // clone() is shallow and keeps the SAME defaults chain.
        c.setProperty("own", "ov");
        Properties cl = (Properties) c.clone();
        p("clone own entry", cl.getProperty("own"));
        p("clone sees defaults", cl.getProperty("d"));
        cl.setProperty("own", "changed");
        p("clone write does not affect original", c.getProperty("own"));
        p("clone size", cl.size());
    }

    // ------------------------------------------------------------------
    // 5. load / store — the parser is the biggest single body in the shim
    // ------------------------------------------------------------------
    static String storeToString(Properties p1, String comment) throws IOException {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        p1.store(bo, comment);
        String s = new String(bo.toByteArray(), "ISO-8859-1");
        List<String> keep = new ArrayList<>();
        for (String line : s.split("\n", -1)) {
            line = line.endsWith("\r") ? line.substring(0, line.length() - 1) : line;
            if (line.isEmpty() || line.startsWith("#") || line.startsWith("!")) continue;
            keep.add(line);
        }
        Collections.sort(keep);
        return keep.toString();
    }

    static Properties loadFrom(String text) throws IOException {
        Properties p1 = new Properties();
        p1.load(new ByteArrayInputStream(text.getBytes("ISO-8859-1")));
        return p1;
    }

    static void loadStore() throws Exception {
        // separators: '=', ':', whitespace, and any of them surrounded by space
        Properties a = loadFrom(
            "eq=1\n"
          + "colon:2\n"
          + "space 3\n"
          + "  lead = 4  \n"
          + "tab\t5\n"
          + "eqspace = 6\n"
          + "novalue\n"
          + "novalueeq=\n"
          + "novaluecolon:\n");
        p("load separators keys", sorted(a.keySet()));
        p("load eq", a.getProperty("eq"));
        p("load colon", a.getProperty("colon"));
        p("load space", a.getProperty("space"));
        p("load leading ws trimmed key", a.getProperty("lead"));
        p("load tab", a.getProperty("tab"));
        p("load eq with spaces", a.getProperty("eqspace"));
        p("load key with no value", a.getProperty("novalue"));
        p("load key with bare eq", a.getProperty("novalueeq"));
        p("load key with bare colon", a.getProperty("novaluecolon"));

        // comments, blank lines, and a '!' comment
        Properties b = loadFrom(
            "# hash comment=notakey\n"
          + "! bang comment=notakey\n"
          + "\n"
          + "   \n"
          + "real=yes\n"
          + "   # indented comment=notakey\n");
        p("load comments ignored", sorted(b.keySet()));
        p("load real after comments", b.getProperty("real"));

        // continuation lines: a trailing backslash joins, and the NEXT line's
        // leading whitespace is dropped.
        Properties c = loadFrom(
            "cont=one \\\n"
          + "     two \\\n"
          + "     three\n"
          + "endslash=a\\\\\n");
        p("load continuation", c.getProperty("cont"));
        p("load escaped trailing backslash", c.getProperty("endslash"));

        // escapes in the VALUE
        Properties d = loadFrom("esc=a\\tb\\nc\\rd\\fe\\=f\\:g\\ h\\\\i\\qj\n");
        p("load value escapes", d.getProperty("esc"));
        // escapes in the KEY: an escaped separator stays part of the key
        Properties e = loadFrom("a\\=b=v\na\\:c=w\na\\ d=x\n");
        p("load key escapes", sorted(e.keySet()));
        p("load key with escaped eq", e.getProperty("a=b"));
        p("load key with escaped colon", e.getProperty("a:c"));
        p("load key with escaped space", e.getProperty("a d"));

        // \\uXXXX
        Properties f = loadFrom("u=\\u00e9\\u0041\\u4e2d\n");
        p("load unicode escape", f.getProperty("u"));
        // A malformed escape is an IllegalArgumentException, not a silent pass.
        t("load malformed unicode", () -> loadFrom("bad=\\u00zz\n"));
        t("load short unicode at EOF", () -> loadFrom("bad=\\u00\n"));

        // ISO-8859-1 is the stream charset: a high byte is a Latin-1 char.
        Properties g = new Properties();
        g.load(new ByteArrayInputStream(new byte[] {'k', '=', (byte) 0xe9, '\n'}));
        p("load latin1 high byte", g.getProperty("k"));

        // The Reader overload takes the characters as given, no charset step.
        Properties h = new Properties();
        h.load(new StringReader("r=\u00e9\u4e2d\n"));
        p("load reader keeps chars", h.getProperty("r"));

        // load ADDS to what is already there and overwrites collisions.
        Properties i = new Properties();
        i.setProperty("keep", "old");
        i.setProperty("over", "old");
        i.load(new ByteArrayInputStream("over=new\nadded=x\n".getBytes("ISO-8859-1")));
        p("load keeps existing", i.getProperty("keep"));
        p("load overwrites", i.getProperty("over"));
        p("load adds", i.getProperty("added"));
        p("load size", i.size());

        // load must NOT write into the defaults.
        Properties dd = new Properties();
        dd.setProperty("d", "1");
        Properties ld = new Properties(dd);
        ld.load(new ByteArrayInputStream("x=2\n".getBytes("ISO-8859-1")));
        p("load did not touch defaults", dd.size());
        p("load own size", ld.size());

        // ---- store ----
        Properties s = new Properties();
        s.setProperty("plain", "v");
        s.setProperty("with space", "a b");
        s.setProperty("with=eq", "c=d");
        s.setProperty("with:colon", "e:f");
        s.setProperty("nl", "a\nb");
        s.setProperty("tab", "a\tb");
        s.setProperty("bs", "a\\b");
        s.setProperty("uni", "\u00e9\u4e2d");
        s.setProperty("lead", "  leading");
        p("store escapes", storeToString(s, "a comment"));
        p("store null comment", storeToString(s, null));

        // store excludes the defaults chain.
        Properties sd = new Properties();
        sd.setProperty("inherited", "no");
        Properties so = new Properties(sd);
        so.setProperty("own", "yes");
        p("store excludes defaults", storeToString(so, null));

        // round trip
        Properties rt = new Properties();
        rt.load(new ByteArrayInputStream(
            storeToStringRaw(s, null).getBytes("ISO-8859-1")));
        List<String> keys = new ArrayList<>(rt.stringPropertyNames());
        Collections.sort(keys);
        StringBuilder sb = new StringBuilder();
        for (String k : keys) sb.append(k).append("->").append(rt.getProperty(k)).append(';');
        p("store/load round trip", sb.toString());
        p("round trip equal", rt.equals(s));

        // the Writer overload
        StringWriter sw = new StringWriter();
        s.store(sw, null);
        Properties rw = new Properties();
        rw.load(new StringReader(sw.toString()));
        p("writer round trip uni", rw.getProperty("uni"));
        p("writer round trip nl", rw.getProperty("nl"));

        // the deprecated save() delegates to store and swallows IOException
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        s.save(bo, null);
        p("save wrote bytes", bo.size() > 0);
    }

    static String storeToStringRaw(Properties p1, String comment) throws IOException {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        p1.store(bo, comment);
        return new String(bo.toByteArray(), "ISO-8859-1");
    }

    // ------------------------------------------------------------------
    // 6. the Map surface the shim also overrides
    // ------------------------------------------------------------------
    static void mapSurface() {
        Properties p1 = new Properties();
        p1.setProperty("a", "1");
        p1.setProperty("b", "2");

        // putAll from a plain Map and from another Properties.
        Map<String, String> m = new LinkedHashMap<>();
        m.put("c", "3");
        m.put("a", "over");
        p1.putAll(m);
        p("putAll from Map", sorted(p1.keySet()));
        p("putAll overwrote", p1.getProperty("a"));

        Properties src = new Properties();
        src.setProperty("d", "4");
        Properties srcDefaults = new Properties();
        srcDefaults.setProperty("notcopied", "x");
        Properties src2 = new Properties(srcDefaults);
        src2.setProperty("e", "5");
        p1.putAll(src);
        p1.putAll(src2);
        p("putAll from Properties", sorted(p1.keySet()));
        p("putAll does not copy source defaults", p1.getProperty("notcopied"));

        // computeIfAbsent / computeIfPresent / compute / merge / replace
        p("computeIfAbsent absent", p1.computeIfAbsent("f", k -> "6"));
        p("computeIfAbsent present", p1.computeIfAbsent("f", k -> "7"));
        p("computeIfAbsent null result stores nothing", p1.computeIfAbsent("g", k -> null));
        p("containsKey after null-result compute", p1.containsKey("g"));
        p("computeIfPresent absent", p1.computeIfPresent("zz", (k, v) -> "x"));
        p("computeIfPresent present", p1.computeIfPresent("f", (k, v) -> v + "!"));
        p("computeIfPresent null removes", p1.computeIfPresent("f", (k, v) -> null));
        p("containsKey after computeIfPresent null", p1.containsKey("f"));
        p("compute", p1.compute("h", (k, v) -> String.valueOf(v) + "-h"));
        p("merge absent", p1.merge("i", "9", (x, y) -> String.valueOf(x) + String.valueOf(y)));
        p("merge present", p1.merge("i", "9", (x, y) -> String.valueOf(x) + String.valueOf(y)));
        p("replace present", p1.replace("i", "R"));
        p("replace absent", p1.replace("zz", "R"));
        p("replace 3-arg wrong old", p1.replace("i", "WRONG", "N"));
        p("replace 3-arg right old", p1.replace("i", "R", "N"));
        p("get after replace", p1.get("i"));
        p("remove 2-arg wrong value", p1.remove("i", "WRONG"));
        p("remove 2-arg right value", p1.remove("i", "N"));

        // forEach sees exactly the own entries
        StringBuilder fe = new StringBuilder();
        List<String> pairs = new ArrayList<>();
        p1.forEach((k, v) -> pairs.add(k + "=" + v));
        Collections.sort(pairs);
        p("forEach own entries", pairs.toString());

        // entrySet / values / keySet content
        p("keySet", sorted(p1.keySet()));
        p("values", sorted(p1.values()));
        p("entrySet", sorted(p1.entrySet()));
        p("entrySet size", p1.entrySet().size());
        p("keySet contains", p1.keySet().contains("a"));
        p("values contains", p1.values().contains("2"));

        // keySet is documented as a VIEW: removing through it removes from the
        // Properties. A snapshot cannot do this.
        Properties v = new Properties();
        v.setProperty("x", "1");
        v.setProperty("y", "2");
        boolean removed = v.keySet().remove("x");
        p("keySet.remove returned", removed);
        p("keySet.remove wrote through", v.size());
        p("keySet.remove key gone", v.containsKey("x"));

        Properties v2 = new Properties();
        v2.setProperty("x", "1");
        v2.setProperty("y", "2");
        Iterator<Object> it = v2.keySet().iterator();
        it.next();
        t("keySet iterator remove", it::remove);
        p("keySet iterator remove size", v2.size());

        Properties v3 = new Properties();
        v3.setProperty("x", "1");
        v3.setProperty("y", "2");
        p("values.remove", v3.values().remove("1"));
        p("values.remove wrote through", v3.size());

        Properties v4 = new Properties();
        v4.setProperty("x", "1");
        v4.setProperty("y", "2");
        p("entrySet.removeIf", v4.entrySet().removeIf(en -> "1".equals(en.getValue())));
        p("entrySet.removeIf wrote through", v4.size());

        Properties v5 = new Properties();
        v5.setProperty("x", "1");
        for (Map.Entry<Object, Object> en : v5.entrySet()) {
            final Map.Entry<Object, Object> fe2 = en;
            t("entry setValue", () -> fe2.setValue("2"));
        }
        p("entry setValue wrote through", v5.getProperty("x"));

        // clear
        Properties cl = new Properties();
        cl.setProperty("a", "1");
        cl.clear();
        p("clear size", cl.size());
        p("clear get", cl.getProperty("a"));
        p("clear isEmpty", cl.isEmpty());
        p("clear keySet", sorted(cl.keySet()));

        // clear does NOT clear the defaults
        Properties cd = new Properties();
        cd.setProperty("d", "1");
        Properties cc = new Properties(cd);
        cc.setProperty("o", "2");
        cc.clear();
        p("clear left defaults", cc.getProperty("d"));
        p("clear own size", cc.size());
    }

    // ------------------------------------------------------------------
    // 7. equals / hashCode / toString
    // ------------------------------------------------------------------
    static void identity() {
        Properties a = new Properties();
        Properties b = new Properties();
        p("empty equals empty", a.equals(b));
        p("equals null", a.equals(null));
        p("equals non-map", a.equals("s"));
        a.setProperty("k", "v");
        p("unequal after put", a.equals(b));
        b.setProperty("k", "v");
        p("equal again", a.equals(b));
        p("hashCode agrees", a.hashCode() == b.hashCode());

        // equals ignores the defaults chain entirely.
        Properties d = new Properties();
        d.setProperty("d", "1");
        Properties withDefaults = new Properties(d);
        withDefaults.setProperty("k", "v");
        p("equals ignores defaults", a.equals(withDefaults));

        // Properties equals a plain Hashtable with the same entries.
        Hashtable<Object, Object> h = new Hashtable<>();
        h.put("k", "v");
        p("equals a Hashtable", a.equals(h));
        p("Hashtable equals Properties", h.equals(a));

        // toString of a single entry is deterministic.
        p("toString one entry", a.toString());
        p("empty toString", new Properties().toString());
    }

    public static void main(String[] args) throws Exception {
        defaultsChain();
        stringFilter();
        nullAxis();
        ctors();
        loadStore();
        mapSurface();
        identity();
        System.out.println("ROWS " + rows);
        System.out.println("DONE PropertiesShadowSweep");
    }
}
