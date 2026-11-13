// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 143, lane L5 (pages
// docs/feature-designs/class-resolution-and-caching.md,
// i66-L5-proposal-reuse-the-class-path-parse-of-a-fat-jar-20261030-RETIRED-20261101.md,
// i73-L5-proposal-user-loader-get-resource-without-the-native-hop-20261106-RETIRED-20261102.md):
// a jar of N generated classes, loaded and initialized through fresh
// URLClassLoaders (parent: the application loader), one at a time and four in
// parallel, plus each class file read back as a resource through the loader.
// One step: the probe writes its own jar into a temp directory.
//
// The deterministic lines are compared with HotSpot; the TIME rows are the A/B
// (ns per class, medians over the later rounds):
//   TIME jar-first-round <ns/class>     round 0 (cold loader, cold JDK paths)
//   TIME jar-later-rounds <ns/class>    median of rounds 1..R-1
//   TIME jar-parallel-4 <ns/class>      four threads, a loader each, wall / (4*N)
//   TIME jar-resource-read <ns/class>   getResourceAsStream(..).readAllBytes()
//
// Switches it is for: CRATONVM_DEFINING_LOADER_SHARED_READS (wave 143,
// jar-parallel-4 mostly), CRATONVM_DEFINE_ADD_CLASS_HINTED,
// CRATONVM_DEFINE_PD_URL_MEMO (define rows), and
// CRATONVM_USER_LOADER_RESOURCE_BYTECODE(_COMPILED) (jar-resource-read).
//
// Run: javac -d out L5W143ClassPathJarLoadBench.java
//      java -cp out L5W143ClassPathJarLoadBench                       (HotSpot)
//      cratonvm --real-jdk|--compatible [--nojit] -cp out L5W143ClassPathJarLoadBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, MEASURED 2026-10-21 by the orchestrator; default flags and -Xint identical)
// classes=400 rounds=7
// sum=79800
// loader-ok=true
// package=p143 package-loader-ok=true
// resource-bytes-match=true
// parallel-loaded=1600 parallel-sum=319200
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

public class L5W143ClassPathJarLoadBench {
    static final int N = 400;
    static final int ROUNDS = 7;

    static byte[] gen(int i) {
        ClassDesc self = ClassDesc.of("p143.C" + i);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("V", ConstantDescs.CD_int, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    code -> code.ldc(i).putstatic(self, "V", ConstantDescs.CD_int).return_());
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            cb.withMethodBody("get", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.getstatic(self, "V", ConstantDescs.CD_int).ireturn());
        });
    }

    static byte[][] BYTES = new byte[N][];

    static Path makeJar() throws Exception {
        Path dir = Files.createTempDirectory("l5w143");
        Path jar = dir.resolve("classes.jar");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar))) {
            for (int i = 0; i < N; i++) {
                BYTES[i] = gen(i);
                out.putNextEntry(new JarEntry("p143/C" + i + ".class"));
                out.write(BYTES[i]);
                out.closeEntry();
            }
        }
        // Deleted in reverse order of registration: the jar, then its directory.
        dir.toFile().deleteOnExit();
        jar.toFile().deleteOnExit();
        return jar;
    }

    /** Loads and initializes every class through `l`; the sum of their `V`s. */
    static long loadAll(ClassLoader l) throws Exception {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            Class<?> c = Class.forName("p143.C" + i, true, l);
            sum += c.getField("V").getInt(null);
        }
        return sum;
    }

    static long median(long[] v) {
        long[] s = v.clone();
        Arrays.sort(s);
        return s[s.length / 2];
    }

    public static void main(String[] args) throws Exception {
        Path jar = makeJar();
        URL[] urls = {jar.toUri().toURL()};
        ClassLoader app = L5W143ClassPathJarLoadBench.class.getClassLoader();
        System.out.println("classes=" + N + " rounds=" + ROUNDS);

        long[] ns = new long[ROUNDS];
        long sum = 0;
        boolean loaderOk = true;
        String pkg = null;
        boolean pkgLoaderOk = true;
        for (int r = 0; r < ROUNDS; r++) {
            try (URLClassLoader l = new URLClassLoader(urls, app)) {
                long t0 = System.nanoTime();
                long s = loadAll(l);
                ns[r] = (System.nanoTime() - t0) / N;
                if (r == 0) {
                    sum = s;
                    Class<?> c = l.loadClass("p143.C7");
                    loaderOk = c.getClassLoader() == l;
                    Package p = c.getPackage();
                    pkg = p.getName();
                    pkgLoaderOk = l.getDefinedPackage("p143") == p;
                } else if (s != sum) {
                    loaderOk = false;
                }
            }
        }
        System.out.println("sum=" + sum);
        System.out.println("loader-ok=" + loaderOk);
        System.out.println("package=" + pkg + " package-loader-ok=" + pkgLoaderOk);

        boolean match = true;
        long resNs;
        try (URLClassLoader l = new URLClassLoader(urls, app)) {
            long[] rr = new long[ROUNDS];
            for (int r = 0; r < ROUNDS; r++) {
                long t0 = System.nanoTime();
                for (int i = 0; i < N; i++) {
                    try (InputStream in = l.getResourceAsStream("p143/C" + i + ".class")) {
                        byte[] got = in.readAllBytes();
                        if (r == 0 && !Arrays.equals(got, BYTES[i])) {
                            match = false;
                        }
                    }
                }
                rr[r] = (System.nanoTime() - t0) / N;
            }
            resNs = median(Arrays.copyOfRange(rr, 1, ROUNDS));
        }
        System.out.println("resource-bytes-match=" + match);

        long[] parSums = new long[4];
        Thread[] ts = new Thread[4];
        long p0 = System.nanoTime();
        for (int t = 0; t < 4; t++) {
            final int k = t;
            ts[t] = new Thread(() -> {
                try (URLClassLoader l = new URLClassLoader(urls, app)) {
                    parSums[k] = loadAll(l);
                } catch (Exception e) {
                    parSums[k] = -1;
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long parNs = (System.nanoTime() - p0) / (4L * N);
        long parSum = 0;
        int parLoaded = 0;
        for (long s : parSums) {
            if (s == sum) {
                parLoaded += N;
            }
            parSum += s;
        }
        System.out.println("parallel-loaded=" + parLoaded + " parallel-sum=" + parSum);

        System.out.println("TIME jar-first-round " + ns[0] + " ns/class");
        System.out.println("TIME jar-later-rounds " + median(Arrays.copyOfRange(ns, 1, ROUNDS)) + " ns/class");
        System.out.println("TIME jar-parallel-4 " + parNs + " ns/class");
        System.out.println("TIME jar-resource-read " + resNs + " ns/class");
    }
}
