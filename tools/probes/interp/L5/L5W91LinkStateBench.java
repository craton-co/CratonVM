// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4, wave 91, lane L5 -- the link step of a class a user
// loader defined but has not linked: `Class.forName(name, true, loader)` on a
// class with no `<clinit>` (row `link-init`), and on a two-level chain whose
// superclass is not linked either (row `link-init-chain`: two links per op).
// The define is outside the timed region, so each op is the link + the empty
// initialization alone. Rows `hidden-init` (one `defineHiddenClass(b, true)`,
// define + link + init) for comparison with `L5W89HiddenInitBench`.
//
// The A/B is `CRATONVM_LINK_STATE_ONE_WRITE` (default off): a link moves the
// class Verifying -> Preparing under one class-manager write guard instead of
// two. With `CRATONVM_DBG_DEFINE_CENSUS=1` the census rows `link-prepare` and
// `write-guard-sweep` (the guard-release re-sweeps, count and total) show
// where the time goes.
//
// Run (n = 400 per row, after a warm-up pass of 60; timings on stderr):
//   $J/javac -d out L5W91LinkStateBench.java
//   $J/java -cp out L5W91LinkStateBench                                    # HotSpot reference
//   $B --java-home $J/.. -cp out L5W91LinkStateBench
//   CRATONVM_LINK_STATE_ONE_WRITE=1 $B --java-home $J/.. -cp out L5W91LinkStateBench
//   (each also with --compatible and --nojit). Phase split:
//   CRATONVM_DBG_DEFINE_CENSUS=1 $B --java-home $J/.. -cp out L5W91LinkStateBench 2>census.txt
//
// HOTSPOT_EXPECTED_BEGIN
// checksum link-init 400
// checksum link-init-chain 400
// checksum hidden-init 400
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L5W91LinkStateBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static int classes;

    static byte[] build(String name, ClassDesc superDesc) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(superDesc);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(superDesc, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    /** Defines what it is given; `forName` then links and initializes it. */
    static final class Definer extends ClassLoader {
        Definer() {
            super(L5W91LinkStateBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static void report(String label, long[] t, int ok, boolean print) {
        if (!print) {
            return;
        }
        long[] c = t.clone();
        Arrays.sort(c);
        System.err.printf(Locale.ROOT, "row %s min %d med %d ns/op%n", label, c[0], c[c.length / 2]);
        System.out.println("checksum " + label + " " + ok);
    }

    static void linkInit(int n, boolean print) throws Exception {
        long[] t = new long[n];
        int ok = 0;
        Definer loader = new Definer();
        for (int i = 0; i < n; i++) {
            String name = "l5w91.L" + (classes++);
            loader.define(name, build(name, ConstantDescs.CD_Object));
            long t0 = System.nanoTime();
            Class<?> c = Class.forName(name, true, loader);
            t[i] = System.nanoTime() - t0;
            if (c.getName().equals(name)) {
                ok++;
            }
        }
        report("link-init", t, ok, print);
    }

    static void linkInitChain(int n, boolean print) throws Exception {
        long[] t = new long[n];
        int ok = 0;
        Definer loader = new Definer();
        for (int i = 0; i < n; i++) {
            String base = "l5w91.A" + classes;
            String sub = "l5w91.B" + (classes++);
            loader.define(base, build(base, ConstantDescs.CD_Object));
            loader.define(sub, build(sub, ClassDesc.of(base)));
            long t0 = System.nanoTime();
            Class<?> c = Class.forName(sub, true, loader);
            t[i] = System.nanoTime() - t0;
            if (c.getSuperclass().getName().equals(base)) {
                ok++;
            }
        }
        report("link-init-chain", t, ok, print);
    }

    static void hiddenInit(int n, boolean print) throws Exception {
        long[] t = new long[n];
        int ok = 0;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (int i = 0; i < n; i++) {
            byte[] b = build("L5W91H" + (classes++), ConstantDescs.CD_Object);
            long t0 = System.nanoTime();
            boolean hidden = lookup.defineHiddenClass(b, true).lookupClass().isHidden();
            t[i] = System.nanoTime() - t0;
            if (hidden) {
                ok++;
            }
        }
        report("hidden-init", t, ok, print);
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        for (int pass = 0; pass < 2; pass++) {
            boolean print = pass == 1;
            int count = print ? n : 60;
            linkInit(count, print);
            linkInitChain(count, print);
            hiddenInit(count, print);
        }
    }
}
