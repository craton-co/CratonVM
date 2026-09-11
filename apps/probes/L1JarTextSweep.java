import java.io.*;
import java.text.*;
import java.util.*;
import java.util.jar.*;
import java.util.zip.*;

/** L1 §10 item 5 — the `java/util/jar/` and `java/text/` surfaces that the
 *  2026-09-11 dial bisection cleared, with precondition 4 measured for each.
 *
 *  `L1TailSweep` turned both families red as whole prefixes, and the page held
 *  them unbisected. Bisected against `cratonvm-l1hm-base-20260911`, one class
 *  at a time, scored on `L1TailSweep` (base 13 diffs):
 *
 *  ```text
 *    java/util/jar/              +34 WORSE   reached=146
 *      java/util/jar/JarFile     +34 WORSE   reached=4     <- the whole of it
 *      java/util/jar/JarEntry     +0 same    reached=16
 *      java/util/jar/Manifest     +0 same    reached=23
 *      java/util/jar/Attributes   +0 same    reached=156
 *      java/util/jar/Attributes$Name +0 same reached=127
 *    java/text/                  +16 WORSE   reached=4
 *      java/text/BreakIterator   +16 WORSE   reached=4     <- the whole of it
 *      java/text/ParseException   +0 same    reached=1
 *      java/text/Normalizer       +0 same    reached=10
 *      java/text/DateFormat       +0 same    reached=0     <- VACUOUS
 *  ```
 *
 *  So each family's red is ONE class, and the other four/three are candidates.
 *  A candidate is not a verdict — wave 3 proved a red arm can be a dial
 *  artefact and a clean tree can still hide a blocker — so this probe exists
 *  to give the trial binary something to be measured against, and to reach the
 *  22 triples of those seven classes that no probe in the tree invokes. Most of
 *  those are `ParseException`'s inherited `Throwable` surface and the four
 *  `Manifest` constructors.
 *
 *  `java/text/DateFormat` is deliberately exercised here too: its single
 *  registration read VACUOUS (`reached == 0`) in the bisection, which is
 *  §7's trap and means its green said nothing at all.
 *
 *  Row isolation is per row, and the temp jar is built under the process's own
 *  temp directory and printed by CONTENT, never by path.
 */
public final class L1JarTextSweep {
    static int rows = 0;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String tag, Body b) {
        rows++;
        String v;
        try {
            Object o = b.run();
            v = o == null ? "null" : String.valueOf(o);
        } catch (Throwable t) {
            String m = t.getMessage();
            v = "THREW " + t.getClass().getName() + (m == null ? "" : " msg=" + scrub(m));
        }
        System.out.println(tag + " |" + v + "|");
    }

    /** Never print a path: it differs between the two arms by construction. */
    static String scrub(String s) {
        return s.replaceAll("/[^ ]*/", "<path>/");
    }

    static File jar;

    static void buildJar() throws IOException {
        jar = File.createTempFile("l1jt", ".jar");
        jar.deleteOnExit();
        Manifest mf = new Manifest();
        Attributes main = mf.getMainAttributes();
        main.put(Attributes.Name.MANIFEST_VERSION, "1.0");
        main.put(Attributes.Name.MAIN_CLASS, "p.Main");
        main.putValue("Probe-Key", "probe-value");
        Attributes per = new Attributes();
        per.putValue("Section-Key", "section-value");
        mf.getEntries().put("p/one.txt", per);
        try (JarOutputStream jos = new JarOutputStream(new FileOutputStream(jar), mf)) {
            JarEntry e = new JarEntry("p/one.txt");
            e.setTime(1000000000000L);
            jos.putNextEntry(e);
            jos.write("hello world".getBytes("UTF-8"));
            jos.closeEntry();
            JarEntry d = new JarEntry("p/");
            jos.putNextEntry(d);
            jos.closeEntry();
        }
    }

    // ---- java/util/jar/Attributes and $Name --------------------------------

    static void attributes() {
        row("A.ctor0.size", () -> new Attributes().size());
        row("A.ctorCap.size", () -> new Attributes(32).size());
        row("A.ctorCopy", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            return new Attributes(a).getValue("K");
        });
        row("A.putValue.returnsOld", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V1");
            return a.putValue("K", "V2") + "/" + a.getValue("K");
        });
        row("A.getValue.miss", () -> new Attributes().getValue("nope"));
        row("A.getValue.nullName", () -> new Attributes().getValue((String) null));
        row("A.containsKey.name", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            return a.containsKey(new Attributes.Name("K")) + "/" + a.containsKey(new Attributes.Name("Z"));
        });
        row("A.containsValue", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            return a.containsValue("V") + "/" + a.containsValue("Z");
        });
        row("A.put.rejectsString", () -> {
            Attributes a = new Attributes();
            return a.put("K", "V");
        });
        row("A.remove", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            return a.remove(new Attributes.Name("K")) + "/" + a.size();
        });
        row("A.clear", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            a.clear();
            return a.size() + "/" + a.isEmpty();
        });
        row("A.putAll", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            Attributes b = new Attributes();
            b.putAll(a);
            return b.getValue("K") + "/" + b.size();
        });
        row("A.putAll.wrongType", () -> {
            Attributes a = new Attributes();
            a.putAll(Map.of("K", "V"));
            return a.size();
        });
        row("A.equalsAndHash", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            Attributes b = new Attributes();
            b.putValue("K", "V");
            return a.equals(b) + "/" + (a.hashCode() == b.hashCode());
        });
        row("A.keySet", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            List<String> s = new ArrayList<>();
            for (Object o : a.keySet()) {
                s.add(String.valueOf(o));
            }
            Collections.sort(s);
            return s;
        });
        row("A.values", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            return new ArrayList<>(a.values());
        });
        row("A.entrySet.size", () -> {
            Attributes a = new Attributes();
            a.putValue("K", "V");
            return a.entrySet().size();
        });

        row("N.ctor", () -> new Attributes.Name("Main-Class").toString());
        row("N.ctor.empty", () -> new Attributes.Name(""));
        row("N.ctor.null", () -> new Attributes.Name(null));
        row("N.ctor.illegalChar", () -> new Attributes.Name("bad name"));
        row("N.ctor.tooLong", () -> new Attributes.Name("x".repeat(71)).toString().length());
        row("N.equals.caseInsensitive", () ->
                new Attributes.Name("Main-Class").equals(new Attributes.Name("main-class")));
        row("N.hashCode.caseInsensitive", () ->
                new Attributes.Name("Main-Class").hashCode() == new Attributes.Name("MAIN-CLASS").hashCode());
        row("N.equals.nonName", () -> new Attributes.Name("K").equals("K"));
        row("N.constants", () -> Attributes.Name.MANIFEST_VERSION + "/" + Attributes.Name.MAIN_CLASS
                + "/" + Attributes.Name.CLASS_PATH + "/" + Attributes.Name.IMPLEMENTATION_TITLE);
    }

    // ---- java/util/jar/Manifest --------------------------------------------

    static void manifest() {
        row("M.ctor0.empty", () -> new Manifest().getMainAttributes().size());
        row("M.ctor0.entries", () -> new Manifest().getEntries().size());
        row("M.ctorStream", () -> {
            String text = "Manifest-Version: 1.0\r\nMain-Class: p.Main\r\n\r\n";
            Manifest m = new Manifest(new ByteArrayInputStream(text.getBytes("UTF-8")));
            return m.getMainAttributes().getValue("Main-Class");
        });
        row("M.ctorStream.sections", () -> {
            String text = "Manifest-Version: 1.0\r\n\r\nName: p/one.txt\r\nSection-Key: sv\r\n\r\n";
            Manifest m = new Manifest(new ByteArrayInputStream(text.getBytes("UTF-8")));
            return m.getEntries().size() + "/" + m.getAttributes("p/one.txt").getValue("Section-Key");
        });
        row("M.ctorStream.garbage", () -> {
            Manifest m = new Manifest(new ByteArrayInputStream("not a manifest".getBytes("UTF-8")));
            return m.getMainAttributes().size();
        });
        row("M.ctorStream.null", () -> new Manifest((InputStream) null).getMainAttributes().size());
        row("M.ctorCopy", () -> {
            Manifest a = new Manifest();
            a.getMainAttributes().putValue("Manifest-Version", "1.0");
            a.getMainAttributes().putValue("K", "V");
            return new Manifest(a).getMainAttributes().getValue("K");
        });
        row("M.ctorCopy.null", () -> new Manifest((Manifest) null).getMainAttributes().size());
        row("M.getAttributes.miss", () -> new Manifest().getAttributes("nope"));
        row("M.write.roundTrip", () -> {
            Manifest a = new Manifest();
            a.getMainAttributes().putValue("Manifest-Version", "1.0");
            a.getMainAttributes().putValue("K", "V");
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            a.write(bo);
            Manifest b = new Manifest(new ByteArrayInputStream(bo.toByteArray()));
            return b.getMainAttributes().getValue("K") + "/" + b.equals(a);
        });
        row("M.clear", () -> {
            Manifest a = new Manifest();
            a.getMainAttributes().putValue("K", "V");
            a.clear();
            return a.getMainAttributes().size() + "/" + a.getEntries().size();
        });
        row("M.equals.nonManifest", () -> new Manifest().equals("x"));
        row("M.hashCode.stable", () -> new Manifest().hashCode() == new Manifest().hashCode());
    }

    // ---- java/util/jar/JarEntry --------------------------------------------

    static void jarEntry() {
        row("E.ctorName", () -> new JarEntry("p/one.txt").getName());
        row("E.ctorName.null", () -> new JarEntry((String) null));
        row("E.ctorEntry", () -> {
            ZipEntry z = new ZipEntry("p/one.txt");
            z.setTime(1000000000000L);
            return new JarEntry(z).getTime();
        });
        row("E.ctorJarEntry", () -> {
            JarEntry a = new JarEntry("p/one.txt");
            a.setComment("c");
            return new JarEntry(a).getComment();
        });
        row("E.getAttributes.detached", () -> new JarEntry("p/one.txt").getAttributes());
        row("E.getCertificates.detached", () -> new JarEntry("p/one.txt").getCertificates());
        row("E.getCodeSigners.detached", () -> new JarEntry("p/one.txt").getCodeSigners());
        row("E.getComment.unset", () -> new JarEntry("p/one.txt").getComment());
        row("E.getCompressedSize.unset", () -> new JarEntry("p/one.txt").getCompressedSize());
        row("E.getSize.unset", () -> new JarEntry("p/one.txt").getSize());
        row("E.isDirectory", () -> new JarEntry("p/").isDirectory() + "/" + new JarEntry("p/one.txt").isDirectory());
        row("E.setGetSize", () -> {
            JarEntry e = new JarEntry("p/one.txt");
            e.setSize(11);
            return e.getSize();
        });
        row("E.setSize.negative", () -> {
            JarEntry e = new JarEntry("p/one.txt");
            e.setSize(-2);
            return e.getSize();
        });
        row("E.readFromJar", () -> {
            try (JarFile jf = new JarFile(jar)) {
                JarEntry e = jf.getJarEntry("p/one.txt");
                return e.getName() + "/" + e.getSize() + "/" + e.getTime() + "/" + e.getMethod()
                        + "/" + e.isDirectory();
            }
        });
        row("E.attributesFromJar", () -> {
            try (JarFile jf = new JarFile(jar)) {
                JarEntry e = jf.getJarEntry("p/one.txt");
                Attributes a = e.getAttributes();
                return a == null ? "null" : a.getValue("Section-Key");
            }
        });
        row("E.manifestFromJar", () -> {
            try (JarFile jf = new JarFile(jar)) {
                Manifest m = jf.getManifest();
                return m.getMainAttributes().getValue("Main-Class")
                        + "/" + m.getMainAttributes().getValue("Probe-Key")
                        + "/" + m.getEntries().size();
            }
        });
    }

    // ---- java/text/ParseException, Normalizer, DateFormat -------------------

    static void text() {
        row("P.ctor", () -> {
            ParseException e = new ParseException("bad", 4);
            return e.getMessage() + "/" + e.getErrorOffset();
        });
        row("P.ctor.nullMessage", () -> new ParseException(null, 0).getMessage());
        row("P.ctor.negativeOffset", () -> new ParseException("bad", -1).getErrorOffset());
        row("P.toString", () -> new ParseException("bad", 4).toString());
        row("P.getLocalizedMessage", () -> new ParseException("bad", 4).getLocalizedMessage());
        row("P.getCause.unset", () -> new ParseException("bad", 4).getCause());
        row("P.initCause", () -> {
            ParseException e = new ParseException("bad", 4);
            e.initCause(new IllegalStateException("why"));
            return String.valueOf(e.getCause());
        });
        row("P.initCause.twice", () -> {
            ParseException e = new ParseException("bad", 4);
            e.initCause(new IllegalStateException("one"));
            e.initCause(new IllegalStateException("two"));
            return "no-throw";
        });
        row("P.initCause.self", () -> {
            ParseException e = new ParseException("bad", 4);
            e.initCause(e);
            return "no-throw";
        });
        row("P.addSuppressed", () -> {
            ParseException e = new ParseException("bad", 4);
            e.addSuppressed(new IllegalStateException("s"));
            return e.getSuppressed().length + "/" + e.getSuppressed()[0].getMessage();
        });
        row("P.addSuppressed.null", () -> {
            new ParseException("bad", 4).addSuppressed(null);
            return "no-throw";
        });
        row("P.getSuppressed.empty", () -> new ParseException("bad", 4).getSuppressed().length);
        row("P.getStackTrace.notNull", () -> new ParseException("bad", 4).getStackTrace() != null);
        row("P.setStackTrace.empty", () -> {
            ParseException e = new ParseException("bad", 4);
            e.setStackTrace(new StackTraceElement[0]);
            return e.getStackTrace().length;
        });
        row("P.setStackTrace.null", () -> {
            new ParseException("bad", 4).setStackTrace(null);
            return "no-throw";
        });
        row("P.printStackTrace.writer", () -> {
            StringWriter w = new StringWriter();
            ParseException e = new ParseException("bad", 4);
            e.setStackTrace(new StackTraceElement[0]);
            e.printStackTrace(new PrintWriter(w));
            return w.toString().trim();
        });
        row("P.printStackTrace.stream", () -> {
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            ParseException e = new ParseException("bad", 4);
            e.setStackTrace(new StackTraceElement[0]);
            e.printStackTrace(new PrintStream(bo, true, "UTF-8"));
            return bo.toString("UTF-8").trim();
        });
        row("P.thrownByDateFormat", () -> {
            try {
                new SimpleDateFormat("yyyy-MM-dd", Locale.ROOT).parse("not-a-date");
                return "no-throw";
            } catch (ParseException e) {
                return e.getClass().getName() + "/offset=" + e.getErrorOffset();
            }
        });

        row("Z.normalize.nfc", () -> Normalizer.normalize("á", Normalizer.Form.NFC));
        row("Z.normalize.nfd.len", () -> Normalizer.normalize("á", Normalizer.Form.NFD).length());
        row("Z.normalize.nullSrc", () -> Normalizer.normalize(null, Normalizer.Form.NFC));
        row("Z.normalize.nullForm", () -> Normalizer.normalize("a", null));
        row("Z.normalize.bothNull", () -> Normalizer.normalize(null, null));
        row("Z.isNormalized.true", () -> Normalizer.isNormalized("abc", Normalizer.Form.NFC));
        row("Z.isNormalized.false", () -> Normalizer.isNormalized("á", Normalizer.Form.NFC));
        row("Z.isNormalized.nullSrc", () -> Normalizer.isNormalized(null, Normalizer.Form.NFC));
        row("Z.isNormalized.nullForm", () -> Normalizer.isNormalized("a", null));

        row("D.dateFormat.getInstance.cls", () -> DateFormat.getInstance().getClass().getName());
        row("D.dateFormat.format", () -> {
            DateFormat f = new SimpleDateFormat("yyyy-MM-dd", Locale.ROOT);
            f.setTimeZone(TimeZone.getTimeZone("UTC"));
            return f.format(new Date(1000000000000L));
        });
        row("D.dateFormat.parseObject", () -> {
            DateFormat f = new SimpleDateFormat("yyyy-MM-dd", Locale.ROOT);
            f.setTimeZone(TimeZone.getTimeZone("UTC"));
            return ((Date) f.parseObject("2001-09-09")).getTime();
        });
        row("D.dateFormat.setLenient", () -> {
            DateFormat f = new SimpleDateFormat("yyyy-MM-dd", Locale.ROOT);
            f.setLenient(false);
            return f.isLenient();
        });
        row("D.dateFormat.equals", () -> {
            DateFormat a = new SimpleDateFormat("yyyy", Locale.ROOT);
            DateFormat b = new SimpleDateFormat("yyyy", Locale.ROOT);
            return a.equals(b);
        });
    }

    static void section(String name, Body b) {
        try {
            b.run();
        } catch (Throwable t) {
            System.out.println(name + ".SECTION |ABORTED " + t.getClass().getName() + "|");
        }
    }

    public static void main(String[] a) throws Exception {
        Locale.setDefault(Locale.ROOT);
        section("JAR", () -> {
            buildJar();
            return null;
        });
        section("A", () -> {
            attributes();
            return null;
        });
        section("M", () -> {
            manifest();
            return null;
        });
        section("E", () -> {
            jarEntry();
            return null;
        });
        section("T", () -> {
            text();
            return null;
        });
        System.out.println("rows " + rows);
        System.out.println("DONE L1JarTextSweep");
    }

    private L1JarTextSweep() {
    }
}
