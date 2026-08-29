import java.io.*;
import java.lang.management.ManagementFactory;
import java.lang.management.RuntimeMXBean;
import java.lang.reflect.Method;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.*;
import java.security.spec.KeySpec;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import javax.crypto.*;
import javax.crypto.spec.*;

/**
 * The five platform surfaces the JDK-only breadth probes deliberately did not
 * cover: {@code ProcessBuilder}, security providers, virtual threads,
 * agents/attach, and JNI.
 *
 * <p>Written to the rules in {@code docs/feature-designs/jdk-only-wave2/
 * L8-strict-corpus-green.md}:
 *
 * <ul>
 *   <li><b>Every blocking call is bounded.</b> {@code Process.waitFor(n, unit)},
 *       {@code Thread.join(ms)}, {@code Future.get(n, unit)},
 *       {@code ExecutorService.awaitTermination(n, unit)}. A section that can
 *       hang cannot be run in CI.
 *   <li><b>Print values, not {@code ok}.</b> Every line carries the numbers and
 *       digests it computed, so a wrong answer diffs against the HotSpot
 *       control instead of reading as a pass.
 *   <li><b>Nothing host-, clock- or PID-dependent is printed.</b> The whole
 *       transcript has to be byte-comparable between HotSpot and both CratonVM
 *       policies on the same machine, so pids, temp paths, provider versions,
 *       thread counts and elapsed times are reduced to booleans or omitted.
 *   <li><b>Tally at the end</b> so a truncated run is visible.
 * </ul>
 *
 * <p>Two sections need a fixture that only the runner can build, and both say
 * so in their own output rather than silently passing:
 *
 * <ul>
 *   <li>{@code -Dcraton.probe.jnilib=<path>} — the shared object built from
 *       {@code probes/jdkonly_jni_probe.c}. Absent: prints {@code lib=absent}.
 *   <li>{@code -javaagent:<jar>} built from {@code probes/JdkOnlyProbeAgent.java}.
 *       Absent: prints {@code agent=absent}.
 * </ul>
 *
 * {@code scripts/jdk-only-strict-probes.sh} builds both and passes them to all
 * three arms, so "absent" in a CI transcript means the runner failed to build
 * the fixture, not that the surface is untested.
 */
public class JdkOnlyPlatformProbe {
    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) {
        section("process", JdkOnlyPlatformProbe::process);
        section("security", JdkOnlyPlatformProbe::security);
        section("vthreads", JdkOnlyPlatformProbe::vthreads);
        section("agent", JdkOnlyPlatformProbe::agent);
        section("jni", JdkOnlyPlatformProbe::jni);
        System.out.println("PLATFORM sections=" + sections + " failed=" + failed);
    }

    // ---------------------------------------------------------------- process

    static boolean windows() {
        return System.getProperty("os.name", "").toLowerCase(Locale.ROOT).contains("win");
    }

    /** A shell invocation that prints exactly {@code s} and a newline. */
    static List<String> shell(String s) {
        return windows() ? List.of("cmd.exe", "/c", s) : List.of("/bin/sh", "-c", s);
    }

    static void process() {
        try {
            // 1. stdout capture and exit status of a successful child.
            ProcessBuilder pb = new ProcessBuilder(shell("echo craton-probe"));
            pb.redirectErrorStream(true);
            Process p = pb.start();
            String out = drain(p.getInputStream());
            boolean done = p.waitFor(20, TimeUnit.SECONDS);
            int rc = done ? p.exitValue() : -999;
            if (!done) p.destroyForcibly();

            // 2. A non-zero exit must survive as a value, not as an exception.
            Process fail = new ProcessBuilder(shell("exit 3")).start();
            boolean failDone = fail.waitFor(20, TimeUnit.SECONDS);
            int failRc = failDone ? fail.exitValue() : -999;
            if (!failDone) fail.destroyForcibly();

            // 3. The child environment is the parent's, plus what we put in it.
            ProcessBuilder envPb = new ProcessBuilder(
                    shell(windows() ? "echo %CRATON_PROBE_VAR%" : "echo \"$CRATON_PROBE_VAR\""));
            envPb.environment().put("CRATON_PROBE_VAR", "env-ok");
            envPb.redirectErrorStream(true);
            Process envP = envPb.start();
            String envOut = drain(envP.getInputStream());
            boolean envDone = envP.waitFor(20, TimeUnit.SECONDS);
            if (!envDone) envP.destroyForcibly();

            // 4. Redirection to a file, and the working directory.
            Path dir = Files.createTempDirectory("craton-proc");
            Path sink = dir.resolve("out.txt");
            ProcessBuilder redir = new ProcessBuilder(shell("echo redirected"));
            redir.directory(dir.toFile());
            redir.redirectOutput(sink.toFile());
            Process rp = redir.start();
            boolean rDone = rp.waitFor(20, TimeUnit.SECONDS);
            if (!rDone) rp.destroyForcibly();
            String fileOut = Files.exists(sink)
                    ? new String(Files.readAllBytes(sink), StandardCharsets.UTF_8).trim() : "<none>";

            // 5. stdin actually reaches the child.
            ProcessBuilder cat = new ProcessBuilder(shell(windows() ? "more" : "cat"));
            cat.redirectErrorStream(true);
            Process cp = cat.start();
            try (OutputStream os = cp.getOutputStream()) {
                os.write("piped-in\n".getBytes(StandardCharsets.UTF_8));
            }
            String catOut = drain(cp.getInputStream());
            boolean catDone = cp.waitFor(20, TimeUnit.SECONDS);
            if (!catDone) cp.destroyForcibly();

            // 6. destroy() on a child that would otherwise outlive us. The exit
            //    code of a killed process is platform-specific, so only the
            //    liveness transition is printed.
            Process sleeper = new ProcessBuilder(shell(windows() ? "ping -n 30 127.0.0.1" : "sleep 30")).start();
            boolean aliveBefore = sleeper.isAlive();
            sleeper.destroyForcibly();
            boolean reaped = sleeper.waitFor(20, TimeUnit.SECONDS);
            boolean aliveAfter = sleeper.isAlive();

            // 7. ProcessHandle: identity and the parent/child relationship. The
            //    pids themselves vary, so only their shape is printed.
            boolean selfPid = ProcessHandle.current().pid() > 0;
            boolean handleAlive = ProcessHandle.current().isAlive();
            Optional<String> cmd = ProcessHandle.current().info().command();

            deleteTree(dir);
            System.out.println("process out=[" + out.trim() + "] rc=" + rc
                    + " failRc=" + failRc
                    + " env=[" + envOut.trim() + "]"
                    + " file=[" + fileOut + "]"
                    + " stdin=[" + catOut.trim() + "]"
                    + " kill=" + aliveBefore + "/" + reaped + "/" + aliveAfter
                    + " selfPid=" + selfPid + " selfAlive=" + handleAlive
                    + " cmdPresent=" + cmd.isPresent());
        } catch (IOException | InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    static String drain(InputStream in) throws IOException {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        byte[] buf = new byte[512];
        int k;
        while ((k = in.read(buf)) > 0) bo.write(buf, 0, k);
        return new String(bo.toByteArray(), StandardCharsets.UTF_8);
    }

    static void deleteTree(Path dir) {
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
            for (Path p : ds) Files.deleteIfExists(p);
        } catch (IOException ignored) { }
        try {
            Files.deleteIfExists(dir);
        } catch (IOException ignored) { }
    }

    // --------------------------------------------------------------- security

    static void security() {
        try {
            // The provider NAMES are a property of the image's java.security,
            // so they must agree across VMs. Versions must not be printed:
            // they are doubles whose formatting is a locale question.
            List<String> providers = new ArrayList<>();
            for (Provider pr : Security.getProviders()) providers.add(pr.getName());

            byte[] msg = "craton-strict-corpus".getBytes(StandardCharsets.UTF_8);
            String sha256 = hex(MessageDigest.getInstance("SHA-256").digest(msg));
            String sha1 = hex(MessageDigest.getInstance("SHA-1").digest(msg));
            String sha3 = hex(MessageDigest.getInstance("SHA3-256").digest(msg));
            String md5 = hex(MessageDigest.getInstance("MD5").digest(msg));

            Mac mac = Mac.getInstance("HmacSHA256");
            mac.init(new SecretKeySpec("0123456789abcdef".getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
            String hmac = hex(mac.doFinal(msg));

            // AES-GCM with a fixed key and IV is deterministic, so the
            // ciphertext and tag are comparable across VMs. A GCM tag that
            // matches is a much stronger statement than "no exception".
            byte[] key = new byte[16];
            for (int i = 0; i < key.length; i++) key[i] = (byte) i;
            byte[] iv = new byte[12];
            for (int i = 0; i < iv.length; i++) iv[i] = (byte) (0x40 + i);
            Cipher gcm = Cipher.getInstance("AES/GCM/NoPadding");
            gcm.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, "AES"), new GCMParameterSpec(128, iv));
            byte[] ct = gcm.doFinal(msg);
            Cipher gcmDec = Cipher.getInstance("AES/GCM/NoPadding");
            gcmDec.init(Cipher.DECRYPT_MODE, new SecretKeySpec(key, "AES"), new GCMParameterSpec(128, iv));
            boolean roundTrip = Arrays.equals(msg, gcmDec.doFinal(ct));

            Cipher cbc = Cipher.getInstance("AES/CBC/PKCS5Padding");
            cbc.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, "AES"), new IvParameterSpec(new byte[16]));
            String cbcCt = hex(cbc.doFinal(msg));

            // PBKDF2 over a fixed salt: deterministic, and the one place a
            // wrong iteration or a wrong PRF shows up as a value.
            KeySpec ks = new PBEKeySpec("passphrase".toCharArray(),
                    "saltsalt".getBytes(StandardCharsets.UTF_8), 4096, 128);
            String pbkdf2 = hex(SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256")
                    .generateSecret(ks).getEncoded());

            // SHA1PRNG re-seeded with a fixed seed is specified to be
            // reproducible, which makes even the RNG diffable.
            String prng;
            try {
                SecureRandom sr = SecureRandom.getInstance("SHA1PRNG");
                sr.setSeed(new byte[]{1, 2, 3, 4, 5, 6, 7, 8});
                byte[] draw = new byte[16];
                sr.nextBytes(draw);
                prng = hex(draw);
            } catch (NoSuchAlgorithmException e) {
                prng = "<no-SHA1PRNG>";
            }

            // Asymmetric: the key material is random, so what is asserted is
            // the verify result, the encodings, and the modulus width.
            KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
            kpg.initialize(2048);
            KeyPair kp = kpg.generateKeyPair();
            Signature sig = Signature.getInstance("SHA256withRSA");
            sig.initSign(kp.getPrivate());
            sig.update(msg);
            byte[] signature = sig.sign();
            Signature ver = Signature.getInstance("SHA256withRSA");
            ver.initVerify(kp.getPublic());
            ver.update(msg);
            boolean verified = ver.verify(signature);
            int modBits = ((java.security.interfaces.RSAPublicKey) kp.getPublic())
                    .getModulus().bitLength();

            // A PKCS12 keystore round-trip through a byte array: store a secret
            // key, read it back, and compare the material.
            KeyStore p12 = KeyStore.getInstance("PKCS12");
            p12.load(null, null);
            char[] pw = "changeit".toCharArray();
            p12.setEntry("secret", new KeyStore.SecretKeyEntry(new SecretKeySpec(key, "AES")),
                    new KeyStore.PasswordProtection(pw));
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            p12.store(bo, pw);
            KeyStore back = KeyStore.getInstance("PKCS12");
            back.load(new ByteArrayInputStream(bo.toByteArray()), pw);
            Key readBack = back.getKey("secret", pw);
            boolean p12RoundTrip = readBack != null && Arrays.equals(key, readBack.getEncoded());

            // The image's own trust store, parsed with the real X.509 factory.
            // Its size is a property of the image, so it agrees across VMs on
            // one machine and is a real parse of hundreds of certificates.
            String cacerts;
            try {
                Path ca = Paths.get(System.getProperty("java.home"), "lib", "security", "cacerts");
                if (Files.isReadable(ca)) {
                    KeyStore jks = KeyStore.getInstance("JKS");
                    try (InputStream in = Files.newInputStream(ca)) {
                        jks.load(in, "changeit".toCharArray());
                    }
                    List<String> aliases = Collections.list(jks.aliases());
                    Collections.sort(aliases);
                    int certs = 0;
                    for (String a : aliases) if (jks.isCertificateEntry(a)) certs++;
                    // Digest the sorted alias list rather than printing it:
                    // hundreds of lines of CA names would drown the transcript,
                    // but any difference still changes the digest.
                    cacerts = certs + "/" + aliases.size() + "/"
                            + hex(MessageDigest.getInstance("SHA-256").digest(
                                    String.join(",", aliases).getBytes(StandardCharsets.UTF_8))).substring(0, 16);
                } else {
                    cacerts = "<unreadable>";
                }
            } catch (GeneralSecurityException | IOException e) {
                cacerts = "<" + e.getClass().getSimpleName() + ">";
            }

            System.out.println("security providers=" + providers);
            System.out.println("security sha256=" + sha256 + " sha1=" + sha1);
            System.out.println("security sha3=" + sha3 + " md5=" + md5);
            System.out.println("security hmac=" + hmac + " pbkdf2=" + pbkdf2 + " prng=" + prng);
            System.out.println("security gcm=" + hex(ct) + " rt=" + roundTrip + " cbc=" + cbcCt);
            System.out.println("security rsa verify=" + verified + " bits=" + modBits
                    + " pubFmt=" + kp.getPublic().getFormat()
                    + " privFmt=" + kp.getPrivate().getFormat()
                    + " sigLen=" + signature.length
                    + " p12=" + p12RoundTrip + " cacerts=" + cacerts);
        } catch (GeneralSecurityException | IOException e) {
            throw new RuntimeException(e);
        }
    }

    static String hex(byte[] b) {
        return new BigInteger(1, b).toString(16);
    }

    // --------------------------------------------------------------- vthreads

    static void vthreads() {
        try {
            // A plain virtual thread, joined with a bound.
            final boolean[] sawVirtual = new boolean[2];
            final String[] name = new String[1];
            Thread v = Thread.ofVirtual().name("craton-v1").unstarted(() -> {
                sawVirtual[0] = Thread.currentThread().isVirtual();
                name[0] = Thread.currentThread().getName();
            });
            v.start();
            boolean joined = v.join(java.time.Duration.ofSeconds(20));

            // Thread.startVirtualThread, the one-liner form.
            CountDownLatch latch = new CountDownLatch(1);
            Thread sv = Thread.startVirtualThread(() -> {
                sawVirtual[1] = Thread.currentThread().isVirtual();
                latch.countDown();
            });
            boolean latched = latch.await(20, TimeUnit.SECONDS);
            sv.join(20_000);

            // A per-task executor with enough tasks to force carrier reuse.
            //
            // NOT try-with-resources, deliberately. `ExecutorService.close()`
            // is `shutdown()` followed by `awaitTermination(1, DAYS)` in a
            // loop, i.e. an UNBOUNDED blocking call, and this probe's own
            // first rule is that every blocking call is bounded. Written with
            // try-with-resources it hung for 300 seconds and was killed,
            // taking the `agent` and `jni` sections with it and reporting a
            // truncated transcript. Written this way the same defect is a
            // VALUE — `terminated=false` against HotSpot's `true` — which
            // diffs, names itself, and lets the rest of the probe run.
            // See docs/known-issues/vm/concurrenthashmap-newkeyset-returns-a-plain-hashset-20260805.md
            AtomicInteger completed = new AtomicInteger();
            int sum = 0;
            ExecutorService es = Executors.newVirtualThreadPerTaskExecutor();
            boolean terminated;
            try {
                List<Future<Integer>> fs = new ArrayList<>();
                for (int i = 0; i < 256; i++) {
                    final int k = i;
                    fs.add(es.submit(() -> {
                        completed.incrementAndGet();
                        return k * 2;
                    }));
                }
                for (Future<Integer> f : fs) sum += f.get(30, TimeUnit.SECONDS);
            } finally {
                es.shutdown();
                terminated = es.awaitTermination(30, TimeUnit.SECONDS);
                if (!terminated) es.shutdownNow();
            }

            // Blocking inside a virtual thread must unmount, not deadlock the
            // carrier: 64 virtual threads on a queue that only ever has one
            // item in flight.
            SynchronousQueue<Integer> sq = new SynchronousQueue<>();
            AtomicInteger handoffs = new AtomicInteger();
            List<Thread> vs = new ArrayList<>();
            for (int i = 0; i < 64; i++) {
                vs.add(Thread.startVirtualThread(() -> {
                    try {
                        Integer got = sq.poll(20, TimeUnit.SECONDS);
                        if (got != null) handoffs.incrementAndGet();
                    } catch (InterruptedException ignored) { }
                }));
            }
            for (int i = 0; i < 64; i++) sq.offer(i, 20, TimeUnit.SECONDS);
            boolean allJoined = true;
            for (Thread t : vs) allJoined &= t.join(java.time.Duration.ofSeconds(20));

            // Pinning: a synchronized block around a park. In JDK 24+ this no
            // longer pins the carrier, but either way it must complete.
            final boolean[] pinnedRan = new boolean[1];
            final Object lock = new Object();
            Thread pinned = Thread.startVirtualThread(() -> {
                synchronized (lock) {
                    try {
                        Thread.sleep(5);
                    } catch (InterruptedException ignored) { }
                    pinnedRan[0] = true;
                }
            });
            boolean pinnedJoined = pinned.join(java.time.Duration.ofSeconds(20));

            // ThreadLocal must be per-virtual-thread, not per-carrier.
            ThreadLocal<Integer> tl = new ThreadLocal<>();
            AtomicInteger tlOk = new AtomicInteger();
            List<Thread> tls = new ArrayList<>();
            for (int i = 0; i < 32; i++) {
                final int k = i;
                tls.add(Thread.startVirtualThread(() -> {
                    tl.set(k);
                    Thread.yield();
                    if (tl.get() != null && tl.get() == k) tlOk.incrementAndGet();
                }));
            }
            for (Thread t : tls) t.join(20_000);

            boolean platformIsNotVirtual = !Thread.currentThread().isVirtual();
            System.out.println("vthreads join=" + joined + " isVirtual=" + sawVirtual[0]
                    + "/" + sawVirtual[1] + " name=" + name[0] + " latched=" + latched
                    + " completed=" + completed.get() + " sum=" + sum
                    + " terminated=" + terminated
                    + " handoffs=" + handoffs.get() + " allJoined=" + allJoined
                    + " pinned=" + pinnedRan[0] + "/" + pinnedJoined
                    + " tl=" + tlOk.get() + " mainPlatform=" + platformIsNotVirtual);
        } catch (InterruptedException | ExecutionException | TimeoutException e) {
            throw new RuntimeException(e);
        }
    }

    // ------------------------------------------------------------ agent/attach

    static void agent() {
        // The instrumentation SURFACE is workload-independent: the method count
        // of the interface is a property of the image and must agree.
        String ifaceMethods;
        try {
            ifaceMethods = String.valueOf(
                    Class.forName("java.lang.instrument.Instrumentation").getMethods().length);
        } catch (ClassNotFoundException e) {
            ifaceMethods = "<absent>";
        }

        // Did an agent actually run? JdkOnlyProbeAgent records its premain in
        // static fields; reflection keeps this probe compilable without it.
        String agentState = "absent";
        String transformed = "-";
        String retransform = "-";
        String appended = "-";
        try {
            Class<?> a = Class.forName("JdkOnlyProbeAgent");
            Method premainRan = a.getMethod("premainRan");
            agentState = String.valueOf(premainRan.invoke(null));
            // The COUNT of transformed classes is workload-dependent (the VMs
            // do not load the same set), so only its shape is asserted.
            int n = (Integer) a.getMethod("transformedCount").invoke(null);
            transformed = n > 0 ? "positive" : "zero";
            retransform = String.valueOf(a.getMethod("canRetransform").invoke(null));
            appended = String.valueOf(a.getMethod("selfTransformSeen").invoke(null));
        } catch (ClassNotFoundException e) {
            // No -javaagent on this run; leave "absent".
        } catch (ReflectiveOperationException e) {
            agentState = "<" + e.getClass().getSimpleName() + ">";
        }

        // RuntimeMXBean: the name and start time vary, so read the shape.
        RuntimeMXBean rt = ManagementFactory.getRuntimeMXBean();
        boolean hasName = rt.getName() != null && !rt.getName().isEmpty();
        boolean hasSpecVersion = rt.getSpecVersion() != null;
        boolean argsNonNull = rt.getInputArguments() != null;

        // The attach API lives in jdk.attach. Self-attach is disabled by
        // default, so what is asserted is that the class resolves and that
        // list() returns a List rather than throwing.
        String attach;
        try {
            Class<?> vm = Class.forName("com.sun.tools.attach.VirtualMachine");
            Object list = vm.getMethod("list").invoke(null);
            attach = (list instanceof List) ? "list-ok" : "list-" + typeName(list);
        } catch (ClassNotFoundException e) {
            attach = "module-absent";
        } catch (ReflectiveOperationException e) {
            Throwable c = e.getCause() != null ? e.getCause() : e;
            attach = "throw-" + c.getClass().getSimpleName();
        }

        System.out.println("agent premain=" + agentState + " transformed=" + transformed
                + " retransform=" + retransform + " selfSeen=" + appended
                + " ifaceMethods=" + ifaceMethods
                + " rtName=" + hasName + " rtSpec=" + hasSpecVersion + " rtArgs=" + argsNonNull
                + " attach=" + attach);
    }

    static String typeName(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    // -------------------------------------------------------------------- jni

    static void jni() {
        // mapLibraryName is pure string work and must agree with the platform.
        String mapped = System.mapLibraryName("cratonjniprobe");

        String lib = System.getProperty("craton.probe.jnilib");
        if (lib == null || lib.isEmpty()) {
            System.out.println("jni mapped=" + mapped + " lib=absent");
            return;
        }
        try {
            System.load(lib);
        } catch (Throwable t) {
            System.out.println("jni mapped=" + mapped + " lib=load-failed:"
                    + t.getClass().getSimpleName());
            failed++;
            return;
        }
        // Accumulate, and print in a finally. Each call below exercises a
        // DIFFERENT JNI family, so one unbound method must not erase the
        // verdict on the ten before it: when the `$`-mangling defect was fixed,
        // the failure moved from the first family to the last, and a section
        // that printed only on full success could not show that the nine in
        // between had started working.
        StringBuilder sb = new StringBuilder("jni mapped=").append(mapped);
        try {
            sb.append(" add=").append(JniProbe.add(40, 2));
            sb.append(" mul=").append(JniProbe.mulLong(0x7fffffffL, 3L));
            sb.append(" scale=").append(JniProbe.scale(1.5, 4));
            sb.append(" rev=").append(JniProbe.reverse("craton"));
            int[] arr = {1, 2, 3, 4, 5};
            sb.append(" arrSum=").append(JniProbe.sumInts(arr));
            sb.append(" abortKept=").append(Arrays.toString(arr));
            JniProbe.doubleInts(arr);
            sb.append(" doubled=").append(Arrays.toString(arr));
            sb.append(" join=").append(JniProbe.joinStrings(new String[]{"a", "b", "c"}));
            JniProbe holder = new JniProbe();
            holder.value = 7;
            sb.append(" field=").append(JniProbe.readValue(holder));
            JniProbe.writeValue(holder, 21);
            sb.append("->").append(holder.value);
            sb.append(" upcall=").append(JniProbe.callBackTriple(9));
            sb.append(" upcallThrow=").append(JniProbe.upcallThrow(4));
            try {
                JniProbe.throwIse("from-native");
                sb.append(" throw=<no-throw>");
            } catch (IllegalStateException e) {
                sb.append(" throw=ISE:").append(e.getMessage());
            }
            sb.append(" registered=").append(JniProbe.registeredNative(5) == 50);
        } catch (UnsatisfiedLinkError e) {
            sb.append(" unsatisfied=").append(e.getMessage());
            failed++;
        } finally {
            System.out.println(sb);
        }
    }

    /**
     * The Java half of {@code probes/jdkonly_jni_probe.c}. Nested so the probe
     * stays a single compilation unit; the C file spells the mangled names
     * accordingly ({@code Java_JdkOnlyPlatformProbe_00024JniProbe_*}).
     */
    public static class JniProbe {
        public int value;

        public static native int add(int a, int b);
        public static native long mulLong(long a, long b);
        public static native double scale(double d, int by);
        public static native String reverse(String s);
        public static native int sumInts(int[] a);
        public static native void doubleInts(int[] a);
        public static native String joinStrings(String[] a);
        public static native int readValue(JniProbe o);
        public static native void writeValue(JniProbe o, int v);
        public static native void throwIse(String msg);
        /** Calls back into {@link #triple(int)} via CallStaticIntMethod. */
        public static native int callBackTriple(int n);
        /**
         * Calls {@link #boom(int)} the same way and reports what the native
         * saw afterwards: {@code iae} when the thrown exception was pending at
         * the up-call's return, {@code no-pending} when it was swallowed. The
         * native clears it, so nothing escapes back to here.
         */
        public static native String upcallThrow(int n);
        /** Bound by JNI_OnLoad's RegisterNatives, not by symbol lookup. */
        public static native int registeredNative(int n);

        public static int triple(int n) {
            return n * 3;
        }

        /** The up-call target for {@link #upcallThrow(int)}. Always throws. */
        public static int boom(int n) {
            throw new IllegalArgumentException("boom-" + n);
        }
    }
}
