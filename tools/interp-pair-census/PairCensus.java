// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Offline bytecode pair/triple census for choosing interpreter
// superinstructions (interpreter round i1 wave 25, lane L7; proposal
// `docs/known-issues/interpreter/i1-L1-proposal-superinstruction-coverage-20260923.md`).
//
// CratonVM's own dynamic census (`CRATONVM_QUICKEN_STATS=pairs`, wave 4)
// counts the pairs its fast path dispatches separately, on the host. This tool
// answers the same question without a CratonVM build, on any JDK 25 (it uses
// the `java.lang.classfile` API), in two modes:
//
//   static  -- every fall-through pair / triple in the methods of the given
//              class roots (`jrt:/java.base` by default, plus directories),
//              with a loop-weighted column (an instruction inside a backward
//              branch's range counts 8x per enclosing loop level):
//                java tools/interp-pair-census/PairCensus.java static [--top N] [jrt:java.base] [<dir>...]
//
//   run     -- DYNAMIC counts: loads <MainClass> and every class of <dir> through
//              a loader that inserts a counter after each instruction that
//              falls through, runs its main, and prints the fall-through pairs
//              and triples actually executed. JDK classes are not instrumented,
//              so run a benchmark whose hot loop is in its own classes:
//                java tools/interp-pair-census/PairCensus.java run [--top N] <dir> <MainClass> [args...]
//
// Pairs are counted on the RAW opcodes the interpreter's fast path dispatches
// (`iload_1` and `iload 5` are different rows), and again by FAMILY (`iload`),
// which is the level a fusion is designed at. A triple is counted only when
// its middle instruction is not a branch target (it can only be reached by
// falling through from the first).
import java.io.IOException;
import java.io.PrintStream;
import java.lang.classfile.ClassFile;
import java.lang.classfile.ClassHierarchyResolver;
import java.lang.classfile.ClassModel;
import java.lang.classfile.ClassTransform;
import java.lang.classfile.CodeElement;
import java.lang.classfile.CodeModel;
import java.lang.classfile.CodeTransform;
import java.lang.classfile.Instruction;
import java.lang.classfile.Label;
import java.lang.classfile.MethodModel;
import java.lang.classfile.Opcode;
import java.lang.classfile.PseudoInstruction;
import java.lang.classfile.instruction.BranchInstruction;
import java.lang.classfile.instruction.LabelTarget;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;
import java.net.URI;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Stream;

public class PairCensus {
    // ---------------------------------------------------------------- counts

    /** Dynamic mode: one slot per instrumented site. */
    static final int MAX_SITES = 1 << 20;
    static final long[] HITS = new long[MAX_SITES];
    /** Site -> packed key `(first << 16) | (second << 8) | third`, third = 0xff for none. */
    static final int[] SITE_KEY = new int[MAX_SITES];
    static int sites;

    /** Called by instrumented code. Races only undercount. */
    public static void hit(int site) {
        HITS[site]++;
    }

    static final class Table {
        final Map<String, long[]> rows = new HashMap<>();

        void add(String key, long count, long weighted) {
            long[] r = rows.computeIfAbsent(key, k -> new long[2]);
            r[0] += count;
            r[1] += weighted;
        }

        void print(PrintStream out, String title, int top, int column) {
            long total = 0;
            for (long[] r : rows.values()) total += r[column];
            out.printf("== %s (total %d)%n", title, total);
            List<Map.Entry<String, long[]>> list = new ArrayList<>(rows.entrySet());
            list.sort((a, b) -> {
                int c = Long.compare(b.getValue()[column], a.getValue()[column]);
                return c != 0 ? c : a.getKey().compareTo(b.getKey());
            });
            for (int i = 0; i < Math.min(top, list.size()); i++) {
                long v = list.get(i).getValue()[column];
                out.printf("  #%-3d %-44s %14d %6.2f%%%n", i + 1, list.get(i).getKey(), v,
                        total == 0 ? 0.0 : v * 100.0 / total);
            }
        }
    }

    // ---------------------------------------------------------------- opcodes

    static int base(Opcode op) {
        return op.bytecode() & 0xff;
    }

    static boolean fallsThrough(Opcode op) {
        return switch (op.kind()) {
            case RETURN, THROW_EXCEPTION, TABLE_SWITCH, LOOKUP_SWITCH -> false;
            case BRANCH -> op != Opcode.GOTO && op != Opcode.GOTO_W;
            case DISCONTINUED_JSR, DISCONTINUED_RET -> false;
            default -> true;
        };
    }

    static final Map<Integer, String> NAMES = new HashMap<>();

    static String mnemonic(int op) {
        if (NAMES.isEmpty()) {
            for (Opcode o : Opcode.values()) {
                if (o.bytecode() <= 0xff) NAMES.put(o.bytecode(), o.name().toLowerCase());
            }
        }
        return NAMES.getOrDefault(op, "op" + op);
    }

    /** The family a fusion is designed at: slot and constant spellings merged. */
    static String family(int op) {
        String n = mnemonic(op);
        if (op >= 0x02 && op <= 0x08 || op == 0x10 || op == 0x11) return "iconst";
        if (op == 0x12 || op == 0x13 || op == 0x14) return "ldc";
        int u = n.indexOf('_');
        if (u > 0 && (n.startsWith("iload") || n.startsWith("lload") || n.startsWith("fload")
                || n.startsWith("dload") || n.startsWith("aload") || n.startsWith("istore")
                || n.startsWith("lstore") || n.startsWith("fstore") || n.startsWith("dstore")
                || n.startsWith("astore") || n.startsWith("lconst") || n.startsWith("fconst")
                || n.startsWith("dconst"))) {
            return n.substring(0, u);
        }
        return n;
    }

    // ---------------------------------------------------------------- static

    static void staticCensus(List<Path> roots, int top) throws IOException {
        Table rawPairs = new Table(), famPairs = new Table(), famTriples = new Table();
        long[] methods = new long[1];
        for (Path root : roots) {
            try (Stream<Path> s = Files.walk(root)) {
                for (Path p : (Iterable<Path>) s.filter(f -> f.toString().endsWith(".class"))::iterator) {
                    ClassModel cm;
                    try {
                        cm = ClassFile.of().parse(Files.readAllBytes(p));
                    } catch (IllegalArgumentException e) {
                        continue;
                    }
                    for (MethodModel mm : cm.methods()) {
                        mm.code().ifPresent(code -> {
                            methods[0]++;
                            staticMethod(code, rawPairs, famPairs, famTriples);
                        });
                    }
                }
            }
        }
        PrintStream out = System.out;
        out.printf("static census: %d methods%n", methods[0]);
        famPairs.print(out, "fall-through pairs by family, loop-weighted", top, 1);
        famPairs.print(out, "fall-through pairs by family, static", top, 0);
        rawPairs.print(out, "fall-through pairs, raw opcodes, loop-weighted", top, 1);
        famTriples.print(out, "fall-through triples by family, loop-weighted", top, 1);
    }

    static void staticMethod(CodeModel code, Table rawPairs, Table famPairs, Table famTriples) {
        List<CodeElement> els = code.elementList();
        // Instruction positions, and which are branch targets.
        List<Instruction> ins = new ArrayList<>();
        List<Boolean> target = new ArrayList<>();
        Map<Label, Integer> labelAt = new HashMap<>();
        boolean pendingLabel = false;
        for (CodeElement e : els) {
            if (e instanceof LabelTarget lt) {
                labelAt.put(lt.label(), ins.size());
                pendingLabel = true;
            } else if (e instanceof Instruction i) {
                ins.add(i);
                target.add(pendingLabel);
                pendingLabel = false;
            }
        }
        // Loop depth per instruction: a backward branch at k to t covers [t, k].
        int[] depth = new int[ins.size()];
        for (int k = 0; k < ins.size(); k++) {
            if (ins.get(k) instanceof BranchInstruction b) {
                Integer t = labelAt.get(b.target());
                if (t != null && t <= k) {
                    for (int j = t; j <= k; j++) depth[j]++;
                }
            }
        }
        for (int k = 0; k + 1 < ins.size(); k++) {
            Opcode a = ins.get(k).opcode();
            if (!fallsThrough(a)) continue;
            int b = base(ins.get(k + 1).opcode());
            long w = (long) Math.pow(8, Math.min(depth[k + 1], 4));
            rawPairs.add(mnemonic(base(a)) + " -> " + mnemonic(b), 1, w);
            famPairs.add(family(base(a)) + " -> " + family(b), 1, w);
            if (k + 2 < ins.size() && fallsThrough(ins.get(k + 1).opcode()) && !target.get(k + 1)) {
                int c = base(ins.get(k + 2).opcode());
                long w3 = (long) Math.pow(8, Math.min(depth[k + 2], 4));
                famTriples.add(family(base(a)) + " -> " + family(b) + " -> " + family(c), 1, w3);
            }
        }
    }

    // ---------------------------------------------------------------- dynamic

    static final ClassDesc SELF = ClassDesc.of("PairCensus");
    static final MethodTypeDesc HIT = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int);

    static synchronized int newSite(int first, int second, int third) {
        if (sites >= MAX_SITES) return -1;
        SITE_KEY[sites] = (first << 16) | (second << 8) | third;
        return sites++;
    }

    static byte[] instrument(byte[] bytes, ClassLoader loader) {
        ClassFile cf = ClassFile.of(ClassFile.ClassHierarchyResolverOption.of(
                ClassHierarchyResolver.defaultResolver().orElse(ClassHierarchyResolver.ofResourceParsing(loader))));
        ClassModel cm = cf.parse(bytes);
        return cf.transformClass(cm, ClassTransform.transformingMethods((mb, me) -> {
            if (!(me instanceof CodeModel code)) {
                mb.with(me);
                return;
            }
            // Pre-scan: per instruction, the site to count after it (or -1).
            List<CodeElement> els = code.elementList();
            List<Instruction> ins = new ArrayList<>();
            List<Boolean> target = new ArrayList<>();
            boolean pendingLabel = false;
            for (CodeElement e : els) {
                if (e instanceof LabelTarget) {
                    pendingLabel = true;
                } else if (e instanceof Instruction i) {
                    ins.add(i);
                    target.add(pendingLabel);
                    pendingLabel = false;
                }
            }
            int[] siteAfter = new int[ins.size()];
            for (int k = 0; k < ins.size(); k++) {
                siteAfter[k] = -1;
                if (k + 1 >= ins.size() || !fallsThrough(ins.get(k).opcode())) continue;
                int prev = 0xff;
                if (k > 0 && fallsThrough(ins.get(k - 1).opcode()) && !target.get(k)) {
                    prev = base(ins.get(k - 1).opcode());
                }
                // Key: (prev, this, next) -- prev = 0xff when this is a branch target.
                siteAfter[k] = newSite(prev, base(ins.get(k).opcode()), base(ins.get(k + 1).opcode()));
            }
            int[] n = {0};
            mb.transformCode(code, (cb, e) -> {
                cb.with(e);
                if (e instanceof Instruction && !(e instanceof PseudoInstruction)) {
                    int site = siteAfter[n[0]++];
                    if (site >= 0) {
                        cb.loadConstant(site);
                        cb.invokestatic(SELF, "hit", HIT);
                    }
                }
            });
        }));
    }

    static final class Loader extends URLClassLoader {
        final Path dir;

        Loader(Path dir) throws IOException {
            super(new URL[] {dir.toUri().toURL()}, PairCensus.class.getClassLoader());
            this.dir = dir;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    Path p = dir.resolve(name.replace('.', '/') + ".class");
                    if (!name.equals("PairCensus") && Files.exists(p)) {
                        try {
                            byte[] b = instrument(Files.readAllBytes(p), this);
                            c = defineClass(name, b, 0, b.length);
                        } catch (IOException e) {
                            throw new ClassNotFoundException(name, e);
                        }
                    } else {
                        c = super.loadClass(name, false);
                    }
                }
                if (resolve) resolveClass(c);
                return c;
            }
        }
    }

    static void report(int top) {
        Table rawPairs = new Table(), famPairs = new Table(), famTriples = new Table(), rawTriples = new Table();
        for (int s = 0; s < sites; s++) {
            long h = HITS[s];
            if (h == 0) continue;
            int key = SITE_KEY[s];
            int a = (key >> 16) & 0xff, b = (key >> 8) & 0xff, c = key & 0xff;
            rawPairs.add(mnemonic(b) + " -> " + mnemonic(c), h, h);
            famPairs.add(family(b) + " -> " + family(c), h, h);
            if (a != 0xff) {
                famTriples.add(family(a) + " -> " + family(b) + " -> " + family(c), h, h);
                rawTriples.add(mnemonic(a) + " -> " + mnemonic(b) + " -> " + mnemonic(c), h, h);
            }
        }
        PrintStream out = System.err;
        out.printf("dynamic census: %d instrumented sites%n", sites);
        famPairs.print(out, "executed fall-through pairs by family", top, 0);
        rawPairs.print(out, "executed fall-through pairs, raw opcodes", top, 0);
        famTriples.print(out, "executed fall-through triples by family", top, 0);
        rawTriples.print(out, "executed fall-through triples, raw opcodes", top, 0);
    }

    // ---------------------------------------------------------------- main

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.err.println("usage: PairCensus static [--top N] [jrt:java.base] [<dir>...]\n"
                    + "       PairCensus run [--top N] <dir> <MainClass> [args...]");
            System.exit(2);
        }
        String mode = args[0];
        int i = 1;
        int top = 20;
        if (i + 1 < args.length && args[i].equals("--top")) {
            top = Integer.parseInt(args[i + 1]);
            i += 2;
        }
        if (mode.equals("static")) {
            List<Path> roots = new ArrayList<>();
            for (; i < args.length; i++) {
                if (args[i].startsWith("jrt:")) {
                    FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
                    roots.add(jrt.getPath("/modules/" + args[i].substring(4)));
                } else {
                    roots.add(Path.of(args[i]));
                }
            }
            if (roots.isEmpty()) {
                roots.add(FileSystems.getFileSystem(URI.create("jrt:/")).getPath("/modules/java.base"));
            }
            staticCensus(roots, top);
        } else if (mode.equals("run")) {
            Path dir = Path.of(args[i]);
            String main = args[i + 1];
            String[] rest = java.util.Arrays.copyOfRange(args, i + 2, args.length);
            final int t = top;
            Runtime.getRuntime().addShutdownHook(new Thread(() -> report(t)));
            Loader loader = new Loader(dir);
            Method m = loader.loadClass(main).getMethod("main", String[].class);
            m.invoke(null, (Object) rest);
        } else {
            System.err.println("unknown mode " + mode);
            System.exit(2);
        }
    }
}
