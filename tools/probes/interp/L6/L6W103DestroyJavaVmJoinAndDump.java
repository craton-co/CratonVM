// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L6 -- the `DestroyJavaVM` thread's
// residue (page `i101-L6-main-thread-stays-alive-during-destroy-java-vm`):
//   1. a timed `join` on the `DestroyJavaVM` mirror waits (it is alive);
//      the served `--compatible` join polled the registry id, which answers
//      the entry dead to Java once `main` ended, and returned at once;
//   2. a thread dump prints it as HotSpot does: `waiting on condition`, a zero
//      stack address, `java.lang.Thread.State: RUNNABLE`, no frame (it printed
//      the launcher's own wait state and frames);
//   3. JFR: `jdk.ThreadEnd` for `main` and `jdk.ThreadStart` for
//      `DestroyJavaVM` (HotSpot's detach of main and attach of DestroyJavaVM).
//
// The parent runs a child of the same VM (the current process's executable
// and launcher arguments up to the main class, so the mode carries over).
// The child's `main` starts a JFR recording and a non-daemon watcher, then
// returns; the watcher finds `DestroyJavaVM`, joins it for 300 ms, and on
// Linux sends its own process SIGQUIT (`kill -3`) so the VM prints a thread
// dump on stdout; then it stops the recording and reads it. The parent
// prints the child's lines and, from the dump, only the `DestroyJavaVM`
// thread's state words, stack address, state line and frame count. On
// Windows (no SIGQUIT) the dump rows read `dump: not on Linux` for both VMs.
//
// Run: javac -d out L6W103DestroyJavaVmJoinAndDump.java
//      java -cp out L6W103DestroyJavaVmJoinAndDump
//      cratonvm [--compatible] -cp out L6W103DestroyJavaVmJoinAndDump
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, Linux, default flags; derived from the sources)
// child exit=0
//   main returning
//   DestroyJavaVM listed: true
//   DestroyJavaVM join(300) waited: true
//   DestroyJavaVM alive after join: true
//   jfr ThreadEnd main: true
//   jfr ThreadStart DestroyJavaVM: true
//   dump DestroyJavaVM header words: waiting on condition
//   dump DestroyJavaVM stack address zero: true
//   dump DestroyJavaVM state: java.lang.Thread.State: RUNNABLE
//   dump DestroyJavaVM frames: 0
// HOTSPOT_EXPECTED_END

import java.io.BufferedReader;
import java.io.File;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

public class L6W103DestroyJavaVmJoinAndDump {
    static final boolean LINUX = System.getProperty("os.name").startsWith("Linux");

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            child();
            return;
        }
        parent();
    }

    static void child() throws Exception {
        Path jfr = Files.createTempFile("l6w103djv", ".jfr");
        Recording r = new Recording();
        r.enable("jdk.ThreadStart").withoutStackTrace();
        r.enable("jdk.ThreadEnd").withoutStackTrace();
        r.start();
        Thread main = Thread.currentThread();
        Thread watcher = new Thread(() -> {
            try {
                main.join(10_000);
                Thread.sleep(200);
                Thread destroy = null;
                for (Thread t : Thread.getAllStackTraces().keySet()) {
                    if ("DestroyJavaVM".equals(t.getName())) {
                        destroy = t;
                    }
                }
                System.out.println("DestroyJavaVM listed: " + (destroy != null));
                if (destroy != null) {
                    long t0 = System.nanoTime();
                    destroy.join(300);
                    long ms = (System.nanoTime() - t0) / 1_000_000;
                    System.out.println("DestroyJavaVM join(300) waited: " + (ms >= 250));
                    System.out.println("DestroyJavaVM alive after join: " + destroy.isAlive());
                }
                r.stop();
                r.dump(jfr);
                r.close();
                boolean mainEnd = false;
                boolean djvStart = false;
                for (RecordedEvent e : RecordingFile.readAllEvents(jfr)) {
                    RecordedThread th = e.getThread("thread");
                    String name = th == null ? null : th.getJavaName();
                    String type = e.getEventType().getName();
                    if (type.equals("jdk.ThreadEnd") && "main".equals(name)) {
                        mainEnd = true;
                    }
                    if (type.equals("jdk.ThreadStart") && "DestroyJavaVM".equals(name)) {
                        djvStart = true;
                    }
                }
                Files.deleteIfExists(jfr);
                System.out.println("jfr ThreadEnd main: " + mainEnd);
                System.out.println("jfr ThreadStart DestroyJavaVM: " + djvStart);
                if (LINUX) {
                    System.out.println("dump-begin");
                    System.out.flush();
                    Process kill = new ProcessBuilder("kill", "-3",
                            String.valueOf(ProcessHandle.current().pid())).start();
                    kill.waitFor(30, TimeUnit.SECONDS);
                    Thread.sleep(1_000);
                    System.out.flush();
                    System.out.println("dump-end");
                }
            } catch (Exception e) {
                System.out.println("watcher: " + e);
            }
        }, "watcher");
        watcher.start();
        Thread.sleep(100);
        System.out.println("main returning");
    }

    static void parent() throws Exception {
        String self = L6W103DestroyJavaVmJoinAndDump.class.getName();
        List<String> cmd = new ArrayList<>();
        String home = System.getProperty("java.home");
        cmd.add(ProcessHandle.current().info().command()
                .orElse(home + File.separator + "bin" + File.separator + "java"));
        String[] argv = ProcessHandle.current().info().arguments().orElse(new String[0]);
        int main = Arrays.asList(argv).indexOf(self);
        if (main > 0) {
            cmd.addAll(Arrays.asList(argv).subList(0, main));
        } else {
            cmd.add("-cp");
            cmd.add(System.getProperty("java.class.path"));
        }
        cmd.add(self);
        cmd.add("child");
        Process p = new ProcessBuilder(cmd).redirectError(ProcessBuilder.Redirect.DISCARD).start();
        p.getOutputStream().close();
        List<String> lines = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(
                new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
            String line;
            while ((line = r.readLine()) != null) {
                lines.add(line);
            }
        }
        boolean done = p.waitFor(120, TimeUnit.SECONDS);
        System.out.println("child exit=" + (done ? String.valueOf(p.exitValue()) : "timeout"));
        boolean inDump = false;
        List<String> dump = new ArrayList<>();
        for (String l : lines) {
            if (l.equals("dump-begin")) {
                inDump = true;
            } else if (l.equals("dump-end")) {
                inDump = false;
            } else if (inDump) {
                dump.add(l);
            } else {
                System.out.println("  " + l);
            }
        }
        if (!LINUX) {
            System.out.println("  dump: not on Linux");
            return;
        }
        Pattern header = Pattern.compile("^\"DestroyJavaVM\" .* nid=\\S+ (.*?)\\s+\\[0x([0-9a-fA-F]+)\\]\\s*$");
        int at = -1;
        for (int i = 0; i < dump.size(); i++) {
            Matcher m = header.matcher(dump.get(i));
            if (m.matches()) {
                at = i;
                System.out.println("  dump DestroyJavaVM header words: " + m.group(1));
                System.out.println("  dump DestroyJavaVM stack address zero: "
                        + (Long.parseUnsignedLong(m.group(2), 16) == 0L));
                break;
            }
        }
        if (at < 0) {
            System.out.println("  dump DestroyJavaVM: not found");
            return;
        }
        String state = at + 1 < dump.size() ? dump.get(at + 1).trim() : "";
        System.out.println("  dump DestroyJavaVM state: " + state);
        int frames = 0;
        for (int i = at + 2; i < dump.size() && !dump.get(i).isBlank(); i++) {
            if (dump.get(i).trim().startsWith("at ")) {
                frames++;
            }
        }
        System.out.println("  dump DestroyJavaVM frames: " + frames);
    }
}
