// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 81, lane L5 -- the loader predicates every
// class-loader native asks per call
// (`docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 81)"). `findLoadedClass0`, `defineClass1` and
// `getResource` each ask "is this receiver a user-defined loader / a bare
// `URLClassLoader`" several times per call; without
// `CRATONVM_LOADER_KIND_MEMO` every ask is a by-name `java/lang/ClassLoader`
// lookup, a superclass walk and a copy of the receiver class's name. The rows
// isolate the natives that ask, with no jar and no delegation miss:
//
//   find-loaded-miss    `findLoadedClass(name)` of a `URLClassLoader`
//                       subclass, for names it never loaded (the ask every
//                       `loadClass` makes first).
//   find-loaded-hit     the same loader after it defined every name: the
//                       namespace hit.
//   define              a fresh `URLClassLoader` subclass per round calls the
//                       protected `defineClass(name, b, 0, len)` for every
//                       class (no delegation, no lookup): `defineClass1`.
//   load-define         that loader's `loadClass`: the platform delegation
//                       miss, `findClass` and the define (W72's
//                       `mem-load-plain` shape, smaller classes).
//
// Every row reports `<row> ns/op=<median of rounds 1..ROUNDS-1>` and
// `<row> first ns/op=<round 0>`; per op = per class name.
//
// Run ($B the cratonvm binary, $J the JDK 25 bin directory; interleave the
// switch arms, five repetitions, medians):
//   $J/javac -d out L5W81LoaderKindBench.java
//   env CRATONVM_NONE=1             $B --java-home $J/.. -cp out L5W81LoaderKindBench
//   env CRATONVM_LOADER_KIND_MEMO=1 $B --java-home $J/.. -cp out L5W81LoaderKindBench
//   $J/java -cp out L5W81LoaderKindBench
// (each also with `--compatible` and with `--nojit`). One row only:
// `L5W81LoaderKindBench <row>`. The switch should lower every row by the same
// few asks' worth (find-loaded-* the most in relative terms) and change no
// line without "ns/".
//
// stdout lines without "ns/" are deterministic (the same on every VM):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// names-per-round=800
// find-loaded-miss found=0
// find-loaded-hit found=800
// define defined=800
// load-define loaded=800
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L5W81LoaderKindBench {
    static final int NAMES = 800;
    static final int ROUNDS = 7;

    /** `public class <name> {}` (version 49, no members). */
    static byte[] classBytes(String internal) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(5);
        out.writeByte(1); out.writeUTF(internal);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF("java/lang/Object");
        out.writeByte(7); out.writeShort(3);
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    static final List<String> NAMES_LIST = new ArrayList<>();
    static final Map<String, byte[]> BYTES = new HashMap<>();

    /** A `URLClassLoader` subclass (parent: platform) with the classes' bytes in memory. */
    static final class Probe extends URLClassLoader {
        Probe() {
            super(new URL[0], ClassLoader.getPlatformClassLoader());
        }

        Class<?> loaded(String name) {
            return findLoadedClass(name);
        }

        Class<?> defineDirect(String name) {
            byte[] b = BYTES.get(name);
            return defineClass(name, b, 0, b.length);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = BYTES.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    interface Round {
        long run() throws Exception;
    }

    interface RoundFactory {
        Round make() throws Exception;
    }

    static void row(String label, String countLabel, RoundFactory factory) throws Exception {
        long[] perOp = new long[ROUNDS];
        long count = 0;
        for (int r = 0; r < ROUNDS; r++) {
            Round round = factory.make();
            long t0 = System.nanoTime();
            count = round.run();
            perOp[r] = (System.nanoTime() - t0) / NAMES;
        }
        long[] later = Arrays.copyOfRange(perOp, 1, ROUNDS);
        Arrays.sort(later);
        System.out.println(label + " " + countLabel + "=" + count);
        System.out.println(label + " first ns/op=" + perOp[0]);
        System.out.println(label + " ns/op=" + later[later.length / 2]);
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        for (int i = 0; i < NAMES; i++) {
            String name = "w81k.K" + i;
            NAMES_LIST.add(name);
            BYTES.put(name, classBytes(name.replace('.', '/')));
        }
        System.out.println("names-per-round=" + NAMES);

        if (only == null || only.equals("find-loaded-miss")) {
            Probe probe = new Probe();
            row("find-loaded-miss", "found", () -> () -> {
                long n = 0;
                for (String name : NAMES_LIST) {
                    if (probe.loaded(name) != null) {
                        n++;
                    }
                }
                return n;
            });
        }
        if (only == null || only.equals("find-loaded-hit")) {
            Probe probe = new Probe();
            for (String name : NAMES_LIST) {
                probe.defineDirect(name);
            }
            row("find-loaded-hit", "found", () -> () -> {
                long n = 0;
                for (String name : NAMES_LIST) {
                    if (probe.loaded(name) != null) {
                        n++;
                    }
                }
                return n;
            });
        }
        if (only == null || only.equals("define")) {
            row("define", "defined", () -> {
                Probe probe = new Probe();
                return () -> {
                    long n = 0;
                    for (String name : NAMES_LIST) {
                        if (probe.defineDirect(name).getClassLoader() == probe) {
                            n++;
                        }
                    }
                    return n;
                };
            });
        }
        if (only == null || only.equals("load-define")) {
            row("load-define", "loaded", () -> {
                Probe probe = new Probe();
                return () -> {
                    long n = 0;
                    for (String name : NAMES_LIST) {
                        if (probe.loadClass(name).getClassLoader() == probe) {
                            n++;
                        }
                    }
                    return n;
                };
            });
        }
    }
}
