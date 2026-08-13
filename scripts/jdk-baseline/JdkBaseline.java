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

    /** Bumped only when the row grammar changes. A consumer must reject a version it does not know. */
    static final String FORMAT_VERSION = "1";

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

        TreeSet<String> members = new TreeSet<>();
        int publicMethods = 0;
        int protectedMethods = 0;
        for (MethodModel m : cm.methods()) {
            int f = m.flags().flagsMask();
            if ((f & (ACC_PUBLIC | ACC_PROTECTED)) == 0) {
                continue;
            }
            String name = m.methodName().stringValue();
            members.add(row("METHOD", name, m.methodType().stringValue(), methodFlags(f)));
            if (!name.equals("<init>") && !name.equals("<clinit>")) {
                if ((f & ACC_PUBLIC) != 0) {
                    publicMethods++;
                } else {
                    protectedMethods++;
                }
            }
        }
        TreeSet<String> fields = new TreeSet<>();
        for (FieldModel f : cm.fields()) {
            int fl = f.flags().flagsMask();
            if ((fl & (ACC_PUBLIC | ACC_PROTECTED)) == 0) {
                continue;
            }
            fields.add(row("FIELD", f.fieldName().stringValue(),
                    f.fieldType().stringValue(), fieldFlags(fl)));
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
