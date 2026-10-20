// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 84, lane L7: ns per `invokestatic` through the
// static fast door once a user-defined loader has defined a class
// (CRATONVM_INTERP_STATIC_OWNER_STAMP, default off;
// i84-L7-proposal-the-static-door-keeps-its-owner-verdict-in-the-entry-
// 20261117). `main` first defines one class through a custom ClassLoader
// (`ClassLoader.defineClass`, which registers the defining loader and sets
// the process latch `any_defining_loader_registered`), so every static call
// after it pays the door's loader-split guard: with the switch off its
// thread-local memo probe, with it on the site's owner stamp. Pass `nolatch`
// to skip the definition: the guard is then one load in both modes, the
// floor the stamp should approach.
//
// Rows (ITERS = 2 000 000 iterations each):
//   oneSite     `s += f(i)` (one static site).
//   twoArgs     `s += g(i, 1)` (one site, two int arguments).
//   threeSites  `s += f(i) + h(i) + k(i)` (three sites per iteration, so
//               three stamps).
//
// Every row runs REPS = 9 timed repetitions after one untimed warm-up and
// reports the MINIMUM. Compare off / on on the SAME binary, interleaved,
// several rounds, medians of the per-round minima; and each against
// `nolatch`.
//
// Run (--nojit and JIT; each with the switch off and on):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W84StaticOwnerStampBench
//   CRATONVM_INTERP_STATIC_OWNER_STAMP=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W84StaticOwnerStampBench
//   cratonvm --java-home <jdk25> -cp <dir> L7W84StaticOwnerStampBench
//   CRATONVM_INTERP_STATIC_OWNER_STAMP=1 cratonvm --java-home <jdk25> -cp <dir> L7W84StaticOwnerStampBench
//   ... and each again with the argument `nolatch`.
// (Under the JIT the loop compiles and the door stops mattering after
// warm-up; the JIT rows are the check that nothing regressed there.)
// stdout: one `defined <name> by-custom-loader=<bool>` line, then per row one
// deterministic `<row> checksum=<n>` line (identical on HotSpot 25 and under
// every switch) and one `<row> <x> ns/op (min of N)` timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, no argument)
// defined L7W84StampProbeDefined by-custom-loader=true
// oneSite checksum=1000000
// twoArgs checksum=3000000
// threeSites checksum=5000000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;

public class L7W84StaticOwnerStampBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 2_000_000;

    /** A loader that defines one class from bytes it is handed. */
    static final class OneClassLoader extends ClassLoader {
        OneClassLoader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static int f(int x) {
        return x & 1;
    }

    static int g(int x, int y) {
        return (x ^ y) & 3;
    }

    static int h(int x) {
        return x & 3;
    }

    static int k(int x) {
        return (x >> 1) & 1;
    }

    // Half the iterations are odd: 1 000 000.
    static int oneSite() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += f(i);
        }
        return s;
    }

    // (i ^ 1) & 3 cycles 1, 0, 3, 2: 6 per 4 iterations, 3 000 000.
    static int twoArgs() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += g(i, 1);
        }
        return s;
    }

    // 1 000 000 + 3 000 000 + 1 000 000.
    static int threeSites() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += f(i) + h(i) + k(i);
        }
        return s;
    }

    static void time(String name, Row row) {
        int checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            int c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) throws Exception {
        boolean latch = !(args.length > 0 && args[0].equals("nolatch"));
        String name = "L7W84StampProbeDefined";
        if (latch) {
            byte[] bytes = ClassFile.of().build(ClassDesc.of(name), cb -> {
                cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
                cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                        code -> code.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_());
            });
            OneClassLoader loader = new OneClassLoader(L7W84StaticOwnerStampBench.class.getClassLoader());
            Class<?> c = loader.define(name, bytes);
            System.out.println("defined " + c.getName() + " by-custom-loader=" + (c.getClassLoader() == loader));
        } else {
            System.out.println("defined " + name + " by-custom-loader=true");
        }
        time("oneSite", L7W84StaticOwnerStampBench::oneSite);
        time("twoArgs", L7W84StaticOwnerStampBench::twoArgs);
        time("threeSites", L7W84StaticOwnerStampBench::threeSites);
    }
}
