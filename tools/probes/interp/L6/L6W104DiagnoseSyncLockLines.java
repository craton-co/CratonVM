// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L6 -- the lock lines of EVERY frame in
// the `-XX:DiagnoseSyncOnValueBasedClasses=2` stack (page
// `i101-L6-diagnose-sync-on-value-based-classes-ignored`, residue "lock lines
// below the top frame"), and a `static synchronized` method's monitor spelled
// `(a java.lang.Class for <name>)`, as HotSpot 25's
// `javaVFrame::print_lock_info_on` / `print_locked_object_class_name` print
// them under `print_stack_on` (`JavaMonitorsInStackTrace` is on by default).
//
// The child enters `outer` (static synchronized: the class mirror), which
// locks PLAIN, calls `middle` (locks OTHER), which calls `inner`, which
// synchronizes on an `Integer`. HotSpot prints, innermost first, each frame's
// held monitors newest first; the top frame's value-based object first.
// Same self-launching scheme as `L6W103DiagnoseSyncArms` (the parent's mode
// flag carries over, absolute class path, scratch directory); addresses are
// `0xADDR`, the log uptime `[UP]`, tabs `<TAB>`.
//
// Run: javac -d out L6W104DiagnoseSyncLockLines.java
//      java -cp out L6W104DiagnoseSyncLockLines
//      cratonvm [--compatible] -cp out L6W104DiagnoseSyncLockLines
//
// HOTSPOT_EXPECTED_BEGIN
// same vm=true
// arm 2 exit=0
//   out: before
//   out: [UP][info][valuebasedclasses] Synchronizing on object 0xADDR of klass java.lang.Integer
//   out: [UP][info][valuebasedclasses] <TAB>at L6W104DiagnoseSyncLockLines.inner(L6W104DiagnoseSyncLockLines.java:67)
//   out: [UP][info][valuebasedclasses] <TAB>- locked <0xADDR> (a java.lang.Integer)
//   out: [UP][info][valuebasedclasses] <TAB>at L6W104DiagnoseSyncLockLines.middle(L6W104DiagnoseSyncLockLines.java:62)
//   out: [UP][info][valuebasedclasses] <TAB>- locked <0xADDR> (a java.lang.Object)
//   out: [UP][info][valuebasedclasses] <TAB>at L6W104DiagnoseSyncLockLines.outer(L6W104DiagnoseSyncLockLines.java:56)
//   out: [UP][info][valuebasedclasses] <TAB>- locked <0xADDR> (a java.lang.Object)
//   out: [UP][info][valuebasedclasses] <TAB>- locked <0xADDR> (a java.lang.Class for L6W104DiagnoseSyncLockLines)
//   out: [UP][info][valuebasedclasses] <TAB>at L6W104DiagnoseSyncLockLines.main(L6W104DiagnoseSyncLockLines.java:78)
//   out: value-based locked
//   out: after
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;

public class L6W104DiagnoseSyncLockLines {
    static final Object PLAIN = new Object();
    static final Object OTHER = new Object();

    // Line numbers are pinned by the expected block: keep this layout.
    static synchronized void outer(Integer boxed) {
        synchronized (PLAIN) {
            middle(boxed);
        }
    }

    static void middle(Integer boxed) {
        synchronized (OTHER) {
            inner(boxed);
        }
    }

    static void inner(Integer boxed) {
        synchronized (boxed) {
            System.out.println("value-based locked");
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            System.out.println("vm=" + System.getProperty("java.vm.name") + "/"
                    + System.getProperty("java.vm.version"));
            Integer boxed = Integer.valueOf(4242);
            System.out.println("before");
            outer(boxed);
            System.out.println("after");
            return;
        }
        File dir = Files.createTempDirectory("l6w104vb").toFile();
        try {
            run(dir);
        } finally {
            File[] left = dir.listFiles();
            if (left != null) {
                for (File f : left) {
                    f.delete();
                }
            }
            dir.delete();
        }
    }

    static String mask(String line) {
        return line.replaceAll("0x[0-9a-fA-F]{16}", "0xADDR")
                .replaceAll("^\\[[0-9.]+s *\\]", "[UP]")
                .replace("\t", "<TAB>");
    }

    static void run(File dir) throws Exception {
        String self = L6W104DiagnoseSyncLockLines.class.getName();
        List<String> cmd = new ArrayList<>();
        String home = System.getProperty("java.home");
        cmd.add(ProcessHandle.current().info().command()
                .orElse(home + File.separator + "bin" + File.separator + "java"));
        String[] argv = ProcessHandle.current().info().arguments().orElse(new String[0]);
        int main = Arrays.asList(argv).indexOf(self);
        for (int i = 0; i < main; i++) {
            String a = argv[i];
            if (a.equals("-cp") || a.equals("-classpath") || a.equals("--class-path")) {
                i++;
            } else if (!a.startsWith("--class-path=")) {
                cmd.add(a);
            }
        }
        cmd.add("-cp");
        cmd.add(absoluteClassPath());
        cmd.add("-XX:+UnlockDiagnosticVMOptions");
        cmd.add("-XX:DiagnoseSyncOnValueBasedClasses=2");
        cmd.add(self);
        cmd.add("child");
        File out = new File(dir, "out.txt");
        File err = new File(dir, "err.txt");
        Process p = new ProcessBuilder(cmd).directory(dir)
                .redirectOutput(out).redirectError(err).start();
        p.getOutputStream().close();
        boolean done = p.waitFor(120, TimeUnit.SECONDS);
        if (!done) {
            p.destroyForcibly();
            p.waitFor();
        }
        String exit = !done ? "timeout" : String.valueOf(p.exitValue());
        List<String> outLines = lines(out);
        String me = "vm=" + System.getProperty("java.vm.name") + "/"
                + System.getProperty("java.vm.version");
        boolean same = false;
        for (String l : outLines) {
            same |= l.equals(me);
        }
        System.out.println("same vm=" + same);
        System.out.println("arm 2 exit=" + exit);
        for (String l : outLines) {
            if (!l.startsWith("vm=") && !l.isEmpty()) {
                System.out.println("  out: " + mask(l));
            }
        }
        for (String l : lines(err)) {
            if (!l.isEmpty()) {
                System.out.println("  err: " + mask(l));
            }
        }
    }

    /** A file's lines; decoding never throws (a console code page on Windows). */
    static List<String> lines(File f) throws Exception {
        String text = new String(Files.readAllBytes(f.toPath()), StandardCharsets.UTF_8);
        List<String> out = new ArrayList<>();
        for (String l : text.split("\\R", -1)) {
            out.add(l);
        }
        if (!out.isEmpty() && out.get(out.size() - 1).isEmpty()) {
            out.remove(out.size() - 1);
        }
        return out;
    }

    /** This process's class path with every entry absolute. */
    static String absoluteClassPath() {
        String cp = System.getProperty("java.class.path", ".");
        if (cp.isEmpty()) {
            cp = ".";
        }
        StringBuilder sb = new StringBuilder();
        for (String e : cp.split(java.util.regex.Pattern.quote(File.pathSeparator))) {
            if (sb.length() > 0) {
                sb.append(File.pathSeparator);
            }
            sb.append(new File(e.isEmpty() ? "." : e).getAbsolutePath());
        }
        return sb.toString();
    }
}
