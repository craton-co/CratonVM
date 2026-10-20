// Interpreter round i4 wave 86, lane L1: `jstat -gc <pid>` and
// `jstat -class <pid>` against a running VM read its live perf-data counters
// (docs/internal/fixed-bugs/interpreter-i84-L1-perf-data-file-is-a-static-subset-FIXED-20261119.md).
//
// The probe runs on HotSpot (it needs the JDK's jstat, from java.home) and
// starts the VM under test as a child running this class in "child" mode:
// the child loads its classes, runs System.gc() three times, prints "ready"
// and sleeps; the parent then runs jstat -gc and jstat -class against the
// child's pid and prints what is deterministic about their output.
//
//   javac -d /tmp/l1w86jstat tools/probes/interp/L1/L1W86JstatLive.java
//   java -cp /tmp/l1w86jstat L1W86JstatLive java
//   java -cp /tmp/l1w86jstat L1W86JstatLive cratonvm --jdk-only
//   java -cp /tmp/l1w86jstat L1W86JstatLive cratonvm --compatible
// (the arguments after the class name are the child's launcher and options;
// the child gets -cp <this class path> L1W86JstatLive child appended)
//
// HOTSPOT_EXPECTED_BEGIN
// gc header: S0C S1C S0U S1U EC EU OC OU MC MU CCSC CCSU YGC YGCT FGC FGCT CGC CGCT GCT
// gc values: 19
// collections at least 3: true
// class header: Loaded Bytes Unloaded Bytes Time
// class values: 5
// loaded more than 100: true
// HOTSPOT_EXPECTED_END
//
// Before wave 86 CratonVM's file carried no GC or class counters, and jstat
// failed against it (no header, a "could not find counter" style error).
// A "-" value (a counter HotSpot's own collector lacks, e.g. CGC under
// Serial) counts as a value; the numbers themselves are not compared.
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;

public class L1W86JstatLive {
    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            child();
            return;
        }
        if (args.length == 0) {
            System.out.println("usage: L1W86JstatLive <launcher> [options...]");
            return;
        }
        List<String> command = new ArrayList<>(List.of(args));
        command.add("-cp");
        command.add(System.getProperty("java.class.path"));
        command.add("L1W86JstatLive");
        command.add("child");
        Process child = new ProcessBuilder(command).redirectErrorStream(true).start();
        BufferedReader out = new BufferedReader(
                new InputStreamReader(child.getInputStream(), StandardCharsets.UTF_8));
        String line;
        while ((line = out.readLine()) != null && !line.equals("ready")) {
            // the child's output before "ready" is not compared
        }
        // Two sampling intervals for the counters to catch up.
        Thread.sleep(500);
        String jstat = System.getProperty("java.home") + "/bin/jstat";
        long pid = child.pid();
        List<String> gc = run(jstat, "-gc", Long.toString(pid));
        List<String> cls = run(jstat, "-class", Long.toString(pid));
        child.destroy();
        child.waitFor(10, TimeUnit.SECONDS);

        String[] gcHeader = tokens(gc, 0);
        String[] gcValues = tokens(gc, 1);
        System.out.println("gc header: " + String.join(" ", gcHeader));
        System.out.println("gc values: " + gcValues.length);
        long collections = 0;
        for (String name : new String[] {"YGC", "FGC", "CGC"}) {
            collections += number(gcHeader, gcValues, name);
        }
        System.out.println("collections at least 3: " + (collections >= 3));
        String[] classHeader = tokens(cls, 0);
        String[] classValues = tokens(cls, 1);
        System.out.println("class header: " + String.join(" ", classHeader));
        System.out.println("class values: " + classValues.length);
        System.out.println("loaded more than 100: " + (number(classHeader, classValues, "Loaded") > 100));
    }

    private static void child() throws Exception {
        List<byte[]> keep = new ArrayList<>();
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 1000; i++) {
                keep.add(new byte[1024]);
            }
            keep.clear();
            System.gc();
        }
        System.out.println("ready");
        System.out.flush();
        Thread.sleep(60_000);
    }

    private static List<String> run(String... command) throws Exception {
        Process p = new ProcessBuilder(command).redirectErrorStream(true).start();
        List<String> lines = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(
                new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
            String line;
            while ((line = r.readLine()) != null) {
                lines.add(line);
            }
        }
        p.waitFor(30, TimeUnit.SECONDS);
        return lines;
    }

    private static String[] tokens(List<String> lines, int index) {
        if (lines.size() <= index) {
            return new String[0];
        }
        String line = lines.get(index).trim();
        return line.isEmpty() ? new String[0] : line.split("\\s+");
    }

    private static long number(String[] header, String[] values, String name) {
        for (int i = 0; i < header.length && i < values.length; i++) {
            if (header[i].equals(name)) {
                try {
                    return (long) Double.parseDouble(values[i]);
                } catch (NumberFormatException e) {
                    return 0;
                }
            }
        }
        return 0;
    }
}
