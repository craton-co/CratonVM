// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.HotSpotDiagnosticMXBean;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * gci i12/td: thread dumps that list virtual threads.
 *
 *   mx N      N parked virtual threads ("td12-v-<i>") and 2 parked platform
 *             threads ("td12-p-<i>"), all in CountDownLatch.await; then
 *             HotSpotDiagnosticMXBean.dumpThreads in plain text and in JSON
 *             (the JDK's ThreadDumper, one ThreadSnapshot.create per thread)
 *   jcmd N    the same threads; then `jcmd <own pid> Thread.dump_to_file`
 *             (plain, then -format=json, then the same file again without and
 *             with -overwrite)
 *   carry     2 virtual threads spinning (mounted) on a 2-carrier scheduler;
 *             then `jcmd <own pid> Thread.print`: its `Carrying virtual thread
 *             #<id>` lines
 *
 * stdout is a pure function of the arguments (counts and fixed words: no
 * pid, path, id, address or time); diagnostics go to stderr.
 */
public class GciTd12ThreadDumpProbe {
    static final CountDownLatch RELEASE = new CountDownLatch(1);
    static volatile boolean spin = true;
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "mx";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        switch (mode) {
            case "mx" -> mx(n);
            case "jcmd" -> jcmd(n);
            case "carry" -> carry(false);
            case "carryframes" -> carry(true);
            case "jcmdq" -> jcmdQuoted();
            case "vmversion" -> vmVersion();
            default -> throw new IllegalArgumentException("mode " + mode);
        }
    }

    static void awaitRelease() {
        try {
            RELEASE.await();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** n parked virtual threads and 2 parked platform threads, all WAITING. */
    static List<Thread> startParked(int n) throws InterruptedException {
        List<Thread> threads = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            threads.add(Thread.ofVirtual().name("td12-v-" + i).start(GciTd12ThreadDumpProbe::awaitRelease));
        }
        for (int i = 0; i < 2; i++) {
            threads.add(Thread.ofPlatform().daemon(true).name("td12-p-" + i)
                    .start(GciTd12ThreadDumpProbe::awaitRelease));
        }
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(60);
        for (Thread t : threads) {
            while (t.getState() != Thread.State.WAITING && System.nanoTime() < deadline) {
                Thread.sleep(5);
            }
        }
        int waiting = 0;
        for (Thread t : threads) {
            if (t.getState() == Thread.State.WAITING) {
                waiting++;
            }
        }
        System.err.println("[td12] waiting=" + waiting + "/" + threads.size());
        return threads;
    }

    static void finish(List<Thread> threads) throws InterruptedException {
        RELEASE.countDown();
        for (Thread t : threads) {
            t.join();
        }
    }

    /** The counts a plain-text ThreadDumper dump yields for the td12 threads. */
    static String plainCounts(String text, int n) {
        int virtualHeaders = 0;
        int platformHeaders = 0;
        int parkedBlocks = 0;
        String[] lines = text.split("\r?\n", -1);
        for (int i = 0; i < lines.length; i++) {
            String line = lines[i];
            if (!line.startsWith("#")) {
                continue;
            }
            boolean v = line.contains("\"td12-v-") && line.contains("\" virtual WAITING ");
            boolean p = line.contains("\"td12-p-") && line.contains("\" WAITING ");
            if (!v && !p) {
                continue;
            }
            if (v) {
                virtualHeaders++;
            } else {
                platformHeaders++;
            }
            for (int j = i + 1; j < lines.length && !lines[j].isEmpty(); j++) {
                if (lines[j].startsWith("    - parking to wait for <java.util.concurrent.CountDownLatch$Sync@")) {
                    parkedBlocks++;
                    break;
                }
            }
        }
        return "virtual=" + virtualHeaders + "/" + n + " platform=" + platformHeaders + "/2 parking="
                + parkedBlocks + "/" + (n + 2);
    }

    /** The counts a JSON ThreadDumper dump yields for the td12 threads. */
    static String jsonCounts(String json, int n) {
        int virtualNamed = 0;
        int platformNamed = 0;
        int parked = 0;
        String[] chunks = json.split("\"tid\": ", -1);
        for (int i = 1; i < chunks.length; i++) {
            String c = chunks[i];
            boolean v = c.contains("\"name\": \"td12-v-") && c.contains("\"virtual\": true");
            boolean p = c.contains("\"name\": \"td12-p-") && !c.contains("\"virtual\": true");
            if (!v && !p) {
                continue;
            }
            if (v) {
                virtualNamed++;
            } else {
                platformNamed++;
            }
            if (c.contains("\"parkBlocker\": {") && c.contains("java.util.concurrent.CountDownLatch$Sync@")) {
                parked++;
            }
        }
        boolean shape = json.contains("\"threadDump\": {") && json.contains("\"threadContainers\": [");
        return "virtual=" + virtualNamed + "/" + n + " platform=" + platformNamed + "/2 parkBlocker="
                + parked + "/" + (n + 2) + " shape=" + shape;
    }

    static void mx(int n) throws Exception {
        List<Thread> threads = startParked(n);
        Path dir = Files.createTempDirectory("td12mx");
        HotSpotDiagnosticMXBean bean = ManagementFactory.getPlatformMXBean(HotSpotDiagnosticMXBean.class);
        for (HotSpotDiagnosticMXBean.ThreadDumpFormat format : HotSpotDiagnosticMXBean.ThreadDumpFormat.values()) {
            boolean json = format == HotSpotDiagnosticMXBean.ThreadDumpFormat.JSON;
            Path file = dir.resolve(json ? "dump.json" : "dump.txt");
            try {
                bean.dumpThreads(file.toAbsolutePath().toString(), format);
                String text = Files.readString(file, StandardCharsets.UTF_8);
                System.err.println("[td12] mx " + format + " bytes=" + text.length());
                System.out.println("mx " + (json ? "json " + jsonCounts(text, n) : "plain " + plainCounts(text, n)));
            } catch (Throwable t) {
                t.printStackTrace();
                System.out.println("mx " + (json ? "json" : "plain") + " threw " + t.getClass().getName());
            }
        }
        finish(threads);
        System.out.println("mx done");
    }

    /** `jcmd <own pid> <command...>`: its exit code and output (pid line dropped). */
    static String[] runJcmd(String... command) throws Exception {
        String exe = System.getProperty("os.name").startsWith("Windows") ? "jcmd.exe" : "jcmd";
        Path jcmd = Path.of(System.getProperty("java.home"), "bin", exe);
        List<String> argv = new ArrayList<>();
        argv.add(jcmd.toString());
        argv.add(Long.toString(ProcessHandle.current().pid()));
        for (String c : command) {
            argv.add(c);
        }
        Process p = new ProcessBuilder(argv).redirectErrorStream(true).start();
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        try (InputStream in = p.getInputStream()) {
            in.transferTo(out);
        }
        boolean exited = p.waitFor(120, TimeUnit.SECONDS);
        int rc = exited ? p.exitValue() : -1;
        String text = out.toString(StandardCharsets.UTF_8);
        System.err.println("[td12] jcmd " + String.join(" ", command) + " rc=" + rc + "\n" + text);
        String body = text;
        int nl = text.indexOf('\n');
        if (text.startsWith(Long.toString(ProcessHandle.current().pid()) + ":") && nl >= 0) {
            body = text.substring(nl + 1);
        }
        return new String[] { Integer.toString(rc), body };
    }

    /** The first word of the dumper's reply: `Created`, `exists` (for `<path> exists, ...`), or `other`. */
    static String reply(String body) {
        String line = body.strip();
        if (line.startsWith("Created ")) {
            return "Created";
        }
        if (line.contains(" exists, use -overwrite to overwrite")) {
            return "exists";
        }
        return "other";
    }

    static void jcmd(int n) throws Exception {
        List<Thread> threads = startParked(n);
        Path dir = Files.createTempDirectory("td12jcmd");
        Path txt = dir.resolve("dump.txt").toAbsolutePath();
        Path json = dir.resolve("dump.json").toAbsolutePath();
        String[] r = runJcmd("Thread.dump_to_file", txt.toString());
        String counts = Files.exists(txt) ? plainCounts(Files.readString(txt, StandardCharsets.UTF_8), n) : "nofile";
        System.out.println("jcmd plain rc=" + r[0] + " reply=" + reply(r[1]) + " " + counts);
        r = runJcmd("Thread.dump_to_file", "-format=json", json.toString());
        counts = Files.exists(json) ? jsonCounts(Files.readString(json, StandardCharsets.UTF_8), n) : "nofile";
        System.out.println("jcmd json rc=" + r[0] + " reply=" + reply(r[1]) + " " + counts);
        r = runJcmd("Thread.dump_to_file", txt.toString());
        System.out.println("jcmd again rc=" + r[0] + " reply=" + reply(r[1]));
        r = runJcmd("Thread.dump_to_file", "-overwrite", txt.toString());
        System.out.println("jcmd overwrite rc=" + r[0] + " reply=" + reply(r[1]));
        finish(threads);
        System.out.println("jcmd done");
    }

    /**
     * `jcmd <pid> Thread.dump_to_file '<dir with a space>/dump.txt'`: the DCmd
     * parser keeps a quoted argument whole (single quotes: the Windows command
     * line leaves them alone; forward slashes: no backslash before a quote).
     */
    static void jcmdQuoted() throws Exception {
        Path dir = Files.createTempDirectory("td12q").resolve("td12 q");
        Files.createDirectories(dir);
        Path file = dir.resolve("dump.txt").toAbsolutePath();
        String quoted = "'" + file.toString().replace('\\', '/') + "'";
        String[] r = runJcmd("Thread.dump_to_file", quoted);
        System.out.println("jcmdq rc=" + r[0] + " reply=" + reply(r[1]) + " file=" + Files.exists(file));
        r = runJcmd("Thread.dump_to_file", "'" + dir.toString().replace('\\', '/') + "/open");
        System.out.println("jcmdq unclosed rc=" + r[0] + " format=" + r[1].contains("Format error in diagnostic command arguments"));
    }

    /**
     * `jcmd <pid> VM.version`: HotSpot's `VersionDCmd` lines, `<java.vm.name>
     * version <java.vm.version>` and `JDK <feature>.<interim>.<update>[.<patch>]`,
     * checked against this VM's own properties.
     */
    static void vmVersion() throws Exception {
        String[] r = runJcmd("VM.version");
        Runtime.Version v = Runtime.version();
        String jdk = v.feature() + "." + v.interim() + "." + v.update() + (v.patch() > 0 ? "." + v.patch() : "");
        String expected = System.getProperty("java.vm.name") + " version " + System.getProperty("java.vm.version")
                + "\nJDK " + jdk + "\n";
        String body = r[1].replace("\r\n", "\n");
        System.err.println("[td12] expected:\n" + expected);
        System.out.println("vmversion rc=" + r[0] + " shape=" + body.equals(expected));
    }

    static void carry(boolean frames) throws Exception {
        List<Thread> spinners = new ArrayList<>();
        CountDownLatch running = new CountDownLatch(2);
        for (int i = 0; i < 2; i++) {
            spinners.add(Thread.ofVirtual().name("td12-s-" + i).start(() -> {
                running.countDown();
                long x = 0;
                while (spin) {
                    x++;
                }
                sink = x;
            }));
        }
        running.await();
        Thread.sleep(200);
        String[] r = runJcmd("Thread.print");
        int lines = 0;
        int matched = 0;
        int runTop = 0;
        int spinnerFrames = 0;
        String[] all = r[1].split("\r?\n");
        for (int k = 0; k < all.length; k++) {
            String line = all[k];
            if (line.startsWith("   Carrying virtual thread #")) {
                // The carrier block's first frame, and whether the carried
                // thread's own frames (the spinning lambda) appear in it.
                if (k + 1 < all.length && all[k + 1].startsWith("\tat jdk.internal.vm.Continuation.run(")) {
                    runTop++;
                }
                for (int m = k + 1; m < all.length && !all[m].isEmpty(); m++) {
                    if (all[m].contains("GciTd12ThreadDumpProbe.lambda$carry$")) {
                        spinnerFrames++;
                        break;
                    }
                }
                lines++;
                long id = Long.parseLong(line.substring("   Carrying virtual thread #".length()).trim());
                for (Thread t : spinners) {
                    if (t.threadId() == id) {
                        matched++;
                    }
                }
            }
        }
        spin = false;
        for (Thread t : spinners) {
            t.join();
        }
        System.out.println("carry rc=" + r[0] + " lines=" + lines + " matched=" + matched);
        if (frames) {
            System.out.println("carryframes runTop=" + runTop + " spinnerFrames=" + spinnerFrames);
        }
    }
}
