import java.nio.CharBuffer;
import java.text.Normalizer;
import java.io.ByteArrayInputStream;
import java.io.File;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.StringReader;
import java.math.BigInteger;
import java.net.URI;
import java.net.URISyntaxException;
import java.net.URL;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Enumeration;
import java.util.List;
import java.util.Properties;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.Vector;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.Map;
import java.util.Optional;
import java.util.PriorityQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicLongArray;

/**
 * First {@code NativeKind::Bridge} census vector.
 *
 * <h2>What a Bridge is, and why that changes the question</h2>
 *
 * <p>{@code Intrinsic} natives replace a method the VM wants to be fast.
 * A {@code Bridge} native replaces a method that <b>real JDK bytecode also
 * implements</b> — the class file is on the boot path, the method has a
 * {@code Code} attribute, and the VM chooses the Rust function anyway. So the
 * question is not only "is the answer right" but "does it agree with the
 * bytecode it shadows". Every triple this file drives was confirmed present in
 * a {@code --dump-native-registry} dump (schema 4) with {@code kind ==
 * "bridge"} and, for all but a handful noted in the record, with
 * {@code real_declaring_method.has_code == true} and {@code acc_native ==
 * false} — i.e. a working JDK implementation exists and is being shadowed.
 *
 * <p>See docs/known-issues/jdk-only/W8-E29-1-bridge-census-round-1.md for the
 * population arithmetic, the four-way triage of the 9,799 rows, and the list of
 * what a second generation would have to reach.
 *
 * <h2>The risk model — why these eleven families</h2>
 *
 * <p>The registry says {@code Bridge} is by far the largest category (9,799
 * rows, 8,777 distinct triples) and that a hello-world run invoked 35 of them.
 * A census cannot drive 8,777 triples, so the families below are the slice
 * reachable from ordinary Java that maximises exposure to the six hazards this
 * project keeps paying for:
 *
 * <ol>
 *   <li><b>A Rust panic reachable from bytecode.</b> A panic is not a Java
 *       throwable — it terminates the VM. Every index, capacity, count and
 *       divisor below is driven out of range on purpose: {@link #sbidx()},
 *       {@link #bytebuf()}, {@link #atomarr()}, {@link #vector()},
 *       {@link #bigint()}. {@code BigInteger.ONE.shiftLeft(Integer.MIN_VALUE)}
 *       is the sharpest of these: HotSpot answers {@code 0} by treating
 *       {@code -n} as UNSIGNED, while a Rust {@code -n} on {@code i32::MIN} is
 *       an overflow that panics in debug and wraps in release.
 *   <li><b>A null contract answered with a plausible default.</b> The JDK's
 *       null rules are asymmetric and specified: {@code ArrayDeque} rejects a
 *       null element with NPE but answers {@code contains(null)} with
 *       {@code false}; {@code TreeMap} with natural ordering throws NPE on a
 *       null key; {@code Vector} accepts null happily. See {@link #deque()},
 *       {@link #treenav()}, {@link #vector()}, {@link #props()}.
 *   <li><b>{@code Ok(None)} from a value-returning native.</b> The clean probe
 *       for this is a class whose API has BOTH shapes over the same state:
 *       {@code ArrayDeque.pollFirst()} must return {@code null} on empty and
 *       {@code removeFirst()} must throw {@code NoSuchElementException};
 *       {@code TreeMap.firstEntry()} must return {@code null} and
 *       {@code firstKey()} must throw. A native that returns {@code Ok(None)}
 *       for the whole family collapses the pair. See {@link #deque()},
 *       {@link #treenav()}.
 *   <li><b>A value read by SLOT INDEX with no type check.</b>
 *       {@code Properties.getProperty} is the JDK's own type check: the map is
 *       a {@code Hashtable<Object,Object>}, and {@code getProperty} must return
 *       {@code null} — not the value, not {@code toString()} — when the stored
 *       value is not a {@code String}. See {@link #props()}. The long/int slot
 *       packing of {@code AtomicLongArray.set(int,long)} is the other one; see
 *       {@link #atomarr()}.
 *   <li><b>Unsigned casts of signed arguments.</b> Negative counts, negative
 *       capacities, {@code Integer.MIN_VALUE} shifts, {@code getShort} of
 *       {@code 0xFFFE} (must be {@code -2}) beside {@code getChar} of the same
 *       bytes (must be {@code 0xFFFE}). See {@link #bytebuf()},
 *       {@link #bigint()}, {@link #sbidx()}.
 *   <li><b>Anything taking a String.</b> A Rust {@code str} cannot hold an
 *       unpaired UTF-16 surrogate, so any bridge that converts a Java String to
 *       a Rust String either rejects, replaces (U+FFFD), or silently reshapes
 *       it. {@link #surrog()} pushes a lone high surrogate through seven
 *       different bridge classes and asserts it comes back bit-identical.
 * </ol>
 *
 * <h2>How this file is driven</h2>
 *
 * <p><b>Run the families in SEPARATE processes first.</b> With no arguments the
 * eleven families run in ascending order of how likely each is to ABORT the VM
 * rather than fail an assertion, so a VM that dies in {@link #bigint()} has
 * already reported the other ten — but a VM that dies in {@link #props()} has
 * reported nothing. {@code --only=<family>} runs one family alone and is the
 * only way to learn anything about a family whose predecessor kills the
 * process; {@code --list} prints the family names.
 *
 * <p>Every call that passes an out-of-range index, a negative count, a null
 * where the JDK specifies a throw, or an unpaired surrogate prints a
 * {@code CK RJdkBridge1 <family>-step=<call>} breadcrumb BEFORE the call, so
 * the last line on stdout names the call that killed the VM.
 *
 * <p>Operands come out of the {@code OPAQUE_*} arrays rather than being written
 * as literals, so neither {@code javac} nor a JIT that reimplements a native as
 * a thin direct helper can answer from the folder instead of from the native.
 * There are no lambdas in this file: a weak {@code invokedynamic} must not
 * redden a family for the wrong reason.
 *
 * <p>Every expected value here was MEASURED on Microsoft OpenJDK 25.0.3+9
 * before it was written. None of them is remembered.
 *
 * <h2>Mode independence</h2>
 *
 * <p>These are JDK library semantics and the bridges are registered in both
 * arms, so this belongs in {@code CORE_CLASSES}.
 */
public class RJdkBridge1 {
    static int checks;

    static int mark;

    /** Sinks whose only purpose is to keep a call from being elided. */
    static int sink;

    static Object sinkO;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Close a block and assert its own size. The count is a tripwire: a block
     * that silently loses rows to an edit still prints {@code CK}, and a
     * hard-coded number that nobody re-derives is how a shrinking vector goes
     * unnoticed.
     */
    static void sectionEnd(String name, int expected) {
        int n = checks - mark;
        mark = checks;
        if (n != expected) {
            throw new AssertionError(
                    "block " + name + " ran " + n + " checks, header says " + expected);
        }
        System.out.println("CK RJdkBridge1 " + name + "=" + n);
    }

    /**
     * A progress marker printed BEFORE a call that may abort the VM instead of
     * throwing. On a VM that panics, the last of these on stdout names the call
     * that killed it.
     */
    static void step(String family, String what) {
        System.out.println("CK RJdkBridge1 " + family + "-step=" + what);
    }

    /** The class of the throwable an operation produced, or {@code "none"}. */
    static String nameOf(Throwable t) {
        return t == null ? "none" : t.getClass().getName();
    }

    // Operand sources neither javac nor a JIT can see through.
    static final int[] OPAQUE_I = {
        0, 1, 2, 3, 4, 5, 6, 8, 9, 99, -1, -2, -5, 0x110000, Integer.MIN_VALUE, 200, 40, 7, 10,
    };

    static final int I0 = 0;
    static final int I1 = 1;
    static final int I2 = 2;
    static final int I3 = 3;
    static final int I4 = 4;
    static final int I5 = 5;
    static final int I6 = 6;
    static final int I8 = 7;
    static final int I9 = 8;
    static final int I99 = 9;
    static final int IM1 = 10;
    static final int IM2 = 11;
    static final int IM5 = 12;
    static final int IBADCP = 13;
    static final int IMIN = 14;
    static final int I200 = 15;
    static final int I40 = 16;
    static final int I7 = 17;
    static final int I10 = 18;

    static final String[] OPAQUE_S = {
        "abcde", "abc", "x", "z", "a", "b", "c", "d", "e", "k", "v", "nope", "dflt", "", "bb",
        "ff",
    };

    static final int SABCDE = 0;
    static final int SABC = 1;
    static final int SX = 2;
    static final int SZ = 3;
    static final int SA = 4;
    static final int SB = 5;
    static final int SC = 6;
    static final int SD = 7;
    static final int SNOPE = 11;
    static final int SDFLT = 12;
    static final int SEMPTY = 13;
    static final int SBB = 14;

    /** "a<lone high surrogate>b" — built from chars, never from a literal. */
    static final String LONE_HI = new String(new char[] {'a', (char) 0xD800, 'b'});

    /** "a<lone LOW surrogate>b" — the other half of the hazard. */
    static final String LONE_LO = new String(new char[] {'a', (char) 0xDC00, 'b'});

    /** "a<U+10437 as a well-formed pair>b". */
    static final String PAIR = new String(new char[] {'a', (char) 0xD801, (char) 0xDC37, 'b'});

    static StringBuilder mk() {
        return new StringBuilder(OPAQUE_S[SABCDE]);
    }

    static TreeMap<String, String> mkTm() {
        TreeMap<String, String> m = new TreeMap<>();
        m.put(OPAQUE_S[SC], "3");
        m.put(OPAQUE_S[SA], "1");
        m.put(OPAQUE_S[SD], "4");
        m.put(OPAQUE_S[SB], "2");
        return m;
    }

    // ------------------------------------------------------------------
    // props — java/util/Properties (90 bridge rows, THREE registrations per
    // triple; the winner is whichever registers last). Hazard 4: getProperty
    // is the JDK's own type check on a Hashtable<Object,Object>.
    // ------------------------------------------------------------------
    static void props() {
        Properties p = new Properties();
        check(p.getProperty(OPAQUE_S[SNOPE]) == null,
                "Properties.getProperty of an absent key must be null");
        check(OPAQUE_S[SDFLT].equals(p.getProperty(OPAQUE_S[SNOPE], OPAQUE_S[SDFLT])),
                "Properties.getProperty(absent, default) must return the default");

        step("props", "getProperty(null)");
        Throwable t = null;
        try {
            sinkO = p.getProperty(null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Properties.getProperty(null) must throw NullPointerException, got " + nameOf(t));
        step("props", "getProperty(null, default)");
        t = null;
        try {
            sinkO = p.getProperty(null, OPAQUE_S[SDFLT]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Properties.getProperty(null, d) must throw NPE — the default must NOT swallow the"
                        + " null key; got " + nameOf(t));

        // The type check. p.put is Hashtable.put, so a non-String value is
        // legal to STORE and must be invisible to getProperty.
        Properties q = new Properties();
        q.put(OPAQUE_S[SK], Integer.valueOf(OPAQUE_I[I7]));
        check(q.getProperty(OPAQUE_S[SK]) == null,
                "Properties.getProperty must return null when the stored value is not a String —"
                        + " NOT the value and NOT its toString()");
        check(OPAQUE_S[SDFLT].equals(q.getProperty(OPAQUE_S[SK], OPAQUE_S[SDFLT])),
                "getProperty(k, d) must fall through to the default when the value is not a"
                        + " String");
        check(Integer.valueOf(OPAQUE_I[I7]).equals(q.get(OPAQUE_S[SK])),
                "Hashtable.get must still see the Integer that getProperty hid");
        Properties q2 = new Properties();
        q2.put(Integer.valueOf(OPAQUE_I[I3]), OPAQUE_S[SV]);
        check(q2.getProperty("3") == null,
                "a non-String KEY must not be found by getProperty(\"3\")");

        Properties r = new Properties();
        check(r.setProperty(OPAQUE_S[SK], "v1") == null,
                "setProperty must return the PREVIOUS value — null the first time");
        check("v1".equals(r.setProperty(OPAQUE_S[SK], "v2")),
                "setProperty must return the previous value on overwrite");
        check("v2".equals(r.getProperty(OPAQUE_S[SK])), "setProperty must have written");

        step("props", "setProperty(k, null)");
        t = null;
        try {
            sinkO = r.setProperty(OPAQUE_S[SK], null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Properties.setProperty(k, null) must throw NPE, got " + nameOf(t));
        step("props", "put(k, null)");
        t = null;
        try {
            sinkO = r.put(OPAQUE_S[SK], null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Properties.put(k, null) must throw NPE, got " + nameOf(t));

        // The defaults chain: a lookup that falls through must NOT make the key
        // a member of the receiver.
        Properties d = new Properties();
        d.setProperty(OPAQUE_S[SA], "da");
        d.setProperty(OPAQUE_S[SB], "db");
        Properties c = new Properties(d);
        c.setProperty(OPAQUE_S[SB], "qb");
        check("da".equals(c.getProperty(OPAQUE_S[SA])),
                "getProperty must fall through to the defaults table");
        check("qb".equals(c.getProperty(OPAQUE_S[SB])),
                "a key present in BOTH tables must resolve to the receiver's value");
        check(c.size() == 1, "size() must count the receiver only, not the defaults");
        check(!c.containsKey(OPAQUE_S[SA]),
                "containsKey must be false for a key that only the defaults table has");
        check(c.get(OPAQUE_S[SA]) == null,
                "Hashtable.get must NOT consult the defaults table");
        check("[a, b]".equals(sortedNames(c.propertyNames())),
                "propertyNames must union the defaults chain");

        Properties mix = new Properties();
        mix.setProperty("s", OPAQUE_S[SV]);
        mix.put("i", Integer.valueOf(OPAQUE_I[I1]));
        mix.put(Integer.valueOf(OPAQUE_I[I2]), OPAQUE_S[SX]);
        List<String> spn = new ArrayList<>(mix.stringPropertyNames());
        Collections.sort(spn);
        check("[s]".equals(spn.toString()),
                "stringPropertyNames must drop entries whose key OR value is not a String, got "
                        + spn);
        check(mix.size() == 3, "stringPropertyNames must not have changed the map's size");
        Properties mix2 = new Properties();
        mix2.setProperty("s", OPAQUE_S[SV]);
        mix2.put("i", Integer.valueOf(OPAQUE_I[I1]));
        check("[i, s]".equals(sortedNames(mix2.propertyNames())),
                "propertyNames keeps a non-String VALUE (only the key must be a String)");

        Properties lo = new Properties();
        try {
            lo.load(new StringReader("a=1\nb : 2\n c 3\n#cmt\n!c2\n\nd=\n"));
        } catch (Throwable x) {
            throw new AssertionError("Properties.load(Reader) threw " + nameOf(x));
        }
        check("1".equals(lo.getProperty(OPAQUE_S[SA])), "load: 'a=1'");
        check("2".equals(lo.getProperty(OPAQUE_S[SB])), "load: 'b : 2' — colon is a separator");
        check("3".equals(lo.getProperty(OPAQUE_S[SC])),
                "load: ' c 3' — a bare space is a separator and leading space is stripped");
        check(OPAQUE_S[SEMPTY].equals(lo.getProperty(OPAQUE_S[SD])),
                "load: 'd=' is the EMPTY string, not null");
        check(lo.size() == 4, "load: '#' and '!' lines and blank lines are not entries, got "
                + lo.size());

        Properties le = new Properties();
        try {
            le.load(new StringReader("k\\ 1=v\\tx\\n\\u0041\nm=a\\\n  b\n"));
        } catch (Throwable x) {
            throw new AssertionError("Properties.load(escapes) threw " + nameOf(x));
        }
        String esc = le.getProperty("k 1");
        check(esc != null && esc.length() == 5 && esc.charAt(0) == 'v' && esc.charAt(1) == '\t'
                        && esc.charAt(2) == 'x' && esc.charAt(3) == '\n' && esc.charAt(4) == 'A',
                "load must decode \\ in the key and \\t \\n \\uXXXX in the value");
        check("ab".equals(le.getProperty("m")),
                "load must join a backslash continuation and strip the next line's leading space");

        Properties ln = new Properties();
        try {
            ln.load(new StringReader("lonely\n"));
        } catch (Throwable x) {
            throw new AssertionError("Properties.load(no separator) threw " + nameOf(x));
        }
        check(OPAQUE_S[SEMPTY].equals(ln.getProperty("lonely")),
                "a key with no separator maps to the EMPTY string, not null");
        check(ln.size() == 1, "the separator-less line is still one entry");

        Properties ls = new Properties();
        try {
            ls.load(new ByteArrayInputStream(
                    "k=é\n".getBytes(StandardCharsets.ISO_8859_1)));
        } catch (Throwable x) {
            throw new AssertionError("Properties.load(InputStream) threw " + nameOf(x));
        }
        check(ls.getProperty(OPAQUE_S[SK]) != null
                        && ls.getProperty(OPAQUE_S[SK]).length() == 1
                        && ls.getProperty(OPAQUE_S[SK]).charAt(0) == 0x00e9,
                "load(InputStream) decodes ISO-8859-1, so byte 0xE9 is U+00E9");

        step("props", "load((InputStream) null)");
        t = null;
        try {
            new Properties().load((InputStream) null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Properties.load(null) must throw NullPointerException, got " + nameOf(t));

        Properties st = new Properties();
        st.setProperty("a b", "c=d");
        st.setProperty("u", "é");
        Properties back = new Properties();
        try {
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            st.store(bo, null);
            back.load(new ByteArrayInputStream(bo.toByteArray()));
        } catch (Throwable x) {
            throw new AssertionError("Properties store/load round trip threw " + nameOf(x));
        }
        check("c=d".equals(back.getProperty("a b")),
                "store must escape the space in a key and the '=' in a value so load reverses it");
        check(back.getProperty("u") != null && back.getProperty("u").length() == 1
                        && back.getProperty("u").charAt(0) == 0x00e9,
                "store/load must round-trip a non-ASCII value");
        check(back.size() == 2, "store/load round trip must not invent or drop entries");

        Properties cl = new Properties();
        cl.setProperty(OPAQUE_S[SA], "1");
        check("1".equals(cl.remove(OPAQUE_S[SA])), "Properties.remove returns the old value");
        check(cl.remove(OPAQUE_S[SA]) == null, "a second remove returns null");
        cl.setProperty(OPAQUE_S[SA], "1");
        cl.clear();
        check(cl.size() == 0 && cl.isEmpty() && cl.getProperty(OPAQUE_S[SA]) == null,
                "clear must empty the table");
        Properties ed = new Properties(d);
        check(ed.equals(new Properties()) && ed.isEmpty(),
                "equals/isEmpty are Hashtable's and must ignore the defaults table");

        sectionEnd("props", 40);
    }

    static final int SK = 9;

    static final int SV = 10;

    static String sortedNames(Enumeration<?> e) {
        List<String> n = new ArrayList<>();
        while (e.hasMoreElements()) {
            n.add(String.valueOf(e.nextElement()));
        }
        Collections.sort(n);
        return n.toString();
    }

    // ------------------------------------------------------------------
    // treenav — java/util/TreeMap (45 rows) + TreeSet (34). Hazards 2 and 3:
    // the null-key contract, and the throw/return-null pair over the same
    // state.
    // ------------------------------------------------------------------
    static void treenav() {
        TreeMap<String, String> empty = new TreeMap<>();
        step("treenav", "TreeMap().firstKey()");
        Throwable t = null;
        try {
            sinkO = empty.firstKey();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "TreeMap.firstKey() on empty must throw NoSuchElementException, got " + nameOf(t));
        step("treenav", "TreeMap().lastKey()");
        t = null;
        try {
            sinkO = empty.lastKey();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "TreeMap.lastKey() on empty must throw NoSuchElementException, got " + nameOf(t));
        check(empty.firstEntry() == null,
                "TreeMap.firstEntry() on empty must return NULL — the other half of the pair"
                        + " whose first half throws");
        check(empty.pollFirstEntry() == null,
                "TreeMap.pollFirstEntry() on empty must return null");

        step("treenav", "TreeMap.put(null, v)");
        t = null;
        try {
            sinkO = empty.put(null, OPAQUE_S[SV]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "TreeMap.put(null, v) under natural ordering must throw NPE, got " + nameOf(t));
        step("treenav", "TreeMap().get(null)");
        t = null;
        try {
            sinkO = empty.get(null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "TreeMap.get(null) must throw NPE even on an EMPTY map, got " + nameOf(t));

        TreeMap<String, String> one = new TreeMap<>();
        one.put(OPAQUE_S[SA], "1");
        step("treenav", "TreeMap.get(null) non-empty");
        t = null;
        try {
            sinkO = one.get(null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "TreeMap.get(null) must throw NPE, not answer null, got " + nameOf(t));
        step("treenav", "TreeMap.containsKey(null)");
        t = null;
        try {
            sink = one.containsKey(null) ? 1 : 0;
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "TreeMap.containsKey(null) must throw NPE, not answer false, got " + nameOf(t));
        step("treenav", "TreeMap.remove(null)");
        t = null;
        try {
            sinkO = one.remove(null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "TreeMap.remove(null) must throw NPE, got " + nameOf(t));

        step("treenav", "TreeMap.subMap(b, a)");
        t = null;
        try {
            sinkO = one.subMap(OPAQUE_S[SB], OPAQUE_S[SA]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "TreeMap.subMap(from > to) must throw IllegalArgumentException, got " + nameOf(t));
        check("{}".equals(one.subMap(OPAQUE_S[SA], OPAQUE_S[SA]).toString()),
                "subMap(k, k) is legal and empty");

        TreeMap<String, String> m = mkTm();
        check("{b=2, c=3, d=4}".equals(m.subMap(OPAQUE_S[SB], true, OPAQUE_S[SD], true).toString()),
                "subMap inclusive/inclusive");
        check("{c=3}".equals(m.subMap(OPAQUE_S[SB], false, OPAQUE_S[SD], false).toString()),
                "subMap exclusive/exclusive");
        check("{a=1, b=2}".equals(m.headMap(OPAQUE_S[SC]).toString()), "headMap is exclusive");
        check("{a=1, b=2, c=3}".equals(m.headMap(OPAQUE_S[SC], true).toString()),
                "headMap(k, true) is inclusive");
        check("{c=3, d=4}".equals(m.tailMap(OPAQUE_S[SC]).toString()), "tailMap is inclusive");
        check("{d=4}".equals(m.tailMap(OPAQUE_S[SC], false).toString()),
                "tailMap(k, false) is exclusive");
        check(OPAQUE_S[SC].equals(m.ceilingKey(OPAQUE_S[SBB])), "ceilingKey(bb) is c");
        check(OPAQUE_S[SB].equals(m.floorKey(OPAQUE_S[SBB])), "floorKey(bb) is b");
        check(OPAQUE_S[SC].equals(m.higherKey(OPAQUE_S[SB])), "higherKey(b) is c");
        check(OPAQUE_S[SA].equals(m.lowerKey(OPAQUE_S[SB])), "lowerKey(b) is a");
        check(m.lowerKey(OPAQUE_S[SA]) == null, "lowerKey(first) must be null");
        check(m.higherKey("e") == null, "higherKey(past last) must be null");
        check(m.comparator() == null, "a naturally ordered TreeMap has a NULL comparator");
        check("{d=4, c=3, b=2, a=1}".equals(m.descendingMap().toString()), "descendingMap order");
        check("[d, c, b, a]".equals(m.descendingKeySet().toString()), "descendingKeySet order");
        check("[a, b, c, d]".equals(m.navigableKeySet().toString()), "navigableKeySet order");
        check("{a=1, b=2, c=3, d=4}".equals(m.toString()), "TreeMap.toString is sorted");
        check(OPAQUE_S[SA].equals(m.firstKey()) && OPAQUE_S[SD].equals(m.lastKey()),
                "first/last key");
        check("a=1".equals(String.valueOf(m.firstEntry()))
                        && "d=4".equals(String.valueOf(m.lastEntry())),
                "first/last entry render as k=v");
        TreeMap<String, String> pm = mkTm();
        check("a=1".equals(String.valueOf(pm.pollFirstEntry()))
                        && "d=4".equals(String.valueOf(pm.pollLastEntry())) && pm.size() == 2,
                "pollFirst/pollLast must REMOVE");

        step("treenav", "subMap view put out of range");
        t = null;
        try {
            sinkO = m.subMap(OPAQUE_S[SB], OPAQUE_S[SD]).put(OPAQUE_S[SZ], "9");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "a write to a subMap view outside its range must throw IllegalArgumentException,"
                        + " got " + nameOf(t));

        TreeSet<String> es = new TreeSet<>();
        step("treenav", "TreeSet().first()");
        t = null;
        try {
            sinkO = es.first();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "TreeSet.first() on empty must throw NoSuchElementException, got " + nameOf(t));
        step("treenav", "TreeSet().last()");
        t = null;
        try {
            sinkO = es.last();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "TreeSet.last() on empty must throw NoSuchElementException, got " + nameOf(t));
        check(es.pollFirst() == null,
                "TreeSet.pollFirst() on empty must return null while first() throws");
        step("treenav", "TreeSet.add(null)");
        t = null;
        try {
            sink = es.add(null) ? 1 : 0;
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "TreeSet.add(null) under natural ordering must throw NPE, got " + nameOf(t));

        TreeSet<String> s = new TreeSet<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB], OPAQUE_S[SC], OPAQUE_S[SD]));
        check("[a, b]".equals(s.headSet(OPAQUE_S[SC]).toString()), "TreeSet.headSet is exclusive");
        check("[c, d]".equals(s.tailSet(OPAQUE_S[SC]).toString()), "TreeSet.tailSet is inclusive");
        check("[b, c]".equals(s.subSet(OPAQUE_S[SB], OPAQUE_S[SD]).toString()), "TreeSet.subSet");
        check(OPAQUE_S[SC].equals(s.ceiling(OPAQUE_S[SBB]))
                        && OPAQUE_S[SB].equals(s.floor(OPAQUE_S[SBB])),
                "TreeSet ceiling/floor");
        check("[d, c, b, a]".equals(s.descendingSet().toString()), "TreeSet.descendingSet");
        step("treenav", "TreeSet.subSet(b, a)");
        t = null;
        try {
            sinkO = s.subSet(OPAQUE_S[SB], OPAQUE_S[SA]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "TreeSet.subSet(from > to) must throw IllegalArgumentException, got " + nameOf(t));

        sectionEnd("treenav", 42);
    }

    // ------------------------------------------------------------------
    // collect — java/util/Collections (26 bridge rows). Hazards 1 and 5:
    // negative counts, and the immutable views' UnsupportedOperationException.
    // ------------------------------------------------------------------
    static void collect() {
        step("collect", "Collections.nCopies(-1, x)");
        Throwable t = null;
        try {
            sinkO = Collections.nCopies(OPAQUE_I[IM1], OPAQUE_S[SX]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "Collections.nCopies(-1, x) must throw IllegalArgumentException — NOT"
                        + " NegativeArraySize and NOT a capacity panic; got " + nameOf(t));
        check("[]".equals(Collections.nCopies(OPAQUE_I[I0], OPAQUE_S[SX]).toString()),
                "nCopies(0, x) is the empty list");
        check("[x, x, x]".equals(Collections.nCopies(OPAQUE_I[I3], OPAQUE_S[SX]).toString()),
                "nCopies(3, x) repeats");

        step("collect", "unmodifiableList().add");
        t = null;
        try {
            sink = Collections.unmodifiableList(new ArrayList<String>()).add(OPAQUE_S[SX]) ? 1 : 0;
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "unmodifiableList().add must throw UnsupportedOperationException, got "
                        + nameOf(t));
        step("collect", "unmodifiableList().set");
        t = null;
        try {
            List<String> u = Collections.unmodifiableList(
                    new ArrayList<>(Arrays.asList(OPAQUE_S[SA])));
            sinkO = u.set(OPAQUE_I[I0], OPAQUE_S[SB]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "unmodifiableList().set must throw UnsupportedOperationException, got "
                        + nameOf(t));
        step("collect", "emptyList().get(0)");
        t = null;
        try {
            sinkO = Collections.emptyList().get(OPAQUE_I[I0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "emptyList().get(0) must throw exactly IndexOutOfBoundsException, got "
                        + nameOf(t));
        step("collect", "singletonList().set");
        t = null;
        try {
            sinkO = Collections.singletonList(OPAQUE_S[SA]).set(OPAQUE_I[I0], OPAQUE_S[SB]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "singletonList().set must throw UnsupportedOperationException, got " + nameOf(t));
        step("collect", "singletonList().add");
        t = null;
        try {
            sink = Collections.singletonList(OPAQUE_S[SA]).add(OPAQUE_S[SB]) ? 1 : 0;
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "singletonList().add must throw UnsupportedOperationException, got " + nameOf(t));
        step("collect", "Collections.swap(list, 0, 5)");
        t = null;
        try {
            Collections.swap(new ArrayList<>(Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB])),
                    OPAQUE_I[I0], OPAQUE_I[I5]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "Collections.swap past the end must throw exactly IndexOutOfBoundsException, got "
                        + nameOf(t));

        List<String> l = new ArrayList<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB], OPAQUE_S[SC]));
        Collections.swap(l, OPAQUE_I[I0], OPAQUE_I[I2]);
        check("[c, b, a]".equals(l.toString()), "Collections.swap");
        List<String> l2 = new ArrayList<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB], OPAQUE_S[SC]));
        Collections.reverse(l2);
        check("[c, b, a]".equals(l2.toString()), "Collections.reverse");
        List<String> l3 = new ArrayList<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB], OPAQUE_S[SC], OPAQUE_S[SD]));
        step("collect", "Collections.rotate(list, -1)");
        Collections.rotate(l3, OPAQUE_I[IM1]);
        check("[b, c, d, a]".equals(l3.toString()),
                "Collections.rotate with a NEGATIVE distance rotates left, got " + l3);
        List<String> l4 = new ArrayList<>(Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB]));
        Collections.fill(l4, null);
        check("[null, null]".equals(l4.toString()), "Collections.fill accepts a null filler");
        check(Collections.binarySearch(Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SC], "e"),
                OPAQUE_S[SB]) == -2,
                "binarySearch miss must return -(insertion point) - 1 == -2");
        check(Collections.binarySearch(Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SC], "e"), "e") == 2,
                "binarySearch hit");
        check(Collections.frequency(Arrays.asList(OPAQUE_S[SA], null, null), null) == 2,
                "Collections.frequency must count NULLS, not throw on them");
        step("collect", "Collections.max(empty)");
        t = null;
        try {
            sinkO = Collections.max(new ArrayList<String>());
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "Collections.max of an empty collection must throw NoSuchElementException, got "
                        + nameOf(t));
        check(Collections.disjoint(Arrays.asList(OPAQUE_S[SA]), Arrays.asList(OPAQUE_S[SB])),
                "Collections.disjoint");
        check(Collections.emptyList() == Collections.emptyList(),
                "Collections.emptyList must be the SAME singleton object every call");

        sectionEnd("collect", 19);
    }

    // ------------------------------------------------------------------
    // deque — java/util/ArrayDeque (34 rows). Hazards 2 and 3: the
    // throw-vs-null pair, and null hostility that is NOT uniform.
    // ------------------------------------------------------------------
    static void deque() {
        // The seven that MUST throw.
        String[] names = {
            "removeFirst", "removeLast", "getFirst", "getLast", "element", "remove", "pop",
        };
        for (int k = 0; k < names.length; k++) {
            ArrayDeque<String> d = new ArrayDeque<>();
            step("deque", "ArrayDeque()." + names[k] + "()");
            Throwable t = null;
            try {
                if (k == 0) {
                    sinkO = d.removeFirst();
                } else if (k == 1) {
                    sinkO = d.removeLast();
                } else if (k == 2) {
                    sinkO = d.getFirst();
                } else if (k == 3) {
                    sinkO = d.getLast();
                } else if (k == 4) {
                    sinkO = d.element();
                } else if (k == 5) {
                    sinkO = d.remove();
                } else {
                    sinkO = d.pop();
                }
            } catch (Throwable x) {
                t = x;
            }
            check("java.util.NoSuchElementException".equals(nameOf(t)),
                    "ArrayDeque." + names[k]
                            + "() on empty must throw NoSuchElementException, got " + nameOf(t));
        }

        // The six that MUST return null over exactly the same state.
        ArrayDeque<String> e = new ArrayDeque<>();
        check(e.pollFirst() == null, "ArrayDeque.pollFirst() on empty must be null");
        check(e.pollLast() == null, "ArrayDeque.pollLast() on empty must be null");
        check(e.poll() == null, "ArrayDeque.poll() on empty must be null");
        check(e.peek() == null, "ArrayDeque.peek() on empty must be null");
        check(e.peekFirst() == null, "ArrayDeque.peekFirst() on empty must be null");
        check(e.peekLast() == null, "ArrayDeque.peekLast() on empty must be null");

        // Null hostility: seven inserters throw, four queries do not.
        String[] ins = {
            "addFirst", "addLast", "add", "offer", "offerFirst", "offerLast", "push",
        };
        for (int k = 0; k < ins.length; k++) {
            ArrayDeque<String> d = new ArrayDeque<>();
            step("deque", "ArrayDeque." + ins[k] + "(null)");
            Throwable t = null;
            try {
                if (k == 0) {
                    d.addFirst(null);
                } else if (k == 1) {
                    d.addLast(null);
                } else if (k == 2) {
                    sink = d.add(null) ? 1 : 0;
                } else if (k == 3) {
                    sink = d.offer(null) ? 1 : 0;
                } else if (k == 4) {
                    sink = d.offerFirst(null) ? 1 : 0;
                } else if (k == 5) {
                    sink = d.offerLast(null) ? 1 : 0;
                } else {
                    d.push(null);
                }
            } catch (Throwable x) {
                t = x;
            }
            check("java.lang.NullPointerException".equals(nameOf(t)),
                    "ArrayDeque." + ins[k] + "(null) must throw NullPointerException, got "
                            + nameOf(t));
        }
        step("deque", "ArrayDeque.contains(null) / remove(null) / removeXOccurrence(null)");
        check(!e.contains(null),
                "ArrayDeque.contains(null) must answer FALSE, not throw — the null rule is"
                        + " asymmetric");
        check(!e.remove(null), "ArrayDeque.remove(null) must answer false");
        check(!e.removeFirstOccurrence(null),
                "ArrayDeque.removeFirstOccurrence(null) must answer false");
        check(!e.removeLastOccurrence(null),
                "ArrayDeque.removeLastOccurrence(null) must answer false");

        step("deque", "new ArrayDeque(-1)");
        Throwable t = null;
        try {
            sink = new ArrayDeque<String>(OPAQUE_I[IM1]).size();
        } catch (Throwable x) {
            t = x;
        }
        check("none".equals(nameOf(t)) && sink == 0,
                "new ArrayDeque(-1) is LEGAL on HotSpot (numElements is a hint) and yields an"
                        + " empty deque; got " + nameOf(t));
        check(new ArrayDeque<String>(OPAQUE_I[I0]).size() == 0, "new ArrayDeque(0) is empty");
        step("deque", "ArrayDeque().iterator().next()");
        t = null;
        try {
            sinkO = new ArrayDeque<String>().iterator().next();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "an empty ArrayDeque iterator's next() must throw NoSuchElementException, got "
                        + nameOf(t));

        ArrayDeque<String> d = new ArrayDeque<>();
        d.addLast(OPAQUE_S[SB]);
        d.addFirst(OPAQUE_S[SA]);
        d.addLast(OPAQUE_S[SC]);
        d.push(OPAQUE_S[SZ]);
        check("[z, a, b, c]".equals(Arrays.toString(d.toArray())),
                "push must insert at the FRONT and toArray must be in head-to-tail order, got "
                        + Arrays.toString(d.toArray()));
        check(OPAQUE_S[SZ].equals(d.pop()), "pop takes from the front");
        check(OPAQUE_S[SC].equals(d.pollLast()), "pollLast takes from the back");
        check(d.size() == 2, "size after two removals");
        ArrayDeque<String> dd = new ArrayDeque<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB], OPAQUE_S[SA], OPAQUE_S[SC]));
        check(dd.removeFirstOccurrence(OPAQUE_S[SA]) && dd.removeLastOccurrence(OPAQUE_S[SA])
                        && "[b, c]".equals(Arrays.toString(dd.toArray())),
                "removeFirst/LastOccurrence must remove ONE element each, from opposite ends");
        ArrayDeque<String> ds = new ArrayDeque<>();
        ds.add(OPAQUE_S[SA]);
        ds.add(OPAQUE_S[SB]);
        check("[a, b]".equals(ds.toString()), "ArrayDeque.toString");

        sectionEnd("deque", 33);
    }

    // ------------------------------------------------------------------
    // vector — java/util/Vector (29 rows). Hazard 1, plus the legacy
    // accessors' exception classes, which are NOT the modern ones.
    // ------------------------------------------------------------------
    static void vector() {
        step("vector", "new Vector().elementAt(0)");
        Throwable t = null;
        try {
            sinkO = new Vector<String>().elementAt(OPAQUE_I[I0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.elementAt on empty must throw ArrayIndexOutOfBoundsException — the LEGACY"
                        + " class, not IndexOutOfBoundsException; got " + nameOf(t));
        step("vector", "new Vector().get(0)");
        t = null;
        try {
            sinkO = new Vector<String>().get(OPAQUE_I[I0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.get on empty must throw ArrayIndexOutOfBoundsException (Vector is the"
                        + " exception to List's IndexOutOfBoundsException), got " + nameOf(t));

        Vector<String> v1 = new Vector<>();
        v1.add(OPAQUE_S[SA]);
        step("vector", "Vector.get(-1)");
        t = null;
        try {
            sinkO = v1.get(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.get(-1) must throw ArrayIndexOutOfBoundsException, got " + nameOf(t));
        step("vector", "Vector.elementAt(-1)");
        t = null;
        try {
            sinkO = v1.elementAt(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.elementAt(-1) must throw ArrayIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("vector", "new Vector().firstElement()");
        t = null;
        try {
            sinkO = new Vector<String>().firstElement();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "Vector.firstElement on empty must throw NoSuchElementException — NOT an index"
                        + " exception; got " + nameOf(t));
        step("vector", "new Vector().lastElement()");
        t = null;
        try {
            sinkO = new Vector<String>().lastElement();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "Vector.lastElement on empty must throw NoSuchElementException, got " + nameOf(t));
        step("vector", "new Vector().set(0, x)");
        t = null;
        try {
            sinkO = new Vector<String>().set(OPAQUE_I[I0], OPAQUE_S[SX]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.set on empty must throw ArrayIndexOutOfBoundsException, got " + nameOf(t));
        step("vector", "new Vector().setElementAt(x, 0)");
        t = null;
        try {
            new Vector<String>().setElementAt(OPAQUE_S[SX], OPAQUE_I[I0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.setElementAt on empty must throw ArrayIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("vector", "new Vector().insertElementAt(x, 5)");
        t = null;
        try {
            new Vector<String>().insertElementAt(OPAQUE_S[SX], OPAQUE_I[I5]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.insertElementAt past the end must throw ArrayIndexOutOfBoundsException,"
                        + " got " + nameOf(t));
        step("vector", "new Vector().add(5, x)");
        t = null;
        try {
            new Vector<String>().add(OPAQUE_I[I5], OPAQUE_S[SX]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.add(5, x) on an empty vector must throw ArrayIndexOutOfBoundsException,"
                        + " got " + nameOf(t));
        step("vector", "Vector.removeElementAt(-1)");
        t = null;
        try {
            v1.removeElementAt(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.removeElementAt(-1) must throw ArrayIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("vector", "new Vector().remove(0)");
        t = null;
        try {
            sinkO = new Vector<String>().remove(OPAQUE_I[I0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "Vector.remove(0) on empty must throw ArrayIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("vector", "new Vector(-1)");
        t = null;
        try {
            sink = new Vector<String>(OPAQUE_I[IM1]).size();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "new Vector(-1) must throw IllegalArgumentException — NOT NegativeArraySize and"
                        + " NOT a capacity panic; got " + nameOf(t));

        Vector<String> v2 = new Vector<>();
        v2.add(OPAQUE_I[I0], OPAQUE_S[SX]);
        check("[x]".equals(v2.toString()), "Vector.add(0, x) at the end index is legal");
        check(new Vector<String>(OPAQUE_I[I0]).capacity() == 0, "new Vector(0).capacity() is 0");
        check(new Vector<String>().capacity() == 10, "new Vector().capacity() is 10");

        Vector<String> vn = new Vector<>();
        vn.add(null);
        check(vn.get(OPAQUE_I[I0]) == null && vn.contains(null)
                        && vn.indexOf(null) == 0 && vn.size() == 1,
                "Vector ACCEPTS null — unlike ArrayDeque — and must find it");
        Vector<String> vr = new Vector<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB], OPAQUE_S[SA]));
        check(vr.removeElement(OPAQUE_S[SA]) && !vr.removeElement(OPAQUE_S[SZ])
                        && "[b, a]".equals(vr.toString()),
                "removeElement removes the FIRST occurrence and answers false when absent");
        Vector<String> vi = new Vector<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB], OPAQUE_S[SA]));
        check(vi.indexOf(OPAQUE_S[SA]) == 0 && vi.lastIndexOf(OPAQUE_S[SA]) == 2
                        && vi.indexOf(OPAQUE_S[SZ]) == -1,
                "indexOf/lastIndexOf/absent");
        check("[a, b]".equals(new Vector<>(
                Arrays.asList(OPAQUE_S[SA], OPAQUE_S[SB])).toString()), "Vector.toString");

        sectionEnd("vector", 20);
    }

    // ------------------------------------------------------------------
    // uri — java/net/URI (29 rows). Hazard 6 (a Rust parser implementing
    // Rust's grammar) plus the -1/null "absent component" contract.
    // ------------------------------------------------------------------
    static void uri() {
        URI u;
        try {
            u = new URI("http://user@host:8080/p/q?x=1#f");
        } catch (Throwable x) {
            throw new AssertionError("new URI(full) threw " + nameOf(x));
        }
        check("http".equals(u.getScheme()), "URI.getScheme");
        check("user".equals(u.getUserInfo()), "URI.getUserInfo");
        check("host".equals(u.getHost()), "URI.getHost");
        check(u.getPort() == 8080, "URI.getPort");
        check("/p/q".equals(u.getPath()), "URI.getPath");
        check("x=1".equals(u.getQuery()), "URI.getQuery");
        check("f".equals(u.getFragment()), "URI.getFragment");
        check("user@host:8080".equals(u.getAuthority()), "URI.getAuthority");

        URI bare;
        try {
            bare = new URI("http://host/p");
        } catch (Throwable x) {
            throw new AssertionError("new URI(bare) threw " + nameOf(x));
        }
        check(bare.getPort() == -1,
                "an ABSENT port must be -1 — the specified sentinel, not 0 and not 80");
        check(bare.getUserInfo() == null, "an absent userInfo must be null, not the empty string");
        check(bare.getQuery() == null, "an absent query must be null");
        check(bare.getFragment() == null, "an absent fragment must be null");

        try {
            URI o = new URI("mailto:a@b.com");
            check(o.isOpaque(), "mailto: is opaque");
            check(o.isAbsolute(), "mailto: is absolute");
            check(o.getPath() == null, "an opaque URI has a NULL path");
            check(o.getHost() == null, "an opaque URI has a null host");
            check("a@b.com".equals(o.getSchemeSpecificPart()), "opaque scheme-specific part");
            URI rel = new URI("a/b");
            check(!rel.isAbsolute(), "a scheme-less URI is not absolute");
            check(rel.getScheme() == null, "a relative URI has a null scheme");
            check("a/b".equals(rel.getPath()), "a relative URI keeps its path");
            check(rel.getHost() == null, "a relative URI has a null host");
            URI empty = new URI(OPAQUE_S[SEMPTY]);
            check(OPAQUE_S[SEMPTY].equals(empty.getPath()) && !empty.isAbsolute(),
                    "the EMPTY string is a legal relative URI whose path is empty");
        } catch (Throwable x) {
            throw new AssertionError("URI shape checks threw " + nameOf(x));
        }

        step("uri", "new URI(\"http://ho st/\")");
        Throwable t = null;
        try {
            sinkO = new URI("http://ho st/");
        } catch (Throwable x) {
            t = x;
        }
        check("java.net.URISyntaxException".equals(nameOf(t)),
                "a space in the authority must be a URISyntaxException, got " + nameOf(t));
        step("uri", "new URI(\"http://[/\")");
        t = null;
        try {
            sinkO = new URI("http://[/");
        } catch (Throwable x) {
            t = x;
        }
        check("java.net.URISyntaxException".equals(nameOf(t)),
                "an unclosed IPv6 bracket must be a URISyntaxException, got " + nameOf(t));
        step("uri", "URI.create(\"http://ho st/\")");
        t = null;
        try {
            sinkO = URI.create("http://ho st/");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "URI.create wraps the syntax error in IllegalArgumentException — a DIFFERENT class"
                        + " from the constructor's; got " + nameOf(t));
        step("uri", "new URI((String) null)");
        t = null;
        try {
            sinkO = new URI(null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "new URI(null) must throw NullPointerException, got " + nameOf(t));

        try {
            check("http://h/a/c".equals(new URI("http://h/a/./b/../c").normalize().toString()),
                    "URI.normalize must collapse . and ..");
            check("a/c".equals(new URI("a/./b/../c").normalize().toString()),
                    "URI.normalize on a relative URI");
            check("http://h/c".equals(new URI("http://h/a/b").resolve("../c").toString()),
                    "URI.resolve(String)");
            check("http://z/q".equals(new URI("http://h/a/b").resolve("http://z/q").toString()),
                    "resolve of an ABSOLUTE reference replaces the base entirely");
            check("b/c".equals(new URI("http://h/a/")
                    .relativize(new URI("http://h/a/b/c")).toString()), "URI.relativize");
            check("http://z/a/b".equals(new URI("http://h/a/")
                            .relativize(new URI("http://z/a/b")).toString()),
                    "relativize across authorities must return the argument UNCHANGED");
            URI raw = new URI("http://h/a%20b?q=%41#f%42");
            check("/a b".equals(raw.getPath()), "getPath decodes %20");
            check("/a%20b".equals(raw.getRawPath()), "getRawPath does NOT decode");
            check("q=A".equals(raw.getQuery()), "getQuery decodes %41");
            check("q=%41".equals(raw.getRawQuery()), "getRawQuery does not decode");
            check("fB".equals(raw.getFragment()), "getFragment decodes");
            check("f%42".equals(raw.getRawFragment()), "getRawFragment does not decode");
            check(new URI("HTTP://Host/P").equals(new URI("http://host/P")),
                    "URI.equals is case-insensitive on scheme and host but NOT on path");
            check(Integer.signum(new URI("http://a").compareTo(new URI("http://b"))) == -1,
                    "URI.compareTo");
            check(new URI("http://h/p").hashCode() == new URI("http://h/p").hashCode(),
                    "equal URIs must have equal hashCodes");
            check("http://h:80/p?q#f".equals(new URI("http://h:80/p?q#f").toString()),
                    "URI.toString must reproduce the input, port 80 included");
            // The port grammar. java.net.URI's port is DIGITS ONLY; anything
            // else demotes the authority to registry-based, which makes
            // getPort() -1 AND getHost() null. Rust's str::parse::<i32>
            // accepts a leading '+' or '-', so a bridge that reaches for it
            // answers -5 and 80 here and keeps a non-null host.
            URI negp = new URI("http://h:-5/p");
            check(negp.getPort() == -1,
                    "':-5' is not a port — Java's grammar is digits only, so getPort is -1, got "
                            + negp.getPort());
            check(negp.getHost() == null,
                    "and the authority is therefore registry-based, so getHost must be NULL");
            check("h:-5".equals(negp.getAuthority()),
                    "the whole 'h:-5' is the authority");
            check(new URI("http://h:+80/p").getPort() == -1,
                    "a leading '+' is not a Java port either, though Rust's integer parser takes"
                            + " it");
            check(new URI("http://h:8x/p").getPort() == -1, "a non-digit port is absent");
            check(new URI("http://h:99999999999/p").getPort() == -1,
                    "a port that overflows int must be reported ABSENT (-1) — NOT wrapped and NOT"
                            + " a panic");
            check("http://h/p".equals(new URI("http://h/p").toURL().toString()), "URI.toURL");
        } catch (Throwable x) {
            throw new AssertionError("URI value checks threw " + nameOf(x));
        }
        step("uri", "new URI(\"a/b\").toURL()");
        t = null;
        try {
            sinkO = new URI("a/b").toURL();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "toURL on a RELATIVE URI must throw IllegalArgumentException, got " + nameOf(t));

        sectionEnd("uri", 50);
    }

    // ------------------------------------------------------------------
    // bytebuf — java/nio/ByteBuffer (80 rows). Hazards 1 and 5: bounds on
    // both the absolute and relative forms, and signed-vs-unsigned reads of
    // the same two bytes.
    // ------------------------------------------------------------------
    static void bytebuf() {
        step("bytebuf", "ByteBuffer.allocate(-1)");
        Throwable t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[IM1]).capacity();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "ByteBuffer.allocate(-1) must throw IllegalArgumentException — NOT"
                        + " NegativeArraySize; got " + nameOf(t));
        check(ByteBuffer.allocate(OPAQUE_I[I0]).capacity() == 0, "allocate(0) is legal");
        step("bytebuf", "ByteBuffer.wrap(byte[4], 2, 10)");
        t = null;
        try {
            sink = ByteBuffer.wrap(new byte[4], OPAQUE_I[I2], OPAQUE_I[I10]).remaining();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "wrap(arr, off, len) past the end must throw exactly IndexOutOfBoundsException,"
                        + " got " + nameOf(t));
        step("bytebuf", "ByteBuffer.wrap(byte[4], -1, 2)");
        t = null;
        try {
            sink = ByteBuffer.wrap(new byte[4], OPAQUE_I[IM1], OPAQUE_I[I2]).remaining();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "wrap with a negative offset must throw exactly IndexOutOfBoundsException, got "
                        + nameOf(t));
        ByteBuffer w = ByteBuffer.wrap(new byte[8], OPAQUE_I[I2], OPAQUE_I[I4]);
        check(w.position() == 2 && w.limit() == 6 && w.capacity() == 8 && w.remaining() == 4,
                "wrap(arr, 2, 4) sets position=2 limit=6 capacity=8");

        step("bytebuf", "ByteBuffer.allocate(4).get(-1)");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[I4]).get(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "absolute get(-1) must throw exactly IndexOutOfBoundsException, got " + nameOf(t));
        step("bytebuf", "ByteBuffer.allocate(4).get(4)");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[I4]).get(OPAQUE_I[I4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "absolute get(capacity) must throw exactly IndexOutOfBoundsException, got "
                        + nameOf(t));
        step("bytebuf", "ByteBuffer.allocate(4).getInt(1)");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[I4]).getInt(OPAQUE_I[I1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "getInt(1) on a 4-byte buffer straddles the end and must throw exactly"
                        + " IndexOutOfBoundsException, got " + nameOf(t));
        step("bytebuf", "relative get() past the limit");
        t = null;
        try {
            ByteBuffer b = ByteBuffer.allocate(OPAQUE_I[I1]);
            b.get();
            sink = b.get();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.BufferUnderflowException".equals(nameOf(t)),
                "the RELATIVE get() past the limit must throw BufferUnderflowException — a"
                        + " different class from the absolute form's; got " + nameOf(t));
        step("bytebuf", "relative put() past the limit");
        t = null;
        try {
            ByteBuffer b = ByteBuffer.allocate(OPAQUE_I[I2]);
            b.put(new byte[] {1, 2, 3});
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.BufferOverflowException".equals(nameOf(t)),
                "a bulk put larger than remaining must throw BufferOverflowException, got "
                        + nameOf(t));
        step("bytebuf", "position(5) on a 4-byte buffer");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[I4]).position(OPAQUE_I[I5]).position();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "position past the limit must throw IllegalArgumentException — NOT an index"
                        + " exception; got " + nameOf(t));
        step("bytebuf", "position(-1)");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[I4]).position(OPAQUE_I[IM1]).position();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "position(-1) must throw IllegalArgumentException, got " + nameOf(t));
        step("bytebuf", "limit(5) on a 4-byte buffer");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[I4]).limit(OPAQUE_I[I5]).limit();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "limit past the capacity must throw IllegalArgumentException, got " + nameOf(t));
        step("bytebuf", "reset() with no mark");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[I4]).reset().position();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.InvalidMarkException".equals(nameOf(t)),
                "reset() with no mark must throw InvalidMarkException, got " + nameOf(t));
        ByteBuffer mk = ByteBuffer.allocate(OPAQUE_I[I8]);
        mk.position(OPAQUE_I[I3]).mark();
        mk.position(OPAQUE_I[I6]);
        check(mk.reset().position() == 3, "mark/reset must restore the marked position");
        step("bytebuf", "limit() below the mark, then reset()");
        t = null;
        try {
            ByteBuffer b = ByteBuffer.allocate(OPAQUE_I[I8]);
            b.position(OPAQUE_I[I6]).mark();
            b.limit(OPAQUE_I[I2]);
            sink = b.reset().position();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.InvalidMarkException".equals(nameOf(t)),
                "a limit() below the mark must DISCARD the mark, so reset() throws"
                        + " InvalidMarkException, got " + nameOf(t));

        ByteBuffer sgn = ByteBuffer.wrap(new byte[] {(byte) 0xFF, (byte) 0xFE});
        check(sgn.getShort(OPAQUE_I[I0]) == -2,
                "getShort of FF FE must be the SIGNED -2, got " + sgn.getShort(0));
        check(sgn.getChar(OPAQUE_I[I0]) == 0xFFFE,
                "getChar of the same two bytes must be the UNSIGNED 0xFFFE, got "
                        + Integer.toHexString(sgn.getChar(0)));
        check(ByteBuffer.wrap(new byte[] {1, 2, 3, 4}).getInt(OPAQUE_I[I0]) == 0x01020304,
                "the default order is BIG endian");
        check(ByteBuffer.wrap(new byte[] {1, 2, 3, 4}).order(ByteOrder.LITTLE_ENDIAN)
                        .getInt(OPAQUE_I[I0]) == 0x04030201,
                "order(LITTLE_ENDIAN) must reverse the bytes of getInt");

        step("bytebuf", "asReadOnlyBuffer().put");
        t = null;
        try {
            ByteBuffer.allocate(OPAQUE_I[I4]).asReadOnlyBuffer().put(OPAQUE_I[I0], (byte) 1);
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "a put into a read-only buffer must throw ReadOnlyBufferException, got "
                        + nameOf(t));
        step("bytebuf", "asReadOnlyBuffer().array()");
        t = null;
        try {
            sinkO = ByteBuffer.allocate(OPAQUE_I[I4]).asReadOnlyBuffer().array();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "array() on a read-only buffer must throw ReadOnlyBufferException — it must NOT"
                        + " hand out the backing array; got " + nameOf(t));
        check(ByteBuffer.allocate(OPAQUE_I[I4]).hasArray(), "a heap buffer hasArray");
        check(ByteBuffer.wrap(new byte[8], OPAQUE_I[I2], OPAQUE_I[I4]).arrayOffset() == 0,
                "wrap(arr, off, len) has arrayOffset 0 — the offset is in the POSITION");

        ByteBuffer sl = ByteBuffer.allocate(OPAQUE_I[I8]);
        sl.position(OPAQUE_I[I2]);
        sl.limit(OPAQUE_I[I6]);
        ByteBuffer sliced = sl.slice();
        check(sliced.position() == 0 && sliced.limit() == 4 && sliced.capacity() == 4,
                "slice() rebases position to 0 and capacity to the old remaining");
        ByteBuffer cp = ByteBuffer.wrap(new byte[] {1, 2, 3, 4});
        cp.position(OPAQUE_I[I2]);
        cp.compact();
        check(cp.position() == 2 && cp.limit() == 4 && cp.get(0) == 3 && cp.get(1) == 4,
                "compact() moves the remaining bytes to the front and leaves position at their"
                        + " end");
        ByteBuffer dup = ByteBuffer.allocate(OPAQUE_I[I4]);
        ByteBuffer dup2 = dup.duplicate();
        dup2.position(OPAQUE_I[I2]);
        check(dup.position() == 0 && dup2.position() == 2,
                "duplicate() must have INDEPENDENT position");
        check(!ByteBuffer.wrap(new byte[] {1, 2}).equals(ByteBuffer.wrap(new byte[] {1, 3})),
                "ByteBuffer.equals compares content");
        check(Integer.signum(ByteBuffer.wrap(new byte[] {1, 2})
                .compareTo(ByteBuffer.wrap(new byte[] {1, 3}))) == -1, "ByteBuffer.compareTo");
        ByteBuffer fl = ByteBuffer.allocate(OPAQUE_I[I8]);
        fl.position(OPAQUE_I[I4]);
        fl.flip();
        check(fl.position() == 0 && fl.limit() == 4, "flip sets limit=position, position=0");
        fl.rewind();
        check(fl.position() == 0 && fl.limit() == 4, "rewind keeps the limit");
        fl.clear();
        check(fl.position() == 0 && fl.limit() == 8, "clear restores limit=capacity");

        sectionEnd("bytebuf", 32);
    }

    // ------------------------------------------------------------------
    // sbidx — java/lang/AbstractStringBuilder (65 rows) + StringBuilder (65)
    // + StringBuffer (65). Hazard 1: every index form driven out of range,
    // and the exception class differs between charAt (StringIndexOutOfBounds)
    // and getChars (plain IndexOutOfBounds).
    // ------------------------------------------------------------------
    static void sbidx() {
        String sioobe = "java.lang.StringIndexOutOfBoundsException";
        step("sbidx", "charAt(-1)");
        Throwable t = null;
        try {
            sink = mk().charAt(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)),
                "StringBuilder.charAt(-1) must throw StringIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("sbidx", "charAt(len)");
        t = null;
        try {
            sink = mk().charAt(OPAQUE_I[I5]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "charAt(length) must throw, got " + nameOf(t));
        step("sbidx", "deleteCharAt(len)");
        t = null;
        try {
            sinkO = mk().deleteCharAt(OPAQUE_I[I5]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "deleteCharAt(length) must throw, got " + nameOf(t));
        step("sbidx", "deleteCharAt(-1)");
        t = null;
        try {
            sinkO = mk().deleteCharAt(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "deleteCharAt(-1) must throw, got " + nameOf(t));
        step("sbidx", "delete(3, 1)");
        t = null;
        try {
            sinkO = mk().delete(OPAQUE_I[I3], OPAQUE_I[I1]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)),
                "delete(start > end) must throw StringIndexOutOfBoundsException, got " + nameOf(t));
        step("sbidx", "delete(-1, 2)");
        t = null;
        try {
            sinkO = mk().delete(OPAQUE_I[IM1], OPAQUE_I[I2]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "delete(-1, 2) must throw, got " + nameOf(t));
        check("a".equals(mk().delete(OPAQUE_I[I1], OPAQUE_I[I99]).toString()),
                "delete(1, 99) must CLAMP the end to length, not throw");
        check("abcde".equals(mk().delete(OPAQUE_I[I5], OPAQUE_I[I5]).toString()),
                "delete(len, len) is a legal no-op");
        step("sbidx", "setLength(-1)");
        t = null;
        try {
            StringBuilder s = mk();
            s.setLength(OPAQUE_I[IM1]);
            sink = s.length();
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)),
                "setLength(-1) must throw StringIndexOutOfBoundsException — NOT"
                        + " NegativeArraySize; got " + nameOf(t));
        StringBuilder grown = mk();
        grown.setLength(OPAQUE_I[I8]);
        check(grown.length() == 8 && grown.charAt(OPAQUE_I[I7]) == 0,
                "setLength(8) must pad with NUL characters");
        step("sbidx", "setCharAt(len)");
        t = null;
        try {
            mk().setCharAt(OPAQUE_I[I5], 'x');
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "setCharAt(length) must throw, got " + nameOf(t));
        step("sbidx", "setCharAt(-1)");
        t = null;
        try {
            mk().setCharAt(OPAQUE_I[IM1], 'x');
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "setCharAt(-1) must throw, got " + nameOf(t));
        step("sbidx", "insert(len+1, x)");
        t = null;
        try {
            sinkO = mk().insert(OPAQUE_I[I6], OPAQUE_S[SX]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "insert past the end must throw, got " + nameOf(t));
        step("sbidx", "insert(-1, x)");
        t = null;
        try {
            sinkO = mk().insert(OPAQUE_I[IM1], OPAQUE_S[SX]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "insert(-1, x) must throw, got " + nameOf(t));
        check("abcdex".equals(mk().insert(OPAQUE_I[I5], OPAQUE_S[SX]).toString()),
                "insert AT the length is legal and appends");
        check("abnullcde".equals(mk().insert(OPAQUE_I[I2], (String) null).toString()),
                "insert(i, (String) null) must insert the four characters \"null\"");
        check("abcdenull".equals(mk().append((String) null).toString()),
                "append((String) null) must append \"null\"");
        check("abcdenull".equals(mk().append((Object) null).toString()),
                "append((Object) null) must append \"null\"");
        check("abcdenull".equals(mk().append((CharSequence) null).toString()),
                "append((CharSequence) null) must append \"null\"");
        check("abcdenu".equals(mk().append((CharSequence) null, OPAQUE_I[I0], OPAQUE_I[I2])
                        .toString()),
                "append((CharSequence) null, 0, 2) must append the first two characters of"
                        + " \"null\"");
        step("sbidx", "substring(3, 1)");
        t = null;
        try {
            sinkO = mk().substring(OPAQUE_I[I3], OPAQUE_I[I1]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "substring(3, 1) must throw, got " + nameOf(t));
        step("sbidx", "substring(len+1)");
        t = null;
        try {
            sinkO = mk().substring(OPAQUE_I[I6]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "substring past the end must throw, got " + nameOf(t));
        check(OPAQUE_S[SEMPTY].equals(mk().substring(OPAQUE_I[I5])),
                "substring(length) is the empty string, not a throw");
        step("sbidx", "getChars into a too-small array");
        t = null;
        try {
            mk().getChars(OPAQUE_I[I0], OPAQUE_I[I3], new char[2], OPAQUE_I[I0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "getChars overflowing the destination must throw exactly"
                        + " IndexOutOfBoundsException — NOT the String subclass; got " + nameOf(t));
        step("sbidx", "getChars at a destination offset that overflows");
        t = null;
        try {
            mk().getChars(OPAQUE_I[I0], OPAQUE_I[I3], new char[5], OPAQUE_I[I3]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "getChars with dstBegin+len past the destination must throw exactly"
                        + " IndexOutOfBoundsException, got " + nameOf(t));
        step("sbidx", "codePointAt(len)");
        t = null;
        try {
            sink = mk().codePointAt(OPAQUE_I[I5]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "codePointAt(length) must throw, got " + nameOf(t));
        step("sbidx", "codePointBefore(0)");
        t = null;
        try {
            sink = mk().codePointBefore(OPAQUE_I[I0]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "codePointBefore(0) must throw, got " + nameOf(t));
        step("sbidx", "codePointCount(0, len+1)");
        t = null;
        try {
            sink = mk().codePointCount(OPAQUE_I[I0], OPAQUE_I[I6]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "codePointCount past the end must throw exactly IndexOutOfBoundsException, got "
                        + nameOf(t));
        step("sbidx", "appendCodePoint(-1)");
        t = null;
        try {
            sinkO = mk().appendCodePoint(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "appendCodePoint(-1) must throw IllegalArgumentException, got " + nameOf(t));
        step("sbidx", "appendCodePoint(0x110000)");
        t = null;
        try {
            sinkO = mk().appendCodePoint(OPAQUE_I[IBADCP]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "appendCodePoint(0x110000) must throw IllegalArgumentException, got " + nameOf(t));
        StringBuilder cp = new StringBuilder();
        cp.appendCodePoint(0x10437);
        check(cp.length() == 2 && cp.charAt(OPAQUE_I[I0]) == 0xD801
                        && cp.charAt(OPAQUE_I[I1]) == 0xDC37,
                "appendCodePoint of a supplementary code point must append the SURROGATE PAIR");
        step("sbidx", "appendCodePoint(0xD800)");
        StringBuilder lone = new StringBuilder();
        lone.appendCodePoint(0xD800);
        check(lone.length() == 1 && lone.charAt(OPAQUE_I[I0]) == 0xD800,
                "appendCodePoint of a LONE SURROGATE value is legal and appends one char");
        step("sbidx", "reverse() over a surrogate pair");
        String rev = new StringBuilder(PAIR).reverse().toString();
        check(rev.length() == 4 && rev.charAt(0) == 'b' && rev.charAt(1) == 0xD801
                        && rev.charAt(2) == 0xDC37 && rev.charAt(3) == 'a',
                "reverse() must keep a surrogate PAIR in order while reversing everything else");
        step("sbidx", "reverse() over a lone surrogate");
        String rev2 = new StringBuilder(LONE_HI).reverse().toString();
        check(rev2.length() == 3 && rev2.charAt(0) == 'b' && rev2.charAt(1) == 0xD800
                        && rev2.charAt(2) == 'a',
                "reverse() of an unpaired surrogate must still reverse the other characters");
        step("sbidx", "repeat(cs, -1)");
        t = null;
        try {
            sinkO = new StringBuilder(OPAQUE_S[SA]).repeat(OPAQUE_S[SB], OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "StringBuilder.repeat with a NEGATIVE count must throw IllegalArgumentException —"
                        + " NOT loop forever and NOT panic on an unsigned cast; got " + nameOf(t));
        check(OPAQUE_S[SA].equals(new StringBuilder(OPAQUE_S[SA])
                        .repeat("bc", OPAQUE_I[I0]).toString()),
                "repeat(cs, 0) appends nothing");
        check("abcbcbc".equals(new StringBuilder(OPAQUE_S[SA])
                        .repeat("bc", OPAQUE_I[I3]).toString()),
                "repeat(cs, 3)");
        check(new StringBuilder(OPAQUE_S[SA]).repeat(0x10437, OPAQUE_I[I2]).length() == 5,
                "repeat(codePoint, 2) appends TWO surrogate pairs, so length is 1 + 4");
        step("sbidx", "new StringBuilder(-1)");
        t = null;
        try {
            sink = new StringBuilder(OPAQUE_I[IM1]).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NegativeArraySizeException".equals(nameOf(t)),
                "new StringBuilder(-1) must throw NegativeArraySizeException — a DIFFERENT class"
                        + " from setLength(-1)'s; got " + nameOf(t));
        check(new StringBuilder(OPAQUE_I[I0]).capacity() == 0, "new StringBuilder(0).capacity()");
        step("sbidx", "ensureCapacity(-1)");
        StringBuilder ec = mk();
        ec.ensureCapacity(OPAQUE_I[IM1]);
        check(ec.length() == 5, "ensureCapacity(-1) is a silent no-op, not a throw");
        check(mk().indexOf(OPAQUE_S[SC], OPAQUE_I[I99]) == -1,
                "indexOf with fromIndex past the end is -1");
        check(mk().indexOf(OPAQUE_S[SC], OPAQUE_I[IM5]) == 2,
                "indexOf CLAMPS a negative fromIndex to 0 rather than throwing");
        check(mk().lastIndexOf(OPAQUE_S[SC], OPAQUE_I[I99]) == 2,
                "lastIndexOf clamps a too-large fromIndex");
        check(mk().lastIndexOf(OPAQUE_S[SC], OPAQUE_I[IM5]) == -1,
                "lastIndexOf with a negative fromIndex is -1");
        check(mk().indexOf(OPAQUE_S[SEMPTY], OPAQUE_I[I99]) == 5,
                "indexOf(\"\", 99) is the LENGTH, not -1 and not 99");
        step("sbidx", "replace(3, 1, z)");
        t = null;
        try {
            sinkO = mk().replace(OPAQUE_I[I3], OPAQUE_I[I1], OPAQUE_S[SZ]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "replace(start > end) must throw, got " + nameOf(t));
        check("az".equals(mk().replace(OPAQUE_I[I1], OPAQUE_I[I99], OPAQUE_S[SZ]).toString()),
                "replace clamps the end to length");
        check("abzcde".equals(mk().replace(OPAQUE_I[I2], OPAQUE_I[I2], OPAQUE_S[SZ]).toString()),
                "replace(i, i, s) is an insertion");
        StringBuilder tr = new StringBuilder(OPAQUE_I[I200]);
        tr.append("ab");
        tr.trimToSize();
        check(tr.capacity() == 2 && tr.length() == 2,
                "trimToSize must shrink the capacity to the length");

        // StringBuffer is a SEPARATE 65-row registration of the same shapes.
        step("sbidx", "StringBuffer.charAt(9)");
        t = null;
        try {
            sink = new StringBuffer(OPAQUE_S[SABC]).charAt(OPAQUE_I[I9]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)), "StringBuffer.charAt(9) must throw, got " + nameOf(t));
        step("sbidx", "StringBuffer.deleteCharAt(9)");
        t = null;
        try {
            sinkO = new StringBuffer(OPAQUE_S[SABC]).deleteCharAt(OPAQUE_I[I9]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)),
                "StringBuffer.deleteCharAt(9) must throw, got " + nameOf(t));
        step("sbidx", "StringBuffer.setLength(-2)");
        t = null;
        try {
            StringBuffer s = new StringBuffer(OPAQUE_S[SABC]);
            s.setLength(OPAQUE_I[IM2]);
            sink = s.length();
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)),
                "StringBuffer.setLength(-2) must throw StringIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("sbidx", "StringBuffer.insert(4, z)");
        t = null;
        try {
            sinkO = new StringBuffer(OPAQUE_S[SABC]).insert(OPAQUE_I[I4], OPAQUE_S[SZ]);
        } catch (Throwable x) {
            t = x;
        }
        check(sioobe.equals(nameOf(t)),
                "StringBuffer.insert past the end must throw, got " + nameOf(t));
        String brev = new StringBuffer(PAIR).reverse().toString();
        check(brev.length() == 4 && brev.charAt(1) == 0xD801 && brev.charAt(2) == 0xDC37,
                "StringBuffer.reverse must keep a surrogate pair in order, exactly like"
                        + " StringBuilder's");

        sectionEnd("sbidx", 55);
    }

    // ------------------------------------------------------------------
    // surrog — hazard 6, across EIGHT bridge classes. A Rust `str` cannot
    // hold an unpaired UTF-16 surrogate, so any bridge that round-trips a
    // Java String through one either rejects it, replaces it with U+FFFD, or
    // reshapes it. Every assertion here is on the CHAR VALUES, never on a
    // printed form.
    // ------------------------------------------------------------------
    /**
     * Assert that a rendered collection string carries the lone surrogate of
     * {@link #LONE_HI} at {@code idx}, rather than the U+FFFD a lossy
     * round-trip through host text leaves behind.
     *
     * <p>{@code idx == 100} means "find it anywhere": a {@code keySet()} view
     * of a one-entry map renders the KEY, and this family reuses one map whose
     * key is ordinary text -- so that row asserts the absence of U+FFFD rather
     * than a position. Every other row knows exactly where the unit belongs.
     */
    static void ckCarries(String what, String rendered, int idx) {
        if (idx == 100) {
            check(rendered.indexOf('\uFFFD') < 0,
                    what + " must not contain U+FFFD, got: " + rendered);
            return;
        }
        check(rendered.length() > idx && rendered.charAt(idx) == 0xD800,
                what + " must carry the lone surrogate at " + idx + ", got charAt(" + idx + ")="
                        + (rendered.length() > idx
                                ? Integer.toHexString(rendered.charAt(idx))
                                : "<short:" + rendered.length() + ">")
                        + " in: " + rendered);
    }

    static void surrog() {
        step("surrog", "StringBuilder.append(lone high surrogate)");
        StringBuilder s = new StringBuilder();
        s.append(LONE_HI);
        check(s.length() == 3 && s.charAt(0) == 'a' && s.charAt(1) == 0xD800
                        && s.charAt(2) == 'b',
                "StringBuilder.append must carry an UNPAIRED high surrogate through unchanged");
        step("surrog", "StringBuilder.insert(lone)");
        String ins = new StringBuilder("xy").insert(OPAQUE_I[I1], LONE_HI).toString();
        check(ins.length() == 5 && ins.charAt(0) == 'x' && ins.charAt(2) == 0xD800
                        && ins.charAt(4) == 'y',
                "StringBuilder.insert must carry an unpaired surrogate through unchanged");
        step("surrog", "StringBuilder.indexOf(lone)");
        check(new StringBuilder("q" + LONE_HI).indexOf(new String(new char[] {(char) 0xD800}))
                        == 2,
                "indexOf must FIND a lone surrogate at its char index");
        step("surrog", "StringBuilder.codePointAt(lone)");
        check(new StringBuilder(LONE_HI).codePointAt(OPAQUE_I[I1]) == 0xD800,
                "codePointAt on an unpaired high surrogate must return the SURROGATE VALUE, not"
                        + " a combined code point and not U+FFFD");
        check(new StringBuilder(LONE_HI).codePointCount(OPAQUE_I[I0], OPAQUE_I[I3]) == 3,
                "an unpaired surrogate counts as its OWN code point, so \"a?b\" is 3");
        check(new StringBuilder(PAIR).codePointCount(OPAQUE_I[I0], OPAQUE_I[I4]) == 3,
                "a well-formed pair counts as ONE code point, so the 4-char PAIR is 3");
        step("surrog", "StringBuilder.reverse(lone LOW surrogate)");
        String rl = new StringBuilder(LONE_LO).reverse().toString();
        check(rl.length() == 3 && rl.charAt(0) == 'b' && rl.charAt(1) == 0xDC00
                        && rl.charAt(2) == 'a',
                "reverse must handle a lone LOW surrogate too");
        step("surrog", "StringBuilder.substring(lone)");
        String sub = new StringBuilder(LONE_HI).substring(OPAQUE_I[I1], OPAQUE_I[I2]);
        check(sub.length() == 1 && sub.charAt(0) == 0xD800,
                "substring must be able to return a STRING THAT IS ONE UNPAIRED SURROGATE");
        step("surrog", "StringBuilder.setCharAt(lone)");
        StringBuilder sc = new StringBuilder(OPAQUE_S[SABC]);
        sc.setCharAt(OPAQUE_I[I1], (char) 0xDBFF);
        check(sc.charAt(1) == 0xDBFF && sc.length() == 3,
                "setCharAt must be able to WRITE an unpaired surrogate");

        step("surrog", "Properties.setProperty(lone key, lone value)");
        Properties p = new Properties();
        p.setProperty("k" + LONE_HI, "v" + LONE_LO);
        String pv = p.getProperty("k" + LONE_HI);
        check(pv != null, "a key containing an unpaired surrogate must be FOUND again");
        check(pv != null && pv.length() == 4 && pv.charAt(2) == 0xDC00,
                "the value must come back with its unpaired low surrogate intact");

        step("surrog", "TreeMap with a lone surrogate key");
        TreeMap<String, String> m = new TreeMap<>();
        m.put(new String(new char[] {(char) 0xD800}), "hi");
        m.put(new String(new char[] {(char) 0xFFFF}), OPAQUE_S[SFF]);
        m.put("A", OPAQUE_S[SA]);
        check(m.size() == 3, "three distinct keys");
        check("A".equals(m.firstKey()), "'A' sorts first");
        check(m.lastKey().length() == 1 && m.lastKey().charAt(0) == 0xFFFF,
                "U+FFFF sorts AFTER U+D800 — String.compareTo is by UTF-16 CODE UNIT, so a"
                        + " surrogate is NOT treated as a supplementary code point");
        check(Integer.signum(new String(new char[] {(char) 0xD800})
                        .compareTo(new String(new char[] {(char) 0xFFFF}))) == -1,
                "the same rule stated directly on compareTo");

        step("surrog", "ArrayDeque holding a lone surrogate string");
        ArrayDeque<String> d = new ArrayDeque<>();
        d.add(LONE_HI);
        check(d.contains(LONE_HI), "ArrayDeque.contains must match an equal surrogate string");
        check(d.peek() != null && d.peek().length() == 3 && d.peek().charAt(1) == 0xD800,
                "ArrayDeque must hand the string back unchanged");
        step("surrog", "Vector holding a lone surrogate string");
        Vector<String> v = new Vector<>();
        v.add(LONE_HI);
        check(v.indexOf(LONE_HI) == 0 && v.contains(new String(new char[] {'a', (char) 0xD800,
                'b'})),
                "Vector.indexOf/contains must match by VALUE, including the surrogate");

        step("surrog", "new URI with a lone surrogate in the path");
        Throwable t = null;
        String us = null;
        try {
            us = new URI("http://h/" + LONE_HI).toString();
        } catch (Throwable x) {
            t = x;
        }
        check("none".equals(nameOf(t)),
                "a lone surrogate in a URI path is accepted by HotSpot, got " + nameOf(t));
        check(us != null && us.length() == 12 && us.charAt(10) == 0xD800,
                "URI.toString must return the path with the unpaired surrogate intact");

        step("surrog", "new BigInteger with a lone surrogate in the digits");
        t = null;
        try {
            sinkO = new BigInteger("1" + new String(new char[] {(char) 0xD800}) + "2");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "a surrogate among the digits must be a NumberFormatException — a Java throwable,"
                        + " NOT a Rust str conversion failure; got " + nameOf(t));

        step("surrog", "new String(char[]) with a lone surrogate");
        String ns = new String(new char[] {'a', (char) 0xD800, 'b'});
        check(ns.length() == 3 && ns.charAt(1) == 0xD800,
                "new String(char[]) must not sanitise an unpaired surrogate");

        // intern() is the one ACC_NATIVE method left in java.lang.String, and
        // the obvious implementation routes the content through a pool keyed
        // by host-language text -- which cannot hold an unpaired surrogate.
        // Two rows, because the first alone can be passed by returning the
        // receiver, and that silently breaks the contract the second asserts.
        step("surrog", "String.intern() with a lone surrogate");
        String is1 = new String(new char[] {'a', (char) 0xD800, 'b'}).intern();
        check(is1.length() == 3 && is1.charAt(1) == 0xD800,
                "intern() must answer the same TEXT, surrogate intact, got charAt(1)="
                        + Integer.toHexString(is1.charAt(1)));

        step("surrog", "String.intern() identity across two equal lone-surrogate strings");
        String is2 = new String(new char[] {'a', (char) 0xD800, 'b'}).intern();
        check(is1 == is2,
                "s.equals(t) must imply s.intern() == t.intern(), including when the text "
                        + "is not representable in the host language");

        // String.valueOf(Object) is `obj.toString()` in the JDK, so for a
        // String it is an IDENTITY. A bridge that decodes and rebuilds passes
        // nothing here — neither the units nor the identity.
        step("surrog", "String.valueOf(Object) with a lone surrogate");
        String vs = String.valueOf((Object) LONE_HI);
        check(vs.length() == 3 && vs.charAt(1) == 0xD800,
                "String.valueOf(Object) must carry the surrogate, got charAt(1)="
                        + Integer.toHexString(vs.charAt(1)));
        check(vs == LONE_HI,
                "String.valueOf(Object) on a String is obj.toString(), which for String is "
                        + "`this` — it must not allocate a copy");

        // The no-match case is the discriminating one: a bridge that rebuilds
        // the receiver loses the surrogate even when it replaces nothing.
        step("surrog", "String.replace(CharSequence,CharSequence) that matches nothing");
        String rp = LONE_HI.replace("q", "z");
        check(rp.length() == 3 && rp.charAt(1) == 0xD800,
                "replace(CharSequence,CharSequence) must not disturb a receiver it does not "
                        + "match, got charAt(1)=" + Integer.toHexString(rp.charAt(1)));
        step("surrog", "String.replace(CharSequence,CharSequence) that matches");
        String rp2 = LONE_HI.replace("a", "z");
        check(rp2.length() == 3 && rp2.charAt(0) == 'z' && rp2.charAt(1) == 0xD800,
                "replace(CharSequence,CharSequence) must replace the match and keep the rest");

        step("surrog", "String.join with a lone surrogate element");
        String js = String.join("-", LONE_HI);
        check(js.length() == 3 && js.charAt(1) == 0xD800,
                "String.join must carry an element's surrogate, got charAt(1)="
                        + Integer.toHexString(js.charAt(1)));

        // Normalizer runs its input through a host-language normalizer whose
        // input type cannot hold a lone surrogate at all. The mixed row is the
        // one that matters: a decomposed e-acute on EACH side of the lone unit
        // must compose independently, which is only true if the lone unit is
        // treated as a run boundary rather than as text.
        step("surrog", "Normalizer.normalize with a lone surrogate");
        String nz = Normalizer.normalize(LONE_HI, Normalizer.Form.NFC);
        check(nz.length() == 3 && nz.charAt(1) == 0xD800,
                "normalize must pass an unpaired surrogate through, got charAt(1)="
                        + Integer.toHexString(nz.charAt(1)));
        check(Normalizer.isNormalized(LONE_HI, Normalizer.Form.NFC),
                "a lone surrogate is unassigned and composes with nothing, so it IS normalized");

        step("surrog", "Normalizer composes the runs either side of a lone surrogate");
        String mixed = new String(new char[] {'e', 0x0301, (char) 0xD800, 'e', 0x0301});
        String mz = Normalizer.normalize(mixed, Normalizer.Form.NFC);
        check(mz.length() == 3 && mz.charAt(0) == 0x00E9 && mz.charAt(1) == 0xD800
                        && mz.charAt(2) == 0x00E9,
                "both runs must compose to U+00E9 with the surrogate intact between them");
        check(!Normalizer.isNormalized(mixed, Normalizer.Form.NFC),
                "isNormalized must answer for the runs, not skip them");

        // CharBuffer is the ninth bridge class in this family. wrap(char[])
        // hands back a HeapCharBuffer whose toString had the units in hand and
        // lost them in the final conversion.
        step("surrog", "CharBuffer.wrap(char[]).toString() with a lone surrogate");
        String cbs = CharBuffer.wrap(new char[] {'a', (char) 0xD800, 'b'}).toString();
        check(cbs.length() == 3 && cbs.charAt(1) == 0xD800,
                "CharBuffer.wrap(char[]).toString() must carry the surrogate, got charAt(1)="
                        + Integer.toHexString(cbs.charAt(1)));

        // -- every COLLECTION renderer, which is one defect in seventeen places
        // Each of these bottoms out in `obj_to_display_units` in
        // native-collections. It used to return a Rust `String`, which cannot
        // hold an unpaired surrogate, so all seventeen substituted U+FFFD.
        // `G63-1` measured three of them and refused to fix only those three;
        // these rows are the other fourteen, so a partial conversion cannot
        // pass. 21 of 29 probe rows diverged before the fix.
        step("surrog", "every collection toString with a lone surrogate");
        List<String> csList = new ArrayList<>();
        csList.add(LONE_HI);
        ckCarries("ArrayList.toString", csList.toString(), 2);
        ckCarries("Arrays.asList().toString", Arrays.asList(LONE_HI, "z").toString(), 2);
        ckCarries("Arrays.toString(Object[])", Arrays.toString(new String[] {LONE_HI}), 2);

        Map<String, String> csMap = new HashMap<>();
        csMap.put("k", LONE_HI);
        ckCarries("HashMap.toString value", csMap.toString(), 4);
        ckCarries("HashMap.toString key", new HashMap<>(Map.of(LONE_HI, "v")).toString(), 2);
        ckCarries("keySet() view toString", csMap.keySet().toString(), 100);
        ckCarries("values() view toString", csMap.values().toString(), 2);

        Map<String, String> csLhm = new LinkedHashMap<>();
        csLhm.put("k", LONE_HI);
        ckCarries("LinkedHashMap.toString", csLhm.toString(), 4);

        LinkedList<String> csLl = new LinkedList<>();
        csLl.add(LONE_HI);
        ckCarries("LinkedList.toString", csLl.toString(), 2);
        ArrayDeque<String> csAd = new ArrayDeque<>();
        csAd.add(LONE_HI);
        ckCarries("ArrayDeque.toString", csAd.toString(), 2);
        PriorityQueue<String> csPq = new PriorityQueue<>();
        csPq.add(LONE_HI);
        ckCarries("PriorityQueue.toString", csPq.toString(), 2);

        TreeMap<String, String> csTm = new TreeMap<>();
        csTm.put("k", LONE_HI);
        ckCarries("TreeMap.toString", csTm.toString(), 4);
        TreeSet<String> csTs = new TreeSet<>();
        csTs.add(LONE_HI);
        ckCarries("TreeSet.toString", csTs.toString(), 2);

        Map<String, String> csChm = new ConcurrentHashMap<>();
        csChm.put("k", LONE_HI);
        ckCarries("ConcurrentHashMap.toString", csChm.toString(), 4);
        Set<String> csKsv = ConcurrentHashMap.newKeySet();
        csKsv.add(LONE_HI);
        ckCarries("ConcurrentHashMap.KeySetView.toString", csKsv.toString(), 2);

        ckCarries("Optional.toString", Optional.of(LONE_HI).toString(), 10);

        // A nested collection reaches the element's OWN toString() through
        // virtual dispatch -- a different arm of the same function.
        List<List<String>> csNested = new ArrayList<>();
        csNested.add(csList);
        ckCarries("nested List.toString dispatches the element's toString", csNested.toString(), 3);

        // A boxed Character CAN itself be a lone surrogate. This one did not
        // print U+FFFD -- it printed '?', from a `char::from_u32().unwrap_or`
        // that cannot represent the value at all.
        List<Character> csChars = new ArrayList<>();
        csChars.add((char) 0xD800);
        String cs = csChars.toString();
        check(cs.length() == 3 && cs.charAt(1) == 0xD800,
                "a boxed Character holding a lone surrogate must print as that unit, got charAt(1)="
                        + Integer.toHexString(cs.charAt(1)));

        // Controls: ordinary text, a REAL surrogate pair, and numbers must all
        // be untouched by a units-level renderer.
        check("[ab, cd]".equals(new ArrayList<>(List.of("ab", "cd")).toString()),
                "ordinary element text must be unchanged");
        check("[1, 2.5, 3]".equals(new ArrayList<>(List.of(1, 2.5, 3L)).toString()),
                "boxed numbers must still render Java-style, incl. the trailing .0");
        String pair = new ArrayList<>(List.of("\uD83D\uDE00")).toString();
        check(pair.length() == 4 && pair.charAt(1) == 0xD83D && pair.charAt(2) == 0xDE00,
                "a WELL-FORMED surrogate pair must survive as two units, not be reshaped");
        check("Optional.empty".equals(Optional.empty().toString()),
                "Optional.empty has no element to render");

        // -- the three routes that reach text WITHOUT going through a String --
        // Sweep 8 asked the append/insert siblings and the surfaces G63-1 N4
        // named: 38 of 42 rows were already exact, including every
        // StringBuilder/StringBuffer overload, Base64, URLEncoder, Collator,
        // MessageDigest, java.time and chars()/codePoints(). These are the
        // three that were not, and they share a shape: the value never IS a
        // String, so the units path that carries a String argument was
        // never on.
        step("surrog", "boxed Character, string concat and %s");

        // A boxed Character can itself BE an unpaired surrogate. This one
        // rendered '?', not even U+FFFD -- a Rust char cannot hold the value at
        // all, and the fallback substituted a character that means something
        // else.
        Character boxedLone = Character.valueOf((char) 0xD800);
        String cat = "" + boxedLone;
        check(cat.length() == 1 && cat.charAt(0) == 0xD800,
                "string concat of a boxed Character must carry the unit, got "
                        + Integer.toHexString(cat.length() == 1 ? cat.charAt(0) : -1));

        // %s is String.valueOf(arg) == arg.toString(). A STRING argument was
        // already exact; every other object went through a renderer that
        // returns host text.
        String fs = String.format("%s", boxedLone);
        check(fs.length() == 1 && fs.charAt(0) == 0xD800,
                "%s of a boxed Character must carry the unit, got "
                        + Integer.toHexString(fs.length() == 1 ? fs.charAt(0) : -1));
        String fo = String.format("%s", new Object() {
            @Override
            public String toString() {
                return LONE_HI;
            }
        });
        ckCarries("%s of an object whose toString carries one", fo, 1);

        // CharBuffer.wrap(CharSequence) STORES what it reads, so a lossy read
        // meant it could not give back the sequence it was handed. G63-1 N2
        // measured the char[] overload and left this one open.
        String cbw = CharBuffer.wrap((CharSequence) LONE_HI).toString();
        ckCarries("CharBuffer.wrap(CharSequence).toString()", cbw, 1);

        // Controls for the same three routes.
        check("Z".equals("" + Character.valueOf('Z')),
                "ordinary boxed Character concat must be unchanged");
        check("ok|q".equals(String.format("%s|%c", "ok", 'q')),
                "ordinary %s and %c must be unchanged");
        String pairWrap = CharBuffer.wrap((CharSequence) "\uD83D\uDE00").toString();
        check(pairWrap.length() == 2 && pairWrap.charAt(0) == 0xD83D,
                "a well-formed pair through CharBuffer.wrap must stay two units");

        // -- URL.toString rebuilds the external form, and rebuilt it as text --
        // Under --jdk-only the real JDK constructor fills the component fields,
        // so getPath()/getFile() were always exact; only this reconstruction
        // goes through us, and it assembled a Rust String. G75-1 N2 — a second,
        // narrower defect than the URI parse (N1), found by asking why the two
        // disagreed rather than assuming one cause.
        step("surrog", "URL.toString/toExternalForm with a lone surrogate");
        URL lurl;
        URL plain;
        try {
            lurl = new URL("http://h.example/" + LONE_HI);
            plain = new URL("http://h.example/ok");
        } catch (Exception e) {
            throw new AssertionError("URL construction must succeed: " + e);
        }
        ckCarries("URL.toString()", lurl.toString(), 18);
        ckCarries("URL.toExternalForm()", lurl.toExternalForm(), 18);
        // The two that were ALREADY right, kept as the rows that locate the
        // defect: components exact, reconstruction not.
        ckCarries("URL.getPath() was already exact", lurl.getPath(), 2);
        ckCarries("URL.getFile() was already exact", lurl.getFile(), 2);
        check("http://h.example/ok".equals(plain.toString()),
                "an ordinary URL must round-trip unchanged");

        // -- every URI ACCESSOR re-parses the raw text ------------------------
        // G75-1 N1. The loss was never in url_parse: each getter reads the raw
        // string and re-parses it, so a read_string in the getter re-lost the
        // unit whatever the parse had done. Converted to units end to end, with
        // ONE splitter — the &str spellings are wrappers over it now.
        step("surrog", "URI accessors with a lone surrogate");
        URI lu;
        URI lu2;
        try {
            lu = new URI("http", "h.example", "/" + LONE_HI, LONE_HI, LONE_HI);
            lu2 = new URI("http://h.example/" + LONE_HI);
        } catch (URISyntaxException e) {
            throw new AssertionError("URI construction must succeed: " + e);
        }
        ckCarries("URI.getPath()", lu.getPath(), 2);
        ckCarries("URI.getRawPath()", lu.getRawPath(), 2);
        ckCarries("URI.getQuery()", lu.getQuery(), 1);
        ckCarries("URI.getRawQuery()", lu.getRawQuery(), 1);
        ckCarries("URI.getFragment()", lu.getFragment(), 1);
        ckCarries("URI.getSchemeSpecificPart()", lu.getSchemeSpecificPart(), 100);
        ckCarries("URI.getRawSchemeSpecificPart()", lu.getRawSchemeSpecificPart(), 100);
        // The SINGLE-string constructor took a different route: its component
        // FIELDS won over the parse, so fixing the getters alone left it wrong.
        ckCarries("URI(String).getPath()", lu2.getPath(), 2);
        ckCarries("URI(String).getRawPath()", lu2.getRawPath(), 2);
        // ...and URL.toURI() re-encoded the string it already held.
        try {
            ckCarries("URL.toURI().getPath()",
                    new java.net.URL("http://h.example/" + LONE_HI).toURI().getPath(), 2);
        } catch (Exception e) {
            throw new AssertionError("URL.toURI must succeed: " + e);
        }
        // Controls: the ASCII-constrained components, and ordinary text.
        check("http".equals(lu.getScheme()), "the scheme must be unchanged");
        check("h.example".equals(lu.getHost()), "the host must be unchanged");
        try {
            URI plainUri = new URI("http://h.example/ok?q=1#f");
            check("/ok".equals(plainUri.getPath()) && "q=1".equals(plainUri.getQuery())
                            && "f".equals(plainUri.getFragment()),
                    "an ordinary URI must parse unchanged");
            check("/a b".equals(new URI("http://h/a%20b").getPath()),
                    "percent-decoding must still work");
        } catch (URISyntaxException e) {
            throw new AssertionError("control URI construction must succeed: " + e);
        }


        // -- java.io.File is a STRING WRAPPER, and the string round-tripped --
        // G70-1 N1. The read_string caller audit narrowed 2904 grep hits to the
        // 18 sites that both round-trip a value to Java AND are invoked under
        // --jdk-only (a static scan joined to --dump-native-registry counts).
        // java.io.File was the dominant family. `<init>` normalises the path
        // and writes it straight BACK into a Java field, so the value is not
        // inspected -- it is handed back, three times over: at construction, at
        // the field read, and once more in getName(), which went through
        // std::path + to_string_lossy.
        step("surrog", "java.io.File path round trip with a lone surrogate");
        char sep = File.separatorChar;
        File lone = new File("d/" + LONE_HI);
        // "d" sep "a" D800 "b" -- the unit lands at index 3.
        ckCarries("File.getPath()", lone.getPath(), 3);
        ckCarries("File.toString()", lone.toString(), 3);
        ckCarries("File.getName()", lone.getName(), 1);
        ckCarries("File.getParent()", new File(LONE_HI + "/x").getParent(), 1);
        ckCarries("new File(String,String).getPath()", new File("p", LONE_HI).getPath(), 3);
        ckCarries("new File(File,String).getPath()",
                new File(new File("p"), LONE_HI).getPath(), 3);

        // The substitution was not only a rendering fault. equals/hashCode/
        // compareTo are computed FROM the path, so a file named with a lone
        // surrogate and one named with a literal U+FFFD COLLIDED: equal, same
        // hash, compareTo 0. A substitution in a key is a merge, not a typo.
        File fffd = new File("d/a�b");
        check(!lone.equals(fffd),
                "a lone-surrogate path must NOT equal the same path with U+FFFD");
        check(lone.hashCode() != fffd.hashCode(),
                "distinct paths must not share a hash once the unit survives");
        check(lone.compareTo(fffd) != 0,
                "compareTo must separate a lone surrogate from U+FFFD");
        check(lone.equals(new File("d/" + LONE_HI)),
                "an equal path must still be equal to itself");

        // File.hashCode is `path.hashCode() ^ 1234321` (WinNTFileSystem folds
        // case first). The old body hashed UTF-8 BYTES and omitted the mixing
        // constant, so new File("AB") answered 2081 where HotSpot answers
        // 1235376. Both halves are asserted here, per platform.
        boolean win = sep == '\\';
        int want = (win ? "ab".hashCode() : "AB".hashCode()) ^ 1234321;
        check(new File("AB").hashCode() == want,
                "File.hashCode must be path.hashCode() ^ 1234321 (case-folded on "
                        + "Windows), got " + new File("AB").hashCode() + " want " + want);
        check(new File("ab").equals(new File("AB")) == win,
                "File equality is case-insensitive on Windows and exact elsewhere");

        // Controls: an ASCII path and a WELL-FORMED pair must be untouched by
        // the units conversion -- the rows that would catch it breaking what
        // already worked.
        check(new File("d/plain.txt").getPath().equals("d" + sep + "plain.txt"),
                "an ordinary ASCII path must be unchanged by the units conversion");
        check("plain.txt".equals(new File("d/plain.txt").getName()),
                "an ordinary basename must be unchanged");
        String fpair = new File("d/😀").getPath();
        check(fpair.length() == 4 && fpair.charAt(2) == 0xD83D && fpair.charAt(3) == 0xDE00,
                "a well-formed pair in a path must stay two units");


        // The root prefix is where a units conversion of the path helpers can
        // silently regress, and no relative-path row can see it: `getParent`
        // and `getName` both consult java.io.File's `prefixLength`, so `C:\`
        // has a NULL parent and an EMPTY name while `C:x` has parent `C:` with
        // no separator present at all. The first conversion got all three
        // wrong. These rows exist so the next one cannot.
        step("surrog", "File root-prefix contracts (getParent/getName)");
        String BS = String.valueOf((char) 92);
        File absF = win ? new File("C:" + BS + "x") : new File("/x");
        check((win ? "C:" + BS : "/").equals(absF.getParent()),
                "the parent of a root-anchored path is the root itself, got " + absF.getParent());
        File rootF = win ? new File("C:" + BS) : new File("/");
        check(rootF.getParent() == null,
                "a root has no parent, got " + rootF.getParent());
        check("".equals(rootF.getName()),
                "a root has an empty name, got [" + rootF.getName() + "]");
        check(win ? "C:".equals(new File("C:x").getParent())
                  : new File("x").getParent() == null,
                "a drive-relative path has a parent with no separator in it");

        // -- URI.relativize REBUILDS a path, and rebuilt it as text -----------
        // G75-1 N3. The accessor conversion (N1) fixed twelve of thirteen rows
        // and left this one, because relativize is the single URI operation
        // that reassembles a path from segments instead of selecting a range of
        // one -- dot-segment removal and recomposition, both splitting on ASCII
        // '/'. Reassembly is where a lossy spelling puts the substitution back.
        step("surrog", "URI.relativize with a lone surrogate");
        URI relBase;
        URI relTarget;
        URI relBase2;
        URI relTarget2;
        try {
            relBase = URI.create("http://h.example/");
            relTarget = new URI("http://h.example/" + LONE_HI);
            relBase2 = URI.create("http://h.example/d/");
            relTarget2 = new URI("http://h.example/d/" + LONE_HI + "?q=" + LONE_HI
                    + "#f" + LONE_HI);
        } catch (URISyntaxException e) {
            throw new AssertionError("URI construction must succeed: " + e);
        }
        ckCarries("URI.relativize().toString()", relBase.relativize(relTarget).toString(), 1);
        ckCarries("URI.relativize().getPath()", relBase.relativize(relTarget).getPath(), 1);
        // The query and the fragment ride through the SAME recomposition.
        URI rel2 = relBase2.relativize(relTarget2);
        ckCarries("relativize().getQuery()", rel2.getQuery(), 3);
        ckCarries("relativize().getFragment()", rel2.getFragment(), 2);
        ckCarries("relativize().toString() with query+fragment", rel2.toString(), 1);

        // Controls: relativization itself must still work, including the
        // dot-segment removal the units rewrite touched.
        try {
            check("x/y".equals(URI.create("http://h.example/d/")
                            .relativize(new URI("http://h.example/d/x/y")).toString()),
                    "ordinary relativization must be unchanged");
            check("c".equals(URI.create("http://h.example/a/")
                            .relativize(new URI("http://h.example/a/b/../c")).toString()),
                    "dot-segment removal must still collapse b/.. -> nothing");
            check("http://other.example/z".equals(URI.create("http://h.example/")
                            .relativize(new URI("http://other.example/z")).toString()),
                    "a non-prefix target must come back unchanged");
        } catch (URISyntaxException e) {
            throw new AssertionError("URI construction must succeed: " + e);
        }


        // The FOURTH File constructor, and the one the audit did not reach: it
        // was invoked zero times across the corpus, so it never entered the
        // live set that the dataflow-plus-registry filter produced. G70-1 N2
        // says a sibling next to a fixed method is not thereby fixed. It was
        // asked rather than assumed, and it diverged.
        step("surrog", "new File(URI) with a lone surrogate");
        File uriFile;
        File uriPlain;
        try {
            uriFile = new File(new URI("file:///d/" + LONE_HI));
            uriPlain = new File(new URI("file:///d/p.txt"));
        } catch (URISyntaxException e) {
            throw new AssertionError("URI construction must succeed: " + e);
        }
        ckCarries("new File(URI).getPath()", uriFile.getPath(), 4);
        check(uriPlain.getPath().endsWith("d" + BS + "p.txt")
                        || uriPlain.getPath().endsWith("d/p.txt"),
                "an ordinary file: URI must still yield its path, got " + uriPlain.getPath());

        sectionEnd("surrog", 111);
    }

    static final int SFF = 15;

    // ------------------------------------------------------------------
    // atomarr — AtomicIntegerArray (26 rows) + AtomicLongArray (26). Hazards
    // 1 and 4: the bounds check on a shadowed bytecode implementation, and
    // the LONG value argument, which occupies two frame slots.
    // ------------------------------------------------------------------
    static void atomarr() {
        String aioobe = "java.lang.ArrayIndexOutOfBoundsException";
        AtomicIntegerArray a = new AtomicIntegerArray(OPAQUE_I[I3]);
        check(a.length() == 3, "AtomicIntegerArray(3).length()");
        step("atomarr", "AtomicIntegerArray.get(-1)");
        Throwable t = null;
        try {
            sink = a.get(OPAQUE_I[IM1]);
        } catch (Throwable x) {
            t = x;
        }
        check(aioobe.equals(nameOf(t)),
                "AtomicIntegerArray.get(-1) must throw ArrayIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("atomarr", "AtomicIntegerArray.get(len)");
        t = null;
        try {
            sink = a.get(OPAQUE_I[I3]);
        } catch (Throwable x) {
            t = x;
        }
        check(aioobe.equals(nameOf(t)), "get(length) must throw, got " + nameOf(t));
        step("atomarr", "AtomicIntegerArray.set(len)");
        t = null;
        try {
            a.set(OPAQUE_I[I3], OPAQUE_I[I1]);
        } catch (Throwable x) {
            t = x;
        }
        check(aioobe.equals(nameOf(t)),
                "set(length) must throw — a WRITE past the end, got " + nameOf(t));
        step("atomarr", "AtomicIntegerArray.compareAndSet(-1)");
        t = null;
        try {
            sink = a.compareAndSet(OPAQUE_I[IM1], OPAQUE_I[I0], OPAQUE_I[I1]) ? 1 : 0;
        } catch (Throwable x) {
            t = x;
        }
        check(aioobe.equals(nameOf(t)), "compareAndSet(-1) must throw, got " + nameOf(t));
        step("atomarr", "AtomicIntegerArray.getAndSet(len)");
        t = null;
        try {
            sink = a.getAndSet(OPAQUE_I[I3], OPAQUE_I[I1]);
        } catch (Throwable x) {
            t = x;
        }
        check(aioobe.equals(nameOf(t)), "getAndSet(length) must throw, got " + nameOf(t));
        step("atomarr", "AtomicIntegerArray.addAndGet(len)");
        t = null;
        try {
            sink = a.addAndGet(OPAQUE_I[I3], OPAQUE_I[I1]);
        } catch (Throwable x) {
            t = x;
        }
        check(aioobe.equals(nameOf(t)), "addAndGet(length) must throw, got " + nameOf(t));
        step("atomarr", "new AtomicIntegerArray(-1)");
        t = null;
        try {
            sink = new AtomicIntegerArray(OPAQUE_I[IM1]).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NegativeArraySizeException".equals(nameOf(t)),
                "new AtomicIntegerArray(-1) must throw NegativeArraySizeException, got "
                        + nameOf(t));
        check(new AtomicIntegerArray(OPAQUE_I[I0]).length() == 0, "a zero-length one is legal");

        AtomicIntegerArray x = new AtomicIntegerArray(OPAQUE_I[I3]);
        x.set(OPAQUE_I[I1], OPAQUE_I[I5]);
        check(x.compareAndExchange(OPAQUE_I[I1], OPAQUE_I[I5], OPAQUE_I[I9]) == 5,
                "compareAndExchange returns the WITNESSED value on success");
        check(x.compareAndExchange(OPAQUE_I[I1], OPAQUE_I[I5], OPAQUE_I[I7]) == 9,
                "compareAndExchange returns the witnessed value on FAILURE too");
        check(x.get(OPAQUE_I[I1]) == 9, "the failed exchange must not have written");
        AtomicIntegerArray ov = new AtomicIntegerArray(OPAQUE_I[I2]);
        ov.set(OPAQUE_I[I0], Integer.MAX_VALUE);
        check(ov.getAndIncrement(OPAQUE_I[I0]) == Integer.MAX_VALUE,
                "getAndIncrement returns the OLD value");
        check(ov.get(OPAQUE_I[I0]) == Integer.MIN_VALUE,
                "the increment must WRAP at MAX_VALUE, not saturate and not panic");
        ov.set(OPAQUE_I[I1], Integer.MIN_VALUE);
        check(ov.decrementAndGet(OPAQUE_I[I1]) == Integer.MAX_VALUE,
                "the decrement must wrap at MIN_VALUE");
        check(ov.getAndAdd(OPAQUE_I[I0], OPAQUE_I[I5]) == Integer.MIN_VALUE
                        && ov.get(OPAQUE_I[I0]) == Integer.MIN_VALUE + 5,
                "getAndAdd returns the old value and adds");
        AtomicIntegerArray ts = new AtomicIntegerArray(OPAQUE_I[I2]);
        ts.set(OPAQUE_I[I0], OPAQUE_I[I7]);
        check("[7, 0]".equals(ts.toString()), "AtomicIntegerArray.toString");

        AtomicLongArray l = new AtomicLongArray(OPAQUE_I[I3]);
        step("atomarr", "AtomicLongArray.get(-1)");
        t = null;
        try {
            sinkO = Long.valueOf(l.get(OPAQUE_I[IM1]));
        } catch (Throwable xx) {
            t = xx;
        }
        check(aioobe.equals(nameOf(t)),
                "AtomicLongArray.get(-1) must throw ArrayIndexOutOfBoundsException — the LONG"
                        + " twin needs its own bounds check; got " + nameOf(t));
        step("atomarr", "AtomicLongArray.get(len)");
        t = null;
        try {
            sinkO = Long.valueOf(l.get(OPAQUE_I[I3]));
        } catch (Throwable xx) {
            t = xx;
        }
        check(aioobe.equals(nameOf(t)), "AtomicLongArray.get(length) must throw, got "
                + nameOf(t));
        step("atomarr", "AtomicLongArray.set(len)");
        t = null;
        try {
            l.set(OPAQUE_I[I3], 1L);
        } catch (Throwable xx) {
            t = xx;
        }
        check(aioobe.equals(nameOf(t)), "AtomicLongArray.set(length, v) must throw, got "
                + nameOf(t));
        step("atomarr", "new AtomicLongArray(-1)");
        t = null;
        try {
            sink = new AtomicLongArray(OPAQUE_I[IM1]).length();
        } catch (Throwable xx) {
            t = xx;
        }
        check("java.lang.NegativeArraySizeException".equals(nameOf(t)),
                "new AtomicLongArray(-1) must throw NegativeArraySizeException, got " + nameOf(t));

        // The slot-packing probe: set(int, long) puts an INT in slot 1 and a
        // LONG in slots 2-3. A native that reads the value from the wrong slot
        // loses the high half, so the value must be one whose halves differ.
        AtomicLongArray lv = new AtomicLongArray(OPAQUE_I[I3]);
        lv.set(OPAQUE_I[I1], 0x1122334455667788L);
        check(lv.get(OPAQUE_I[I1]) == 0x1122334455667788L,
                "AtomicLongArray.set(int, long) must store all SIXTY-FOUR bits — the long value"
                        + " argument spans two frame slots; got "
                        + Long.toHexString(lv.get(OPAQUE_I[I1])));
        check(lv.compareAndSet(OPAQUE_I[I1], 0x1122334455667788L, -2L),
                "compareAndSet must compare all 64 bits of the expected value");
        check(lv.get(OPAQUE_I[I1]) == -2L, "and must write the full 64-bit update");
        check(lv.getAndAdd(OPAQUE_I[I1], 3L) == -2L && lv.get(OPAQUE_I[I1]) == 1L,
                "getAndAdd on a NEGATIVE long returns the old value and adds correctly");
        AtomicLongArray lo = new AtomicLongArray(OPAQUE_I[I1]);
        lo.set(OPAQUE_I[I0], Long.MAX_VALUE);
        check(lo.incrementAndGet(OPAQUE_I[I0]) == Long.MIN_VALUE,
                "a long increment must WRAP at Long.MAX_VALUE, not panic");

        sectionEnd("atomarr", 26);
    }

    // ------------------------------------------------------------------
    // bigint — java/math/BigInteger (24 rows). The family most likely to
    // abort: division by zero, Integer.MIN_VALUE negation, negative bit
    // addresses, and a String parser.
    // ------------------------------------------------------------------
    static void bigint() {
        String ae = "java.lang.ArithmeticException";
        String nfe = "java.lang.NumberFormatException";
        BigInteger one = BigInteger.ONE;
        BigInteger zero = BigInteger.ZERO;

        step("bigint", "ONE.divide(ZERO)");
        Throwable t = null;
        try {
            sinkO = one.divide(zero);
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)),
                "BigInteger.divide by zero must throw ArithmeticException — NOT a Rust division"
                        + " panic; got " + nameOf(t));
        step("bigint", "ONE.remainder(ZERO)");
        t = null;
        try {
            sinkO = one.remainder(zero);
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)), "remainder by zero must throw ArithmeticException, got "
                + nameOf(t));
        step("bigint", "ONE.mod(ZERO)");
        t = null;
        try {
            sinkO = one.mod(zero);
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)), "mod(0) must throw ArithmeticException, got " + nameOf(t));
        step("bigint", "ONE.mod(-7)");
        t = null;
        try {
            sinkO = one.mod(new BigInteger("-7"));
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)),
                "mod with a NEGATIVE modulus must throw ArithmeticException, got " + nameOf(t));
        check("5".equals(new BigInteger("-9").mod(new BigInteger("7")).toString()),
                "mod of a negative value is the NON-NEGATIVE residue 5");
        check("-2".equals(new BigInteger("-9").remainder(new BigInteger("7")).toString()),
                "remainder of a negative value keeps the sign: -2 — this is where mod and"
                        + " remainder diverge");
        check("-1".equals(new BigInteger("-9").divide(new BigInteger("7")).toString()),
                "divide TRUNCATES toward zero");
        check("1".equals(new BigInteger("-9").divide(new BigInteger("-7")).toString()),
                "divide of two negatives");
        step("bigint", "ONE.modInverse(ZERO)");
        t = null;
        try {
            sinkO = one.modInverse(zero);
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)), "modInverse(0) must throw ArithmeticException, got "
                + nameOf(t));
        step("bigint", "4.modInverse(8) - not invertible");
        t = null;
        try {
            sinkO = new BigInteger("4").modInverse(new BigInteger("8"));
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)),
                "a non-invertible modInverse must throw ArithmeticException, not return a wrong"
                        + " answer; got " + nameOf(t));
        check("5".equals(new BigInteger("3").modInverse(new BigInteger("7")).toString()),
                "3^-1 mod 7 is 5");
        step("bigint", "4.modPow(-1, 8) - not invertible");
        t = null;
        try {
            sinkO = new BigInteger("4").modPow(new BigInteger("-1"), new BigInteger("8"));
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)),
                "modPow with a negative exponent inverts first, so a non-invertible base must"
                        + " throw ArithmeticException; got " + nameOf(t));
        check("24".equals(new BigInteger("2").modPow(new BigInteger("10"),
                new BigInteger("1000")).toString()), "2^10 mod 1000 is 24");
        step("bigint", "modPow with modulus 0");
        t = null;
        try {
            sinkO = new BigInteger("2").modPow(new BigInteger("3"), zero);
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)), "modPow(_, 0) must throw ArithmeticException, got "
                + nameOf(t));

        step("bigint", "valueOf(Long.MIN_VALUE).intValueExact()");
        t = null;
        try {
            sink = BigInteger.valueOf(Long.MIN_VALUE).intValueExact();
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)),
                "intValueExact out of int range must throw ArithmeticException — NOT truncate;"
                        + " got " + nameOf(t));
        check(new BigInteger("-2147483648").intValueExact() == Integer.MIN_VALUE,
                "intValueExact accepts exactly Integer.MIN_VALUE");
        step("bigint", "9223372036854775808.longValueExact()");
        t = null;
        try {
            sinkO = Long.valueOf(new BigInteger("9223372036854775808").longValueExact());
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)),
                "longValueExact of 2^63 must throw ArithmeticException, got " + nameOf(t));

        step("bigint", "new BigInteger(new byte[0])");
        t = null;
        try {
            sinkO = new BigInteger(new byte[0]);
        } catch (Throwable x) {
            t = x;
        }
        check(nfe.equals(nameOf(t)),
                "new BigInteger(byte[0]) must throw NumberFormatException — NOT an empty-slice"
                        + " panic and NOT zero; got " + nameOf(t));
        check("0".equals(new BigInteger(0, new byte[0]).toString()),
                "new BigInteger(0, byte[0]) IS legal and is zero — the two-argument form differs"
                        + " from the one-argument form");
        step("bigint", "new BigInteger(2, byte[])");
        t = null;
        try {
            sinkO = new BigInteger(OPAQUE_I[I2], new byte[] {1});
        } catch (Throwable x) {
            t = x;
        }
        check(nfe.equals(nameOf(t)),
                "an out-of-range signum must throw NumberFormatException, got " + nameOf(t));
        step("bigint", "new BigInteger(0, nonzero magnitude)");
        t = null;
        try {
            sinkO = new BigInteger(OPAQUE_I[I0], new byte[] {1});
        } catch (Throwable x) {
            t = x;
        }
        check(nfe.equals(nameOf(t)),
                "signum 0 with a non-zero magnitude must throw NumberFormatException, got "
                        + nameOf(t));
        check("-1".equals(new BigInteger(OPAQUE_I[IM1], new byte[] {1}).toString()),
                "signum -1 with magnitude 1 is -1");
        check("-1".equals(new BigInteger(new byte[] {(byte) 0xFF}).toString()),
                "the one-argument byte[] form is TWO'S COMPLEMENT, so 0xFF is -1");
        check("-32768".equals(new BigInteger(new byte[] {(byte) 0x80, 0}).toString()),
                "0x8000 two's complement is -32768");

        step("bigint", "ONE.shiftLeft(Integer.MIN_VALUE)");
        t = null;
        String sl = null;
        try {
            sl = one.shiftLeft(OPAQUE_I[IMIN]).toString();
        } catch (Throwable x) {
            t = x;
        }
        check("none".equals(nameOf(t)),
                "ONE.shiftLeft(Integer.MIN_VALUE) must not throw — HotSpot negates it as an"
                        + " UNSIGNED shift distance, where Rust's -i32::MIN is an overflow; got "
                        + nameOf(t));
        check("0".equals(sl),
                "and the answer is 0: a right shift by 2^31 clears every bit, got " + sl);
        step("bigint", "ZERO.shiftLeft(Integer.MIN_VALUE)");
        check("0".equals(zero.shiftLeft(OPAQUE_I[IMIN]).toString()),
                "ZERO.shiftLeft(MIN_VALUE) is 0");
        check("4".equals(new BigInteger("16").shiftLeft(OPAQUE_I[IM2]).toString()),
                "shiftLeft with a negative distance shifts RIGHT");
        check("64".equals(new BigInteger("16").shiftRight(OPAQUE_I[IM2]).toString()),
                "shiftRight with a negative distance shifts LEFT");
        check("-5".equals(new BigInteger("-9").shiftRight(OPAQUE_I[I1]).toString()),
                "shiftRight of a negative value rounds toward NEGATIVE INFINITY: -9 >> 1 is -5");
        step("bigint", "ONE.testBit(-1)");
        t = null;
        try {
            sink = one.testBit(OPAQUE_I[IM1]) ? 1 : 0;
        } catch (Throwable x) {
            t = x;
        }
        check(ae.equals(nameOf(t)),
                "testBit(-1) must throw ArithmeticException — NOT an unsigned-cast panic; got "
                        + nameOf(t));
        check(new BigInteger("-1").testBit(OPAQUE_I[I0]),
                "testBit on a negative value uses the infinite two's-complement form: -1 has bit"
                        + " 0 set");
        check(!new BigInteger("5").testBit(OPAQUE_I[I200]),
                "testBit far past the magnitude is false, not an index panic");
        check(new BigInteger("-1").bitCount() == 0,
                "bitCount of -1 is 0 — it counts bits DIFFERING from the sign bit");
        check(new BigInteger("-9").bitCount() == 1,
                "bitCount(-9) is 1 — ...11110111 has exactly one bit differing from the sign bit");
        check(new BigInteger("-1").bitLength() == 0, "bitLength of -1 is 0");
        check(zero.bitLength() == 0, "bitLength of 0 is 0");
        check(Arrays.equals(zero.toByteArray(), new byte[] {0}),
                "ZERO.toByteArray is a ONE-byte array holding 0, not an empty array");
        check(Arrays.equals(new BigInteger("-1").toByteArray(), new byte[] {(byte) 0xFF}),
                "(-1).toByteArray is {0xFF}");
        check(Arrays.equals(new BigInteger("255").toByteArray(), new byte[] {0, (byte) 0xFF}),
                "255.toByteArray needs a LEADING ZERO byte to stay positive");
        check("0".equals(zero.gcd(zero).toString()), "gcd(0, 0) is 0");
        check("6".equals(new BigInteger("-12").gcd(new BigInteger("18")).toString()),
                "gcd is always non-negative");
        check("-1".equals(zero.not().toString()), "~0 is -1");
        check("5".equals(new BigInteger("-1").and(new BigInteger("5")).toString()),
                "-1 & 5 is 5 — the bit ops are on the infinite two's-complement form");
        check("-1".equals(new BigInteger("-2").or(one).toString()), "-2 | 1 is -1");
        check("0".equals(new BigInteger("-1").xor(new BigInteger("-1")).toString()),
                "x ^ x is 0 for a negative x");
        check(new BigInteger("97").isProbablePrime(OPAQUE_I[I40]), "97 is prime");
        check(!one.isProbablePrime(OPAQUE_I[I40]), "1 is NOT prime");
        step("bigint", "isProbablePrime with a negative certainty");
        check(new BigInteger("97").isProbablePrime(OPAQUE_I[IM5]),
                "a certainty <= 0 means 'do no work and answer true', not a panic");
        check("121932631137021795226185032733622923332237463801111263526900"
                        .equals(new BigInteger("123456789012345678901234567890")
                                .multiply(new BigInteger("987654321098765432109876543210"))
                                .toString()),
                "a 30x30-digit multiply");
        check("123456788148148161864"
                        .equals(new BigInteger("123456789012345678901234567890")
                                .divide(new BigInteger("1000000007")).toString()),
                "a long division");
        step("bigint", "new BigInteger(\"\")");
        t = null;
        try {
            sinkO = new BigInteger(OPAQUE_S[SEMPTY]);
        } catch (Throwable x) {
            t = x;
        }
        check(nfe.equals(nameOf(t)),
                "new BigInteger(\"\") must throw NumberFormatException, got " + nameOf(t));
        check("7".equals(new BigInteger("+7").toString()),
                "a leading '+' is accepted");
        step("bigint", "new BigInteger(\"1_0\")");
        t = null;
        try {
            sinkO = new BigInteger("1_0");
        } catch (Throwable x) {
            t = x;
        }
        check(nfe.equals(nameOf(t)),
                "'1_0' must be a NumberFormatException — Java's grammar has no digit separators"
                        + " even though Rust's literals do; got " + nameOf(t));
        check("255".equals(new BigInteger("ff", 16).toString()), "radix 16 parse");

        sectionEnd("bigint", 55);
    }

    static final String[] FAMILIES = {
        "props", "treenav", "collect", "deque", "vector", "uri", "bytebuf", "sbidx", "surrog",
        "atomarr", "bigint",
    };

    static void runFamily(String name) {
        if ("props".equals(name)) {
            props();
        } else if ("treenav".equals(name)) {
            treenav();
        } else if ("collect".equals(name)) {
            collect();
        } else if ("deque".equals(name)) {
            deque();
        } else if ("vector".equals(name)) {
            vector();
        } else if ("uri".equals(name)) {
            uri();
        } else if ("bytebuf".equals(name)) {
            bytebuf();
        } else if ("sbidx".equals(name)) {
            sbidx();
        } else if ("surrog".equals(name)) {
            surrog();
        } else if ("atomarr".equals(name)) {
            atomarr();
        } else if ("bigint".equals(name)) {
            bigint();
        } else {
            throw new AssertionError("unknown family: " + name);
        }
    }

    public static void main(String[] args) {
        String only = null;
        for (int k = 0; k < args.length; k++) {
            if (args[k].startsWith("--only=")) {
                only = args[k].substring("--only=".length());
            } else if ("--list".equals(args[k])) {
                for (int j = 0; j < FAMILIES.length; j++) {
                    System.out.println("CK RJdkBridge1 family=" + FAMILIES[j]);
                }
                return;
            }
        }
        if (only == null) {
            for (int k = 0; k < FAMILIES.length; k++) {
                runFamily(FAMILIES[k]);
            }
        } else {
            System.out.println("CK RJdkBridge1 only=" + only);
            runFamily(only);
        }
        System.out.println("CK RJdkBridge1 checks=" + checks);
        System.out.println("PASS RJdkBridge1 (" + checks + " checks)");
    }
}
