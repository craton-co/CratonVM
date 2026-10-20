// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 94, lane L5
// (`i87-L4-proposal-define-and-first-instance-cost`): does a define cost more
// in a VM that has seen many user loaders? HotSpot's dictionary is per loader,
// so a define does not depend on how many other loaders exist. The class
// manager's define asks `get_loaded_class_id(name)` for a stub to upgrade,
// which probes every user loader the VM has registered, for every define;
// `CRATONVM_DEFINE_STUB_PROBE_BY_NAME=1` asks the class store's name index
// first.
//
// Rows (ns per define, median over N fresh classes, each a tiny empty class
// built with java.lang.classfile, defined by ONE loader):
//   define-lone    before the crowd: only the bench's own loaders exist
//   define-crowd   after K other loaders (kept alive) each defined one class
//
// Output: "row <name> med <ns> ns/op" on stderr, "checksum <name> <n>" on
// stdout. Args: [n] [k] (defaults 400, 2000).
//
// Run: javac -d . L5W94DefineCrowdBench.java
//      java -cp . L5W94DefineCrowdBench
//      cratonvm --jdk-only -cp . L5W94DefineCrowdBench
//      CRATONVM_DEFINE_STUB_PROBE_BY_NAME=1 cratonvm --jdk-only -cp . L5W94DefineCrowdBench
//      (and the same two with --compatible). With the census:
//      CRATONVM_DBG_DEFINE_CENSUS=1 cratonvm --jdk-only -cp . L5W94DefineCrowdBench 2>census.txt
//      grep -E 'define-pre-checks|cm-write|class-manager-define|row ' census.txt
//
// HOTSPOT_EXPECTED_BEGIN (derived: every define succeeds)
// checksum define-lone 400
// checksum define-crowd 400
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

public class L5W94DefineCrowdBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W94DefineCrowdBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    static void row(String name, int n, boolean print) {
        Loader loader = new Loader();
        long[] t = new long[n];
        int ok = 0;
        for (int i = 0; i < n; i++) {
            String cls = "Dw" + (classes++);
            byte[] bytes = build(cls);
            long t0 = System.nanoTime();
            Class<?> c = loader.define(cls, bytes);
            t[i] = System.nanoTime() - t0;
            if (c.getName().equals(cls)) {
                ok++;
            }
        }
        if (print) {
            Arrays.sort(t);
            System.err.printf(Locale.ROOT, "row %s med %d ns/op%n", name, t[n / 2]);
            System.out.println("checksum " + name + " " + ok);
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int k = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        row("warm", 100, false);
        row("define-lone", n, true);
        List<Object> keep = new ArrayList<>();
        for (int i = 0; i < k; i++) {
            Loader other = new Loader();
            String cls = "Dx" + i;
            keep.add(other.define(cls, build(cls)));
            keep.add(other);
        }
        row("warm", 100, false);
        row("define-crowd", n, true);
        if (keep.size() != 2 * k) {
            throw new AssertionError();
        }
    }
}
