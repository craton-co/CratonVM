// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L6 -- the non-default arms of
// `-XX:DiagnoseSyncOnValueBasedClasses` (page
// `i101-L6-diagnose-sync-on-value-based-classes-ignored`), run by the probe
// itself so the battery verifies them: the parent launches a child of the
// same VM (`ProcessHandle.current().info().command()` plus the parent's own
// launcher arguments up to the main class, so `--compatible` / `--jdk-only`
// carry over) once per arm, in a scratch directory (HotSpot's `=1` leaves an
// `hs_err_pid*.log` there), and prints the child's exit and its lines with
// the object address spelled `0xADDR`, the log uptime `[UP]` and a tab
// `<TAB>`. The child synchronizes on a plain `Object`, then on an `Integer`
// (`@jdk.internal.ValueBased`, boot loader).
//
// HotSpot 25 (`ObjectSynchronizer::handle_sync_on_value_based_class`):
//   =0: nothing; the lock works.
//   =1: `fatal(...)`: the error report header on stdout, whose
//       `#  fatal error:` line names the object, its klass and the top frame;
//       then abort (exit status 134 on Linux; non-zero on Windows).
//   =2: `[valuebasedclasses]` info lines on stdout (the mode configures
//       stdout for the tag), the locking thread's stack, then the run goes on.
//   =3: out of range: two VM lines and the launcher's two `Error:` lines on
//       stderr, exit 1.
// Expected from code reading (the HotSpot run is the oracle). Every arm's stderr
// is printed; the child's class path is the parent's, made absolute. Of the
// `=1` report only `#  fatal error:` lines are kept (the rest names VM, pid).
//
// Run: javac -d out L6W103DiagnoseSyncArms.java; java -cp out L6W103DiagnoseSyncArms
//      cratonvm [--compatible] -cp out L6W103DiagnoseSyncArms
//
// HOTSPOT_EXPECTED_BEGIN
// same vm=true
// arm 0 exit=0
//   out: before
//   out: plain object locked
//   out: value-based locked
//   out: after
// arm 1 exit=nonzero
//   out: before
//   out: plain object locked
//   out: #  fatal error: Synchronizing on object 0xADDR of klass java.lang.Integer at L6W103DiagnoseSyncArms.main(L6W103DiagnoseSyncArms.java:83)
// arm 2 exit=0
//   out: before
//   out: plain object locked
//   out: [UP][info][valuebasedclasses] Synchronizing on object 0xADDR of klass java.lang.Integer
//   out: [UP][info][valuebasedclasses] <TAB>at L6W103DiagnoseSyncArms.main(L6W103DiagnoseSyncArms.java:83)
//   out: [UP][info][valuebasedclasses] <TAB>- locked <0xADDR> (a java.lang.Integer)
//   out: value-based locked
//   out: after
// arm 3 exit=1
//   err: int DiagnoseSyncOnValueBasedClasses=3 is outside the allowed range [ 0 ... 2 ]
//   err: Improperly specified VM option 'DiagnoseSyncOnValueBasedClasses=3'
//   err: Error: Could not create the Java Virtual Machine.
//   err: Error: A fatal exception has occurred. Program will exit.
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;

public class L6W103DiagnoseSyncArms {
    static final Object PLAIN = new Object();

    static final String[] PROBE_LINES = {
        "before", "plain object locked", "value-based locked", "after", "vm="
    };

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            System.out.println("vm=" + System.getProperty("java.vm.name") + "/"
                    + System.getProperty("java.vm.version"));
            Integer boxed = Integer.valueOf(4242);
            System.out.println("before");
            synchronized (PLAIN) {
                System.out.println("plain object locked");
            }
            // The expected `.java:83` lines above name this line.
            synchronized (boxed) { System.out.println("value-based locked"); }
            System.out.println("after");
            return;
        }
        File dir = Files.createTempDirectory("l6w103vb").toFile();
        try {
            for (int arm = 0; arm <= 3; arm++) {
                run(dir, arm);
            }
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
                .replaceAll("^\\[[0-9.]+s\\]", "[UP]")
                .replace("\t", "<TAB>");
    }

    static boolean kept(String line, int arm) {
        if (arm == 3) {
            return !line.isEmpty();
        }
        if (line.startsWith("#  fatal error:") || line.contains("[valuebasedclasses]")) {
            return true;
        }
        for (String p : PROBE_LINES) {
            if (line.equals(p) || (p.equals("vm=") && line.startsWith(p))) {
                return true;
            }
        }
        return false;
    }

    static void run(File dir, int arm) throws Exception {
        String self = L6W103DiagnoseSyncArms.class.getName();
        List<String> cmd = new ArrayList<>();
        String home = System.getProperty("java.home");
        cmd.add(ProcessHandle.current().info().command()
                .orElse(home + File.separator + "bin" + File.separator + "java"));
        String[] argv = ProcessHandle.current().info().arguments().orElse(new String[0]);
        int main = Arrays.asList(argv).indexOf(self);
        // The parent's launcher options (the mode flag) carry over, except its
        // class path: the child runs in the scratch directory, where a
        // relative `-cp out` names nothing (the child then failed with exit 1
        // on HotSpot, wave 103 hand-back). The class path is always this
        // process's own, made absolute.
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
        if (arm != 0) {
            cmd.add("-XX:+UnlockDiagnosticVMOptions");
            cmd.add("-XX:DiagnoseSyncOnValueBasedClasses=" + arm);
        }
        cmd.add(self);
        cmd.add("child");
        File out = new File(dir, "out-" + arm + ".txt");
        File err = new File(dir, "err-" + arm + ".txt");
        Process p = new ProcessBuilder(cmd).directory(dir)
                .redirectOutput(out).redirectError(err).start();
        p.getOutputStream().close();
        boolean done = p.waitFor(120, TimeUnit.SECONDS);
        if (!done) {
            p.destroyForcibly();
            p.waitFor();
        }
        String exit = !done ? "timeout"
                : p.exitValue() == 0 ? "0"
                : arm == 1 ? "nonzero"
                : String.valueOf(p.exitValue());
        List<String> outLines = lines(out);
        List<String> errLines = lines(err);
        if (arm == 0) {
            String me = "vm=" + System.getProperty("java.vm.name") + "/"
                    + System.getProperty("java.vm.version");
            boolean same = false;
            for (String l : outLines) {
                same |= l.equals(me);
            }
            System.out.println("same vm=" + same);
        }
        System.out.println("arm " + arm + " exit=" + exit);
        for (String l : outLines) {
            if (kept(l, arm) && !l.startsWith("vm=")) {
                System.out.println("  out: " + mask(l));
            }
        }
        // Every arm's stderr, so a child that failed to start shows why.
        for (String l : errLines) {
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
