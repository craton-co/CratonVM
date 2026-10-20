// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4, wave 88, lane L5 -- the define steps of
// `L4W87DefineClassSplitBench` that this lane owns, isolated:
//
//   define         ClassLoader.defineClass(name, b, 0, len) on a user loader
//   define-pd      the same with an explicit ProtectionDomain (null CodeSource)
//   lookup-define  MethodHandles.lookup().defineClass(bytes) (application loader)
//   hidden-define  lookup().defineHiddenClass(bytes, false) (no init)
//   hidden-init    lookup().defineHiddenClass(bytes, true)
//
// The A/B is `CRATONVM_DEFINE_NO_CLASS_PATH_CODE_SOURCE` (default off): a
// user-loader or hidden define without a code source skips the class-path
// probe for a class file of its name (two passes over every class-path entry
// of the three built-in class paths, a stat per directory on the second).
// The probe's cost grows with the class path's directory entries, so the
// rows are also run with a class path of 20 extra (empty) directories.
//
// Run (n = 300 defines per row, after a warm-up of 40):
//   $J/javac -d out L5W88DefineCodeSourceBench.java
//   $J/java -cp out L5W88DefineCodeSourceBench                                   # HotSpot reference
//   $B --java-home $J/.. -cp out L5W88DefineCodeSourceBench
//   CRATONVM_DEFINE_NO_CLASS_PATH_CODE_SOURCE=1 $B --java-home $J/.. -cp out L5W88DefineCodeSourceBench
//   CP=out; for i in $(seq 1 20); do mkdir -p d$i; CP="$CP:d$i"; done        (';' on Windows)
//   $B --java-home $J/.. -cp "$CP" L5W88DefineCodeSourceBench
//   CRATONVM_DEFINE_NO_CLASS_PATH_CODE_SOURCE=1 $B --java-home $J/.. -cp "$CP" L5W88DefineCodeSourceBench
// (each also with --compatible). The phase split of the same run:
//   CRATONVM_DBG_DEFINE_CENSUS=1 $B --java-home $J/.. -cp out L5W88DefineCodeSourceBench 2>census.txt
//
// HOTSPOT_EXPECTED_BEGIN
// checksum define 300
// checksum define-pd 300
// checksum lookup-define 300
// checksum hidden-define 300
// checksum hidden-init 300
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.security.ProtectionDomain;
import java.util.Arrays;
import java.util.Locale;

public class L5W88DefineCodeSourceBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W88DefineCodeSourceBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b, ProtectionDomain pd) {
            return pd == null ? defineClass(name, b, 0, b.length) : defineClass(name, b, 0, b.length, pd);
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

    interface Step {
        boolean run(Loader loader, String name, byte[] bytes) throws Throwable;
    }

    static void row(String label, int n, boolean print, Step step) throws Throwable {
        Loader loader = new Loader();
        long[] t = new long[n];
        int ok = 0;
        for (int i = 0; i < n; i++) {
            String name = "Ds" + (classes++);
            byte[] bytes = build(name);
            long t0 = System.nanoTime();
            if (step.run(loader, name, bytes)) {
                ok++;
            }
            t[i] = System.nanoTime() - t0;
        }
        if (print) {
            long[] c = t.clone();
            Arrays.sort(c);
            System.out.printf(Locale.ROOT, "row %s min %d med %d ns/op%n", label, c[0], c[n / 2]);
            System.out.println("checksum " + label + " " + ok);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        ProtectionDomain pd = new ProtectionDomain(null, null);
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (int pass = 0; pass < 2; pass++) {
            boolean print = pass == 1;
            int count = print ? n : 40;
            row("define", count, print, (l, c, b) -> l.define(c, b, null).getName().equals(c));
            row("define-pd", count, print, (l, c, b) -> l.define(c, b, pd).getName().equals(c));
            row("lookup-define", count, print, (l, c, b) -> lookup.defineClass(b).getName().equals(c));
            row("hidden-define", count, print,
                    (l, c, b) -> lookup.defineHiddenClass(b, false).lookupClass().isHidden());
            row("hidden-init", count, print,
                    (l, c, b) -> lookup.defineHiddenClass(b, true).lookupClass().isHidden());
        }
    }
}
