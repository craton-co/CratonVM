/*
 * JdkBaseline — emit one stable, diffable baseline file per JDK class, from the
 * running JDK's own runtime image.
 *
 * Driven by scripts/jdk-baseline/generate.py; see
 * docs/known-issues/jdk-only/E32-R11-JDK-BASELINE-CAPABILITY-20260813.md for the
 * format, the failure mode a consuming guard must have, and the nominated Rust
 * helper.
 *
 * WHY THE CLASS FILE AND NOT `javap` TEXT
 * ---------------------------------------
 * The source is `jrt:/modules/<module>/<binary/name>.class`, parsed with
 * `java.lang.classfile` (JEP 484, final in JDK 24). Three reasons, each of which
 * bit something in this tree:
 *
 *   1. `javap` prints SOURCE-LEVEL signatures and puts the JVM descriptor on a
 *      separate `descriptor:` line. Pairing the two means parsing prose. The
 *      class file carries the descriptor as one constant-pool string, which is
 *      the exact key `NativeRegistry::register(class, name, descriptor)` uses.
 *   2. Reflection (`Class.forName` + `getDeclaredMethods`) cannot load a class
 *      whose class-file minor version is 65535 — every JEP 505
 *      `StructuredTaskScope` class in JDK 25 is preview-versioned, and those are
 *      exactly the classes row 16 of the E25 sweep needs an oracle for. Reading
 *      the bytes has no such restriction.
 *   3. `jdk.internal.access.SharedSecrets` is in a package `java.base` does not
 *      export. Reading its bytes needs no `--add-exports`; the generator must
 *      not need flags that a CI job might forget.
 *
 * WHY EVERY MEMBER AND NOT JUST THE PUBLIC ONES  (format version 2, F23-1)
 * ------------------------------------------------------------------------
 * Version 1 of this file emitted a member only if ACC_PUBLIC or ACC_PROTECTED
 * was set. That made the baselines structurally blind to package-private and
 * private members -- which is the access level MOST JDK natives live at, and
 * therefore the one population a *native* registrar most needs an oracle for.
 * On `jdk.internal.misc.CDS`, the first class anybody pointed `javap -p` at, the
 * blind spot produced an error in both directions at once:
 *
 *   - false positive: `logLambdaFormInvoker(Ljava/lang/String;)V` read as
 *     off-surface. It is real -- `private static native` -- and sits beside a
 *     public four-String overload whose body just concatenates and calls it.
 *   - false negative: `getCDSConfigStatus()I` (called from `<clinit>`, so every
 *     public predicate on the class depends on it) and
 *     `needsClassInitBarrier0(Ljava/lang/Class;)Z` are real natives with no
 *     registration, and a public-only baseline cannot report a MISSING private
 *     native as anything at all.
 *
 * So version 2 emits every entry of the method and field tables, with the access
 * level in the flags column. "Does this member exist" and "is this member public"
 * become two different questions a consumer asks separately -- see
 * `Baseline::declared_surface` vs `Baseline::public_surface` in
 * native-builtins/src/jdk_baseline.rs. The `# public-methods` and
 * `# protected-methods` headers keep their version-1 meaning exactly, so a guard
 * whose denominator was the public surface has the same denominator it had.
 *
 * The version was bumped even though no COLUMN changed. The row grammar is the
 * same; the population is not, and a version-1 parser reading a version-2 file
 * would answer `declares()` from a wider set than it was written against without
 * ever saying so. That is the same silent-widening this change exists to stop.
 *
 * BYTE STABILITY
 * --------------
 * Every collection is sorted before emission; rows are joined with an explicit
 * '\n' (never System.lineSeparator()) and written UTF-8. No timestamp is
 * emitted — the only volatile header fields are the JDK's own version strings,
 * and they are there on purpose: a JDK bump must show up as a diff.
 */

import java.io.IOException;
import java.lang.classfile.ClassFile;
import java.lang.classfile.ClassModel;
import java.lang.classfile.FieldModel;
import java.lang.classfile.MethodModel;
import java.lang.classfile.constantpool.ClassEntry;
import java.lang.module.ModuleDescriptor;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Locale;
import java.util.Optional;
import java.util.TreeSet;

public final class JdkBaseline {

    /**
     * Bumped when the row grammar OR the emitted population changes. A consumer must reject a
     * version it does not know.
     *
     * <p>2 (F23-1): every member is emitted, not only public/protected ones.
     */
    static final String FORMAT_VERSION = "2";

    public static void main(String[] args) throws Exception {
        Path outDir = null;
        String prefix = "jdk25-";
        List<String> classes = new ArrayList<>();
        List<String> modules = new ArrayList<>();
        boolean withSupertypes = false;
        boolean stdout = false;

        for (int i = 0; i < args.length; i++) {
            switch (args[i]) {
                case "--out" -> outDir = Paths.get(args[++i]);
                case "--prefix" -> prefix = args[++i];
                case "--class" -> classes.add(args[++i]);
                case "--classes-file" -> readList(Paths.get(args[++i]), classes, modules);
                case "--module" -> modules.add(args[++i]);
                case "--with-supertypes" -> withSupertypes = true;
                case "--stdout" -> stdout = true;
                default -> throw new IllegalArgumentException("unknown arg: " + args[i]);
            }
        }

        FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
        List<String> moduleDirs = new ArrayList<>();
        try (var s = Files.list(jrt.getPath("/modules"))) {
            s.forEach(p -> moduleDirs.add(p.getFileName().toString()));
        }
        Collections.sort(moduleDirs);

        LinkedHashSet<String> todo = new LinkedHashSet<>(classes);
        if (withSupertypes) {
            LinkedHashSet<String> extra = new LinkedHashSet<>();
            for (String c : classes) {
                extra.addAll(supertypeClosure(jrt, moduleDirs, c));
            }
            todo.addAll(extra);
        }

        for (String binaryName : new TreeSet<>(todo)) {
            emit(outDir, prefix + binaryName + ".tsv",
                    classBaseline(jrt, moduleDirs, binaryName), stdout);
        }
        for (String m : new TreeSet<>(modules)) {
            emit(outDir, prefix + "module-" + m + ".tsv", moduleBaseline(m), stdout);
        }
    }

    static void emit(Path outDir, String name, String text, boolean stdout) throws IOException {
        if (stdout || outDir == null) {
            System.out.print("===== " + name + " =====\n");
            System.out.print(text);
        } else {
            Files.createDirectories(outDir);
            Files.write(outDir.resolve(name), text.getBytes(StandardCharsets.UTF_8));
            System.out.println("wrote " + outDir.resolve(name));
        }
    }

    /** `class <fqn>` / `module <name>` / bare fqn. `#` comments and blanks skipped. */
    static void readList(Path p, List<String> classes, List<String> modules) throws IOException {
        for (String line : Files.readAllLines(p, StandardCharsets.UTF_8)) {
            // Cut a trailing `# ...` comment. No binary name, module name or
            // descriptor can contain '#', so this is unambiguous.
            int hash = line.indexOf('#');
            String t = (hash >= 0 ? line.substring(0, hash) : line).strip();
            if (t.isEmpty()) {
                continue;
            }
            if (t.startsWith("module ")) {
                modules.add(t.substring("module ".length()).strip());
            } else if (t.startsWith("class ")) {
                classes.add(t.substring("class ".length()).strip());
            } else {
                classes.add(t);
            }
        }
    }

    // ---------------------------------------------------------------- lookup

    record Located(String module, ClassModel model) {}

    static Located locate(FileSystem jrt, List<String> moduleDirs, String binaryName) {
        String rel = binaryName.replace('.', '/') + ".class";
        for (String m : moduleDirs) {
            Path p = jrt.getPath("/modules", m, rel);
            if (Files.exists(p)) {
                try {
                    return new Located(m, ClassFile.of().parse(Files.readAllBytes(p)));
                } catch (IOException e) {
                    throw new RuntimeException("read " + p, e);
                }
            }
        }
        throw new IllegalStateException(
                "NOT IN THE RUNTIME IMAGE: " + binaryName + " (searched " + moduleDirs.size()
                        + " modules under jrt:/modules). A class that is not in the image cannot"
                        + " be baselined from it; say so in the record rather than emitting an"
                        + " empty file, which would read as 'this class has no members'.");
    }

    static List<String> supertypeClosure(FileSystem jrt, List<String> moduleDirs, String binaryName) {
        LinkedHashSet<String> seen = new LinkedHashSet<>();
        collect(jrt, moduleDirs, binaryName, seen);
        seen.remove(binaryName);
        return new ArrayList<>(seen);
    }

    static void collect(FileSystem jrt, List<String> moduleDirs, String binaryName,
                        LinkedHashSet<String> seen) {
        if (!seen.add(binaryName)) {
            return;
        }
        Located l;
        try {
            l = locate(jrt, moduleDirs, binaryName);
        } catch (IllegalStateException e) {
            return; // not in the image; recorded by its absence from SUPERTYPE rows
        }
        Optional<ClassEntry> sup = l.model().superclass();
        sup.ifPresent(ce -> collect(jrt, moduleDirs, binary(ce.asInternalName()), seen));
        for (ClassEntry ce : l.model().interfaces()) {
            collect(jrt, moduleDirs, binary(ce.asInternalName()), seen);
        }
    }

    static String binary(String internalName) {
        return internalName.replace('/', '.');
    }

    // ------------------------------------------------------------- emission

    static String classBaseline(FileSystem jrt, List<String> moduleDirs, String binaryName) {
        Located l = locate(jrt, moduleDirs, binaryName);
        ClassModel cm = l.model();

        List<String> rows = new ArrayList<>();
        rows.add(row("CLASS", binaryName, "", classFlags(cm.flags().flagsMask())));
        rows.add(row("EXTENDS",
                cm.superclass().map(ce -> binary(ce.asInternalName())).orElse(""), "", ""));
        TreeSet<String> ifaces = new TreeSet<>();
        for (ClassEntry ce : cm.interfaces()) {
            ifaces.add(binary(ce.asInternalName()));
        }
        for (String i : ifaces) {
            rows.add(row("IMPLEMENTS", i, "", ""));
        }
        // Transitive closure, so a consumer can tell that a method it is looking
        // for is INHERITED rather than absent. `javap -public` never shows these.
        for (String s : new TreeSet<>(supertypeClosure(jrt, moduleDirs, binaryName))) {
            rows.add(row("SUPERTYPE", s, "", ""));
        }

        // EVERY entry of the method table, at every access level. See the
        // "WHY EVERY MEMBER" note at the top of this file: the version-1
        // public/protected filter that used to stand here is the defect.
        TreeSet<String> members = new TreeSet<>();
        int publicMethods = 0;
        int protectedMethods = 0;
        int declaredMethods = 0;
        for (MethodModel m : cm.methods()) {
            int f = m.flags().flagsMask();
            String name = m.methodName().stringValue();
            members.add(row("METHOD", name, m.methodType().stringValue(), methodFlags(f)));
            if (!name.equals("<clinit>")) {
                // `# declared-methods` counts <init> and excludes <clinit>, which is
                // exactly what `javap -p <class> | grep -c '('` counts: javap prints
                // the class initialiser as `static {};`, with no parentheses.
                declaredMethods++;
            }
            if (!name.equals("<init>") && !name.equals("<clinit>")) {
                if ((f & ACC_PUBLIC) != 0) {
                    publicMethods++;
                } else if ((f & ACC_PROTECTED) != 0) {
                    protectedMethods++;
                }
            }
        }
        TreeSet<String> fields = new TreeSet<>();
        int declaredFields = 0;
        for (FieldModel f : cm.fields()) {
            int fl = f.flags().flagsMask();
            fields.add(row("FIELD", f.fieldName().stringValue(),
                    f.fieldType().stringValue(), fieldFlags(fl)));
            declaredFields++;
        }
        // The rows go into TreeSets to be sorted, and a set can silently swallow a
        // duplicate. JVMS 4.6 forbids two methods with the same name AND descriptor,
        // so a collision here means the parse is wrong, not the class file -- and it
        // would show up as a header count no reader could reconcile.
        if (members.size() != cm.methods().size() || fields.size() != cm.fields().size()) {
            throw new IllegalStateException(binaryName + ": the sorted row set lost a member ("
                    + members.size() + "/" + cm.methods().size() + " methods, "
                    + fields.size() + "/" + cm.fields().size() + " fields). Two class-file "
                    + "entries produced the same row, which JVMS 4.6 says cannot happen.");
        }
        rows.addAll(members);
        rows.addAll(fields);

        StringBuilder sb = new StringBuilder();
        header(sb, "class", binaryName, l.module(),
                "jrt:/modules/" + l.module() + "/" + binaryName.replace('.', '/') + ".class");
        // Counts are DERIVED from the rows below and exist so a truncated or
        // hand-edited file is caught by the reader before it is believed.
        sb.append("# public-methods\t").append(publicMethods).append('\n');
        sb.append("# protected-methods\t").append(protectedMethods).append('\n');
        // `# declared-methods` is the version-2 population: every method-table
        // entry except <clinit>, at every access level. It is the number
        // `javap -p <class> | grep -c '('` prints. `# public-methods` is
        // unchanged from version 1 and remains the denominator of every guard
        // whose question is "is this on the PUBLIC surface".
        sb.append("# declared-methods\t").append(declaredMethods).append('\n');
        sb.append("# declared-fields\t").append(declaredFields).append('\n');
        sb.append("# rows\t").append(rows.size()).append('\n');
        sb.append("# columns\tkind\tname\tdescriptor\tflags\n");
        for (String r : rows) {
            sb.append(r).append('\n');
        }
        return sb.toString();
    }

    static String moduleBaseline(String moduleName) {
        ModuleDescriptor d = ModuleLayer.boot().findModule(moduleName)
                .orElseThrow(() -> new IllegalStateException(
                        "no such module in the boot layer: " + moduleName))
                .getDescriptor();
        TreeSet<String> rows = new TreeSet<>();
        for (ModuleDescriptor.Requires r : d.requires()) {
            rows.add(row("REQUIRES", r.name(), "", lowerJoin(r.modifiers())));
        }
        int unqualifiedExports = 0;
        for (ModuleDescriptor.Exports e : d.exports()) {
            if (e.targets().isEmpty()) {
                rows.add(row("EXPORTS", e.source(), "", ""));
                unqualifiedExports++;
            } else {
                for (String t : new TreeSet<>(e.targets())) {
                    rows.add(row("EXPORTS", e.source(), t, "qualified"));
                }
            }
        }
        for (ModuleDescriptor.Opens o : d.opens()) {
            if (o.targets().isEmpty()) {
                rows.add(row("OPENS", o.source(), "", ""));
            } else {
                for (String t : new TreeSet<>(o.targets())) {
                    rows.add(row("OPENS", o.source(), t, "qualified"));
                }
            }
        }
        for (String u : new TreeSet<>(d.uses())) {
            rows.add(row("USES", u, "", ""));
        }
        for (ModuleDescriptor.Provides p : d.provides()) {
            for (String impl : p.providers()) {
                rows.add(row("PROVIDES", p.service(), impl, ""));
            }
        }
        for (String pkg : new TreeSet<>(d.packages())) {
            rows.add(row("PACKAGE", pkg, "", ""));
        }

        StringBuilder sb = new StringBuilder();
        header(sb, "module", moduleName, moduleName,
                "ModuleLayer.boot().findModule(\"" + moduleName + "\").getDescriptor()");
        sb.append("# unqualified-exports\t").append(unqualifiedExports).append('\n');
        sb.append("# rows\t").append(rows.size()).append('\n');
        sb.append("# columns\tkind\tname\ttarget\tflags\n");
        for (String r : rows) {
            sb.append(r).append('\n');
        }
        return sb.toString();
    }

    static String lowerJoin(java.util.Set<?> set) {
        TreeSet<String> t = new TreeSet<>();
        for (Object o : set) {
            t.add(o.toString().toLowerCase(Locale.ROOT));
        }
        return String.join(",", t);
    }

    static void header(StringBuilder sb, String kind, String name, String module, String source) {
        sb.append("# jdk-baseline\t").append(FORMAT_VERSION).append('\n');
        sb.append("# kind\t").append(kind).append('\n');
        sb.append("# name\t").append(name).append('\n');
        sb.append("# module\t").append(module).append('\n');
        sb.append("# java.version\t").append(System.getProperty("java.version")).append('\n');
        sb.append("# java.vm.version\t").append(System.getProperty("java.vm.version")).append('\n');
        sb.append("# java.vendor.version\t")
                .append(System.getProperty("java.vendor.version", "")).append('\n');
        sb.append("# source\t").append(source).append('\n');
        sb.append("# generator\tscripts/jdk-baseline/generate.py\n");
        sb.append("# regenerate\tpython scripts/jdk-baseline/generate.py --update\n");
    }

    static String row(String kind, String a, String b, String flags) {
        // No field may contain a tab: JVM descriptors, binary names, package
        // names and module names cannot, so TSV needs no quoting or escaping.
        return kind + "\t" + a + "\t" + b + "\t" + flags;
    }

    // --------------------------------------------------------------- flags

    static final int ACC_PUBLIC = 0x0001;
    static final int ACC_PRIVATE = 0x0002;
    static final int ACC_PROTECTED = 0x0004;
    static final int ACC_STATIC = 0x0008;
    static final int ACC_FINAL = 0x0010;
    static final int ACC_SYNCHRONIZED = 0x0020;
    static final int ACC_VOLATILE = 0x0040;
    static final int ACC_BRIDGE = 0x0040;
    static final int ACC_TRANSIENT = 0x0080;
    static final int ACC_VARARGS = 0x0080;
    static final int ACC_NATIVE = 0x0100;
    static final int ACC_INTERFACE = 0x0200;
    static final int ACC_ABSTRACT = 0x0400;
    static final int ACC_STRICT = 0x0800;
    static final int ACC_SYNTHETIC = 0x1000;
    static final int ACC_ANNOTATION = 0x2000;
    static final int ACC_ENUM = 0x4000;

    // ACC_SUPER (0x0020) is deliberately NOT emitted for classes: it is the same
    // bit as ACC_SYNCHRONIZED and carries no meaning for a member census.
    static String classFlags(int f) {
        List<String> out = new ArrayList<>();
        if ((f & ACC_PUBLIC) != 0) out.add("public");
        if ((f & ACC_FINAL) != 0) out.add("final");
        if ((f & ACC_INTERFACE) != 0) out.add("interface");
        if ((f & ACC_ABSTRACT) != 0) out.add("abstract");
        if ((f & ACC_ANNOTATION) != 0) out.add("annotation");
        if ((f & ACC_ENUM) != 0) out.add("enum");
        if ((f & ACC_SYNTHETIC) != 0) out.add("synthetic");
        return String.join(",", out);
    }

    static String methodFlags(int f) {
        List<String> out = new ArrayList<>();
        if ((f & ACC_PUBLIC) != 0) out.add("public");
        if ((f & ACC_PROTECTED) != 0) out.add("protected");
        if ((f & ACC_PRIVATE) != 0) out.add("private");
        if ((f & ACC_STATIC) != 0) out.add("static");
        if ((f & ACC_FINAL) != 0) out.add("final");
        if ((f & ACC_SYNCHRONIZED) != 0) out.add("synchronized");
        if ((f & ACC_BRIDGE) != 0) out.add("bridge");
        if ((f & ACC_VARARGS) != 0) out.add("varargs");
        if ((f & ACC_NATIVE) != 0) out.add("native");
        if ((f & ACC_ABSTRACT) != 0) out.add("abstract");
        if ((f & ACC_STRICT) != 0) out.add("strictfp");
        if ((f & ACC_SYNTHETIC) != 0) out.add("synthetic");
        return String.join(",", out);
    }

    static String fieldFlags(int f) {
        List<String> out = new ArrayList<>();
        if ((f & ACC_PUBLIC) != 0) out.add("public");
        if ((f & ACC_PROTECTED) != 0) out.add("protected");
        if ((f & ACC_PRIVATE) != 0) out.add("private");
        if ((f & ACC_STATIC) != 0) out.add("static");
        if ((f & ACC_FINAL) != 0) out.add("final");
        if ((f & ACC_VOLATILE) != 0) out.add("volatile");
        if ((f & ACC_TRANSIENT) != 0) out.add("transient");
        if ((f & ACC_SYNTHETIC) != 0) out.add("synthetic");
        if ((f & ACC_ENUM) != 0) out.add("enum");
        return String.join(",", out);
    }

    private JdkBaseline() {}
}
