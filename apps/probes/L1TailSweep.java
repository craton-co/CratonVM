import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.text.*;
import java.time.*;
import java.util.*;
import java.util.jar.*;
import java.util.zip.*;

/** L1 wave 2 — the families the 118-probe tree never dispatches.
 *
 *  The per-family dial sweep of 2026-09-10 read `0 worse` for
 *  `java/util/jar/`, `java/util/zip/`, `java/text/`, `java/time/`,
 *  `java/util/Stack`, `java/util/Vector`, `java/util/PriorityQueue` and
 *  `java/util/ResourceBundle` — and 44 of 44, 43 of 44, 44 of 44 and 43 of 44
 *  of those rows were VACUOUS: `enforcement_dial.reached` was 0, so the zero
 *  was the zero of a question never posed. This probe is the question.
 *
 *  Hygiene: stdout only; no identity hash, no address, no timing, no
 *  hash-container iteration order, no absolute path. A `ZipFile` is built in
 *  a temp directory whose name is not printed. `Date`/`TimeZone` rows use a
 *  FIXED epoch and a FIXED zone so the two VMs cannot choose independently.
 */
public final class L1TailSweep {
    static int rows = 0;
    static void t(String tag, Object v) { rows++; System.out.println(tag + " |" + v + "|"); }
    static void thrown(String tag, Callable r) {
        rows++;
        try { Object v = r.call(); System.out.println(tag + " |no-throw:" + v + "|"); }
        catch (Throwable e) {
            String m = e.getMessage();
            System.out.println(tag + " |" + e.getClass().getName() + (m == null ? "" : ": " + m) + "|");
        }
    }
    interface Callable { Object call() throws Exception; }

    /** Each section is isolated, because a section that dies takes every
     *  later one with it and the survivors then read as VACUOUS rather than
     *  as untested. Two sections died on `cratonvm-l1-base-20260910` while
     *  this probe was being written (`new ZipFile(<missing>)` and
     *  `new PropertyResourceBundle(stream)`), and each one hid three whole
     *  families behind it. */
    static void section(String name, Body b) {
        try { b.run(); }
        catch (Throwable e) {
            rows++;
            System.out.println("SECTION-DIED." + name + " |" + e.getClass().getName() + "|");
        }
    }
    interface Body { void run() throws Exception; }

    public static void main(String[] a) {
        // The fallback-locale row reads `Locale.getDefault(DISPLAY)`, which is
        // an environment value the two VMs resolve independently. Pin it.
        Locale.setDefault(Locale.US);
        section("stackVector", L1TailSweep::stackVector);
        section("priorityQueue", L1TailSweep::priorityQueue);
        section("zipAndJar", L1TailSweep::zipAndJar);
        section("text", L1TailSweep::text);
        section("time", L1TailSweep::time);
        section("resourceBundle", L1TailSweep::resourceBundle);
        section("propertyBundle", L1TailSweep::propertyBundle);
        System.out.println("rows " + rows);
        System.out.println("DONE L1TailSweep");
        // AFTER the trailer: this row kills this VM outright (an uncatchable
        // `MethodCallFailed::InternalError`), so a truncation here costs one
        // line instead of the row count and the DONE marker.
        section("missingZip", L1TailSweep::missingZip);
    }

    /** LAST, deliberately, and it prints a CLASS not a message.
     *
     *  Two reasons, both learned here. HotSpot's message carries the absolute
     *  temp path, which is a value the two VMs choose independently. And on
     *  `cratonvm-l1-base-20260910` under `--jdk-only` this row does not throw
     *  a Java exception at all: `native_jar_file_open` raises
     *  `internal error: JarFile: cannot open ...`, which is
     *  `MethodCallFailed::InternalError` — uncatchable, so `thrown`'s
     *  `catch (Throwable)` never runs and the VM exits 1. Placed anywhere but
     *  last it truncates the probe at that point, and `java/text/`,
     *  `java/time/` and `java/util/ResourceBundle` then read as VACUOUS
     *  because the dial was never asked about them.
     */
    static void missingZip() throws Exception {
        Path dir = Files.createTempDirectory("l1tailz");
        rows++;
        try {
            new ZipFile(dir.resolve("absent.jar").toFile()).close();
            System.out.println("zipFile.noSuchFile |no-throw|");
        } catch (Throwable e) {
            System.out.println("zipFile.noSuchFile |" + e.getClass().getName() + "|");
        }
        Files.deleteIfExists(dir);
    }

    // ---------------------------------------------------------------- Stack
    static void stackVector() {
        Stack<String> st = new Stack<>();
        t("st.empty.fresh", st.empty());
        thrown("st.peek.empty", () -> new Stack<String>().peek());
        thrown("st.pop.empty", () -> new Stack<String>().pop());
        t("st.push", st.push("a") + "," + st.push("b") + "," + st.push("c"));
        t("st.size", st.size());
        t("st.peek", st.peek());
        t("st.pop", st.pop());
        t("st.search.top", st.search("b"));
        t("st.search.miss", st.search("zz"));
        t("st.toString", st.toString());
        t("st.contains", st.contains("a"));
        t("st.empty", st.empty());
        t("st.elementAt", st.elementAt(0));
        t("st.iterator", join(st.iterator()));
        t("st.isEmpty", st.isEmpty());
        t("st.add", st.add("d"));
        t("st.indexOf", st.indexOf("b"));
        t("st.indexOf.miss", st.indexOf("zz"));
        t("st.set", st.set(0, "A") + "," + st.toString());
        t("st.remove(int)", st.remove(0) + "," + st.toString());
        t("st.toArray", Arrays.toString(st.toArray()));
        t("st.toArray.type", st.toArray().getClass().getName());
        st.clear();
        t("st.clear", st.size() + "," + st.isEmpty() + "," + st.empty());

        Vector<String> v = new Vector<>();
        v.addElement("p"); v.addElement("q"); v.insertElementAt("o", 0);
        t("v.toString", v.toString());
        t("v.size", v.size());
        t("v.capacity>=size", v.capacity() >= v.size());
        t("v.firstElement", v.firstElement());
        t("v.lastElement", v.lastElement());
        t("v.indexOf", v.indexOf("q"));
        t("v.elements", join(Collections.list(v.elements()).iterator()));
        v.removeElement("o");
        t("v.afterRemove", v.toString());
        thrown("v.elementAt.oob", () -> new Vector<String>().elementAt(3));
        thrown("v.firstElement.empty", () -> new Vector<String>().firstElement());
    }

    // -------------------------------------------------------- PriorityQueue
    static void priorityQueue() {
        PriorityQueue<Integer> pq = new PriorityQueue<>();
        t("pq.peek.empty", String.valueOf(pq.peek()));
        t("pq.poll.empty", String.valueOf(pq.poll()));
        thrown("pq.element.empty", () -> new PriorityQueue<Integer>().element());
        thrown("pq.remove.empty", () -> new PriorityQueue<Integer>().remove());
        for (int x : new int[] { 5, 1, 9, 3, 7 }) { pq.offer(x); }
        t("pq.size", pq.size());
        t("pq.peek", pq.peek());
        StringBuilder drained = new StringBuilder();
        PriorityQueue<Integer> copy = new PriorityQueue<>(pq);
        while (!copy.isEmpty()) { drained.append(copy.poll()).append('.'); }
        t("pq.drainOrder", drained.toString());
        t("pq.contains", pq.contains(9));
        t("pq.remove(obj)", pq.remove(9) + "," + pq.size());
        PriorityQueue<String> rev = new PriorityQueue<>(Comparator.reverseOrder());
        rev.addAll(List.of("b", "d", "a"));
        t("pq.comparator", rev.poll() + rev.poll() + rev.poll());
        thrown("pq.offer.null", () -> new PriorityQueue<String>().offer(null));
        List<Integer> sorted = new ArrayList<>(pq);
        Collections.sort(sorted);
        t("pq.contentsSorted", sorted.toString());
        PriorityQueue<Integer> one = new PriorityQueue<>();
        one.add(42);
        t("pq.toString.one", one.toString());
        t("pq.toString.empty", new PriorityQueue<Integer>().toString());
    }

    // -------------------------------------------------------- zip and jar
    static void zipAndJar() throws Exception {
        CRC32 c = new CRC32();
        c.update("hello".getBytes(StandardCharsets.UTF_8));
        t("crc32", c.getValue());
        c.reset();
        t("crc32.reset", c.getValue());
        CRC32C c32 = new CRC32C();
        c32.update("hello".getBytes(StandardCharsets.UTF_8));
        t("crc32c", c32.getValue());
        c32.reset();
        t("crc32c.reset", c32.getValue());
        CRC32C c32b = new CRC32C();
        c32b.update('h');
        c32b.update('i');
        t("crc32c.updateInt", c32b.getValue());
        CRC32C c32c = new CRC32C();
        byte[] hb = "xxhelloxx".getBytes(StandardCharsets.UTF_8);
        c32c.update(hb, 2, 5);
        t("crc32c.updateRange", c32c.getValue());
        t("crc32c.rangeEqualsWhole", c32c.getValue() == c32.getValue());
        thrown("crc32c.updateRange.oob", () -> { new CRC32C().update(hb, 2, 99); return "ok"; });
        Adler32 ad = new Adler32();
        ad.update("hello".getBytes(StandardCharsets.UTF_8));
        t("adler32", ad.getValue());

        Path dir = Files.createTempDirectory("l1tail");
        Path jar = dir.resolve("probe.jar");
        Manifest mf = new Manifest();
        mf.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        mf.getMainAttributes().put(Attributes.Name.MAIN_CLASS, "p.Main");
        mf.getMainAttributes().put(new Attributes.Name("Probe-Key"), "probe-value");
        try (JarOutputStream jos = new JarOutputStream(Files.newOutputStream(jar), mf)) {
            JarEntry e1 = new JarEntry("p/one.txt");
            e1.setTime(1000000000000L);
            jos.putNextEntry(e1);
            jos.write("first entry".getBytes(StandardCharsets.UTF_8));
            jos.closeEntry();
            JarEntry e2 = new JarEntry("p/two.txt");
            e2.setTime(1000000000000L);
            jos.putNextEntry(e2);
            jos.write("second".getBytes(StandardCharsets.UTF_8));
            jos.closeEntry();
        }
        t("jar.exists", Files.size(jar) > 0);

        try (JarFile jf = new JarFile(jar.toFile())) {
            List<String> names = new ArrayList<>();
            Enumeration<JarEntry> en = jf.entries();
            while (en.hasMoreElements()) { names.add(en.nextElement().getName()); }
            Collections.sort(names);
            t("jf.entries", names.toString());
            t("jf.size", jf.size());
            JarEntry one = jf.getJarEntry("p/one.txt");
            t("jf.getJarEntry", one == null ? "null" : one.getName());
            t("je.getSize", one == null ? -1L : one.getSize());
            t("je.getTime", one == null ? -1L : one.getTime());
            t("je.isDirectory", one != null && one.isDirectory());
            t("je.getMethod", one == null ? -1 : one.getMethod());
            try (InputStream is = jf.getInputStream(one)) {
                t("jf.getInputStream", new String(is.readAllBytes(), StandardCharsets.UTF_8));
            }
            Manifest got = jf.getManifest();
            t("jf.manifest.notNull", got != null);
            t("mf.mainClass", got == null ? "null" : got.getMainAttributes().getValue(Attributes.Name.MAIN_CLASS));
            t("mf.probeKey", got == null ? "null" : got.getMainAttributes().getValue("Probe-Key"));
            t("mf.version", got == null ? "null" : got.getMainAttributes().getValue(Attributes.Name.MANIFEST_VERSION));
            t("mf.attrSize", got == null ? -1 : got.getMainAttributes().size());
            t("attrName.equals", Attributes.Name.MAIN_CLASS.equals(new Attributes.Name("Main-Class")));
            t("attrName.toString", Attributes.Name.MAIN_CLASS.toString());
            t("attrName.hashEq", Attributes.Name.MAIN_CLASS.hashCode()
                                 == new Attributes.Name("Main-Class").hashCode());
            t("jf.getName.endsWith", jf.getName().endsWith("probe.jar"));
            t("jf.missing", String.valueOf(jf.getJarEntry("p/nope.txt")));
        }

        try (ZipFile zf = new ZipFile(jar.toFile())) {
            t("zf.size", zf.size());
            ZipEntry ze = zf.getEntry("p/two.txt");
            t("zf.getEntry", ze == null ? "null" : ze.getName());
            t("ze.getSize", ze == null ? -1L : ze.getSize());
            t("ze.getCompressedSize>0", ze != null && ze.getCompressedSize() > 0);
            try (InputStream is = zf.getInputStream(ze)) {
                t("zf.getInputStream", new String(is.readAllBytes(), StandardCharsets.UTF_8));
            }
            t("zf.getName.endsWith", zf.getName().endsWith("probe.jar"));
            t("zf.missing", String.valueOf(zf.getEntry("nope")));
            List<String> zn = new ArrayList<>();
            Enumeration<? extends ZipEntry> ze2 = zf.entries();
            while (ze2.hasMoreElements()) { zn.add(ze2.nextElement().getName()); }
            Collections.sort(zn);
            t("zf.entries", zn.toString());
            t("zf.comment", String.valueOf(zf.getComment()));
            List<String> zs = zf.stream().map(ZipEntry::getName).sorted().toList();
            t("zf.stream", zs.toString());
        }
        // ZipFile(String) — the ctor overload the tree never dispatched.
        try (ZipFile zfs = new ZipFile(jar.toString())) {
            t("zfString.size", zfs.size());
            t("zfString.getEntry", String.valueOf(zfs.getEntry("p/one.txt") != null));
        }
        ZipEntry ce = new ZipEntry("c.txt");
        ce.setComment("a comment");
        t("ze.setComment", ce.getComment());
        t("ze.getName", ce.getName());

        // round-trip through the stream API, which is where a wrong CRC shows
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ZipOutputStream zos = new ZipOutputStream(bos)) {
            ZipEntry e = new ZipEntry("x.txt");
            e.setTime(1000000000000L);
            zos.putNextEntry(e);
            zos.write("round trip".getBytes(StandardCharsets.UTF_8));
            zos.closeEntry();
        }
        try (ZipInputStream zis = new ZipInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            ZipEntry e = zis.getNextEntry();
            t("zis.name", e == null ? "null" : e.getName());
            t("zis.body", new String(zis.readAllBytes(), StandardCharsets.UTF_8));
        }
        Files.deleteIfExists(jar);
        Files.deleteIfExists(dir);
    }

    // ------------------------------------------------------------- java.text
    static void text() {
        BreakIterator w = BreakIterator.getWordInstance(Locale.ROOT);
        w.setText("one two three");
        List<Integer> bounds = new ArrayList<>();
        for (int i = w.first(); i != BreakIterator.DONE; i = w.next()) { bounds.add(i); }
        t("bi.word.bounds", bounds.toString());
        BreakIterator s = BreakIterator.getSentenceInstance(Locale.ROOT);
        s.setText("A b. C d.");
        List<Integer> sb = new ArrayList<>();
        for (int i = s.first(); i != BreakIterator.DONE; i = s.next()) { sb.add(i); }
        t("bi.sentence.bounds", sb.toString());
        BreakIterator ch = BreakIterator.getCharacterInstance(Locale.ROOT);
        ch.setText("ab");
        t("bi.char.first", ch.first());
        t("bi.char.last", ch.last());
        t("bi.char.previous", ch.previous());
        BreakIterator l = BreakIterator.getLineInstance(Locale.ROOT);
        l.setText("aa bb");
        t("bi.line.first", l.first());
        t("bi.line.next", l.next());

        t("norm.NFC", Normalizer.normalize("Å", Normalizer.Form.NFC).length());
        t("norm.NFD", Normalizer.normalize("Å", Normalizer.Form.NFD).length());
        t("norm.isNormalized", Normalizer.isNormalized("abc", Normalizer.Form.NFC));
        thrown("norm.null", () -> Normalizer.normalize(null, Normalizer.Form.NFC));
        thrown("norm.nullForm", () -> Normalizer.normalize("a", null));
        thrown("norm.bothNull", () -> Normalizer.normalize(null, null));
        thrown("isNorm.null", () -> Normalizer.isNormalized(null, Normalizer.Form.NFC));
        thrown("isNorm.nullForm", () -> Normalizer.isNormalized("a", null));

        // ParseException is lane T's registrar; read it, do not retire it.
        thrown("df.parse.bad", () -> new SimpleDateFormat("yyyy", Locale.ROOT).parse("zzz"));
    }

    // ------------------------------------------------------------- java.time
    static void time() {
        Duration d = Duration.ofSeconds(3725, 500);
        t("dur.toString", d.toString());
        t("dur.seconds", d.getSeconds());
        t("dur.nano", d.getNano());
        t("dur.plus", d.plusMinutes(2).toString());
        t("dur.parse", Duration.parse("PT1H2M3S").toString());
        t("dur.compare", Duration.ofSeconds(1).compareTo(Duration.ofSeconds(2)));
        t("zone.utc", ZoneId.of("UTC").getId());
        t("zone.offset", ZoneId.of("+02:00").getId());
        t("zone.fixed", ZoneOffset.ofHours(-5).getId());
        thrown("zone.bad", () -> ZoneId.of("Not/AZone"));
        Instant i = Instant.ofEpochSecond(1000000000L);
        t("instant.toString", i.toString());
        t("ldt", LocalDateTime.ofInstant(i, ZoneOffset.UTC).toString());
    }

    // -------------------------------------------------------- ResourceBundle
    static void resourceBundle() throws Exception {
        thrown("rb.missing", () -> ResourceBundle.getBundle("no.such.Bundle", Locale.ROOT));
        t("rb.control.formats", ResourceBundle.Control.FORMAT_DEFAULT.toString());
        ResourceBundle.Control ctl = ResourceBundle.Control.getControl(ResourceBundle.Control.FORMAT_DEFAULT);
        t("rb.control.toBundleName", ctl.toBundleName("a.B", Locale.ROOT));
        t("rb.control.toResourceName", ctl.toResourceName("a.B", "properties"));
        t("rb.control.candidates", ctl.getCandidateLocales("a.B", Locale.ROOT).toString());
        t("rb.control.fallback", String.valueOf(ctl.getFallbackLocale("a.B", Locale.ROOT)));
        getBundleOverloads();
    }

    /** Split out: `new PropertyResourceBundle(stream)` NPEs on this VM
     *  (`Cannot invoke "java.util.Collection.toArray()" because "c" is null`),
     *  and with it inline the seven `getBundle` overload rows below it never
     *  ran. */
    static void propertyBundle() throws Exception {
        // A bundle that DOES exist in every image: sun.text.resources is not
        // portable, so build one from a Properties stream instead.
        ResourceBundle pb = new PropertyResourceBundle(
                new ByteArrayInputStream("k1=v1\nk2=v2\n".getBytes(StandardCharsets.UTF_8)));
        List<String> keys = new ArrayList<>(pb.keySet());
        Collections.sort(keys);
        t("prb.keys", keys.toString());
        t("prb.getString", pb.getString("k1"));
        t("prb.containsKey", pb.containsKey("k2"));
        thrown("prb.missingKey", () -> pb.getString("nope"));
        List<String> en = new ArrayList<>();
        Enumeration<String> e = pb.getKeys();
        while (e.hasMoreElements()) { en.add(e.nextElement()); }
        Collections.sort(en);
        t("prb.getKeys", en.toString());
        t("prb.getObject", String.valueOf(pb.getObject("k2")));
        t("prb.getLocale", String.valueOf(pb.getLocale()));
        t("prb.getBaseBundleName", String.valueOf(pb.getBaseBundleName()));
        List<String> ks = new ArrayList<>(pb.keySet());
        Collections.sort(ks);
        t("prb.keySet", ks.toString());

    }

    static void getBundleOverloads() {
        // Every `getBundle` overload, against a base name no image carries.
        // The ANSWER is the refusal, and the refusal's TEXT is the row: it
        // names the locale the lookup ended on, which is what a wrong
        // candidate list changes.
        ClassLoader cl = L1TailSweep.class.getClassLoader();
        ResourceBundle.Control dflt =
                ResourceBundle.Control.getControl(ResourceBundle.Control.FORMAT_DEFAULT);
        thrown("gb.name", () -> ResourceBundle.getBundle("no.such.B"));
        thrown("gb.name.loc.cl", () -> ResourceBundle.getBundle("no.such.B", Locale.ROOT, cl));
        thrown("gb.name.loc.cl.ctl",
               () -> ResourceBundle.getBundle("no.such.B", Locale.ROOT, cl, dflt));
        thrown("gb.name.loc.ctl", () -> ResourceBundle.getBundle("no.such.B", Locale.ROOT, dflt));
        thrown("gb.name.ctl", () -> ResourceBundle.getBundle("no.such.B", dflt));
        thrown("gb.name.mod",
               () -> ResourceBundle.getBundle("no.such.B", L1TailSweep.class.getModule()));
        thrown("gb.name.loc.mod",
               () -> ResourceBundle.getBundle("no.such.B", Locale.ROOT,
                                              L1TailSweep.class.getModule()));
    }

    static String join(Iterator<?> it) {
        StringBuilder sb = new StringBuilder("[");
        while (it.hasNext()) { sb.append(it.next()); if (it.hasNext()) { sb.append(", "); } }
        return sb.append(']').toString();
    }

    private L1TailSweep() { }
}
