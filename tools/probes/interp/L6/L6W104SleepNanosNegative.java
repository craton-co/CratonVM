// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L6 -- `Thread.sleepNanos0` with a
// negative time. HotSpot 25's `JVM_SleepNanos` checks the time first:
// `IllegalArgumentException("nanosecond timeout value out of range")`, and
// only then the interrupt (so an interrupted thread keeps its flag). CratonVM
// returned at once with no exception. No JDK caller passes a negative time
// (`Thread.sleep` validates), so the probe reaches the private native by
// reflection in a child launched with `--add-opens java.base/java.lang=
// ALL-UNNAMED` (same VM, the parent's launcher options carried over).
//
// Run: javac -d out L6W104SleepNanosNegative.java
//      java -cp out L6W104SleepNanosNegative
//      cratonvm [--compatible] -cp out L6W104SleepNanosNegative
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// child exit=0
//   sleepNanos0(-1): java.lang.IllegalArgumentException: nanosecond timeout value out of range
//   interrupted sleepNanos0(-1): java.lang.IllegalArgumentException: nanosecond timeout value out of range
//   flag kept: true
//   interrupted sleepNanos0(0): java.lang.InterruptedException: sleep interrupted
//   flag kept: false
//   done
// HOTSPOT_EXPECTED_END

import java.io.BufferedReader;
import java.io.File;
import java.io.InputStreamReader;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;

public class L6W104SleepNanosNegative {
    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            child();
            return;
        }
        parent();
    }

    static String call(Method m, long nanos) {
        try {
            m.invoke(null, nanos);
            return "returned";
        } catch (InvocationTargetException e) {
            return String.valueOf(e.getCause());
        } catch (Throwable t) {
            return "reflect: " + t;
        }
    }

    static void child() throws Exception {
        Method m = Thread.class.getDeclaredMethod("sleepNanos0", long.class);
        m.setAccessible(true);
        System.out.println("sleepNanos0(-1): " + call(m, -1L));
        Thread.currentThread().interrupt();
        System.out.println("interrupted sleepNanos0(-1): " + call(m, -1L));
        System.out.println("flag kept: " + Thread.currentThread().isInterrupted());
        System.out.println("interrupted sleepNanos0(0): " + call(m, 0L));
        System.out.println("flag kept: " + Thread.interrupted());
        System.out.println("done");
    }

    static void parent() throws Exception {
        String self = L6W104SleepNanosNegative.class.getName();
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
        cmd.add("--add-opens");
        cmd.add("java.base/java.lang=ALL-UNNAMED");
        cmd.add(self);
        cmd.add("child");
        Process p = new ProcessBuilder(cmd).redirectErrorStream(true).start();
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
        for (String l : lines) {
            System.out.println("  " + l);
        }
    }
}
