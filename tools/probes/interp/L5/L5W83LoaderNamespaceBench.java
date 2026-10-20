// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 83, lane L5 -- the loader-namespace lookups
// every user-loader `findLoadedClass0` and `defineClass1` makes
// (`docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 83)"). Without `CRATONVM_LOADER_NAMESPACE_INDEX` each
// lookup scans two process-global vectors (one row per live loader of every
// VM, under a process-wide mutex each) and walks the receiver's class
// hierarchy by field name to tell the real `ClassLoader` layout from the
// synthetic one. The rows run W81's four shapes twice: once with this
// bench's loaders the only user loaders ("lone-"), then with CROWD more live
// `URLClassLoader` subclasses, each of which defined one class, so each has a
// row in both vectors ("crowd-"):
//
//   find-loaded-miss    `findLoadedClass(name)` for names never loaded.
//   find-loaded-hit     the same after the loader defined every name.
//   define              a fresh loader per round, `defineClass` per name.
//   load-define         a fresh loader per round, `loadClass` per name (the
//                       platform delegation miss, `findClass`, the define).
//
// Without the switch the crowd- rows should be slower than the lone- rows
// (O(live loaders) per lookup); with it, both should sit at the lone- level
// or below.
//
// Every row reports `<row> ns/op=<median of rounds 1..ROUNDS-1>` and
// `<row> first ns/op=<round 0>`; per op = per class name.
//
// Run ($B the cratonvm binary, $J the JDK 25 bin directory; interleave the
// switch arms, five repetitions, medians):
//   $J/javac -d out L5W83LoaderNamespaceBench.java
//   env CRATONVM_NONE=1                   $B --java-home $J/.. -cp out L5W83LoaderNamespaceBench
//   env CRATONVM_LOADER_NAMESPACE_INDEX=1 $B --java-home $J/.. -cp out L5W83LoaderNamespaceBench
//   $J/java -cp out L5W83LoaderNamespaceBench
// (each also with `--compatible` and with `--nojit`). One row pair only:
// `L5W83LoaderNamespaceBench <row>` (both lone- and crowd- of it). No line
// without "ns/" may change with the switch.
//
// stdout lines without "ns/" are deterministic (the same on every VM):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// names-per-round=800
// lone-find-loaded-miss found=0
// lone-find-loaded-hit found=800
// lone-define defined=800
// lone-load-define loaded=800
// crowd-loaders=1000 defined=1000
// crowd-find-loaded-miss found=0
// crowd-find-loaded-hit found=800
// crowd-define defined=800
// crowd-load-define loaded=800
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

public class L5W83LoaderNamespaceBench {
    static final int NAMES = 800;
    static final int CROWD = 1000;
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
    /** The crowd: kept reachable for the whole crowd- half. */
    static final List<ClassLoader> CROWD_LOADERS = new ArrayList<>();

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

    static void rows(String prefix, String only) throws Exception {
        if (only == null || only.equals("find-loaded-miss")) {
            Probe probe = new Probe();
            row(prefix + "find-loaded-miss", "found", () -> () -> {
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
            row(prefix + "find-loaded-hit", "found", () -> () -> {
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
            row(prefix + "define", "defined", () -> {
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
            row(prefix + "load-define", "loaded", () -> {
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

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        for (int i = 0; i < NAMES; i++) {
            String name = "w83n.N" + i;
            NAMES_LIST.add(name);
            BYTES.put(name, classBytes(name.replace('.', '/')));
        }
        System.out.println("names-per-round=" + NAMES);

        rows("lone-", only);

        int defined = 0;
        for (int i = 0; i < CROWD; i++) {
            String name = "w83c.C" + i;
            BYTES.put(name, classBytes(name.replace('.', '/')));
            Probe member = new Probe();
            if (member.defineDirect(name).getClassLoader() == member) {
                defined++;
            }
            CROWD_LOADERS.add(member);
        }
        System.out.println("crowd-loaders=" + CROWD_LOADERS.size() + " defined=" + defined);

        rows("crowd-", only);
    }
}
