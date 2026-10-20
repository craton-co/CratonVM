// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4, wave 89, lane L5 -- `defineHiddenClass(b, true)`
// split: the define alone, the define with initialization, and an
// initialization of a hidden class defined without it (`Class.forName` on a
// hidden class is impossible, so `ensureInitialized` through a full-privilege
// lookup on the hidden class). The A/B is `CRATONVM_LINK_SKIPS_HIDDEN_PACKAGE`
// (default off: a hidden class's link pays the application class-path
// package step under a bogus per-class package key).
//
// Run (n = 300 per row, after a warm-up of 40):
//   $J/javac -d out L5W89HiddenInitBench.java
//   $J/java -cp out L5W89HiddenInitBench                                        # HotSpot reference
//   $B --java-home $J/.. -cp out L5W89HiddenInitBench
//   CRATONVM_LINK_SKIPS_HIDDEN_PACKAGE=1 $B --java-home $J/.. -cp out L5W89HiddenInitBench
//   (each also with --compatible). Phase split:
//   CRATONVM_DBG_DEFINE_CENSUS=1 $B --java-home $J/.. -cp out L5W89HiddenInitBench 2>census.txt
//
// HOTSPOT_EXPECTED_BEGIN
// checksum hidden-define 300
// checksum hidden-init 300
// checksum hidden-ensure-initialized 300
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L5W89HiddenInitBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
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
        boolean run(byte[] bytes) throws Throwable;
    }

    static void row(String label, int n, boolean print, Step step) throws Throwable {
        long[] t = new long[n];
        int ok = 0;
        for (int i = 0; i < n; i++) {
            byte[] bytes = build("Hi" + (classes++));
            long t0 = System.nanoTime();
            if (step.run(bytes)) {
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
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (int pass = 0; pass < 2; pass++) {
            boolean print = pass == 1;
            int count = print ? n : 40;
            row("hidden-define", count, print, b -> lookup.defineHiddenClass(b, false).lookupClass().isHidden());
            row("hidden-init", count, print, b -> lookup.defineHiddenClass(b, true).lookupClass().isHidden());
            row("hidden-ensure-initialized", count, print, b -> {
                MethodHandles.Lookup h = lookup.defineHiddenClass(b, false);
                return h.ensureInitialized(h.lookupClass()) == h.lookupClass();
            });
        }
    }
}
