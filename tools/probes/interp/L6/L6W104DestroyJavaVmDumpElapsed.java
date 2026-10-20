// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L6 -- the thread dump's `elapsed=` of
// the `DestroyJavaVM` thread (page
// `i101-L6-main-thread-stays-alive-during-destroy-java-vm`, residue "the
// dump's elapsed= counts from main's registration"). HotSpot's `elapsed=` is
// the age of the `JavaThread` (`ThreadStatisticalInfo`), and `DestroyJavaVM`
// is a new one, attached after `main` returned; CratonVM reuses the
// launcher's main entry and counted from `main`'s registration.
//
// The child's `main` runs 2.5 s, then returns; a non-daemon watcher waits for
// it, sleeps 200 ms and (Linux only) sends its own process SIGQUIT, so the
// VM prints a thread dump on stdout. The parent reads the `DestroyJavaVM`
// header's `elapsed=` and prints whether it is under 1.5 s (HotSpot: about
// 0.3 s; before the fix: over 2.5 s).
// On Windows (no SIGQUIT) the dump row reads `dump: not on Linux`.
//
// Run: javac -d out L6W104DestroyJavaVmDumpElapsed.java
//      java -cp out L6W104DestroyJavaVmDumpElapsed
//      cratonvm [--compatible] -cp out L6W104DestroyJavaVmDumpElapsed
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, Linux)
// child exit=0
//   main returning
//   dump DestroyJavaVM found: true
//   dump DestroyJavaVM elapsed under 1.5s: true
// HOTSPOT_EXPECTED_END

import java.io.BufferedReader;
import java.io.File;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public class L6W104DestroyJavaVmDumpElapsed {
    static final boolean LINUX = System.getProperty("os.name").startsWith("Linux");

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            child();
            return;
        }
        parent();
    }

    static void child() throws Exception {
        Thread main = Thread.currentThread();
        Thread watcher = new Thread(() -> {
            try {
                main.join(30_000);
                Thread.sleep(200);
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
        Thread.sleep(2_500);
        System.out.println("main returning");
    }

    static void parent() throws Exception {
        String self = L6W104DestroyJavaVmDumpElapsed.class.getName();
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
        Pattern header = Pattern.compile("^\"DestroyJavaVM\" .*\\belapsed=([0-9.]+)s\\b.*$");
        for (String l : dump) {
            Matcher m = header.matcher(l);
            if (m.matches()) {
                System.out.println("  dump DestroyJavaVM found: true");
                System.out.println("  dump DestroyJavaVM elapsed under 1.5s: "
                        + (Double.parseDouble(m.group(1)) < 1.5));
                return;
            }
        }
        System.out.println("  dump DestroyJavaVM found: false");
    }
}
