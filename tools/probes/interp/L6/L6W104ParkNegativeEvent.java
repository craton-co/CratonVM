// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L6 -- the `jdk.ThreadPark` event of a
// relative `Unsafe.park(false, time)` with `time < 0` (page
// `i102-L6-park-until-zero-parks-forever`, last residue).
//
// HotSpot 25 `Unsafe_Park`: `Parker::park` returns at once for `time < 0`,
// and the event is posted with `timeout = time` as the caller passed it,
// `until = Long.MIN_VALUE`. CratonVM waited zero too but reported timeout 0.
// No JDK caller passes a negative time (`LockSupport.parkNanos` guards
// `nanos > 0`), so the probe calls `jdk.internal.misc.Unsafe.park` by
// reflection in a child it launches with `--add-exports
// java.base/jdk.internal.misc=ALL-UNNAMED` (same VM, the parent's launcher
// options carried over). Rows: park(false, -5), park(false, -1000000), and
// park(false, 1) as the control.
//
// Run: javac -d out L6W104ParkNegativeEvent.java
//      java -cp out L6W104ParkNegativeEvent
//      cratonvm [--compatible] -cp out L6W104ParkNegativeEvent
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// child exit=0
//   events=3
//   park(false, -5): timeout=-5 until=MIN
//   park(false, -1000000): timeout=-1000000 until=MIN
//   park(false, 1): timeout=1 until=MIN
//   done
// HOTSPOT_EXPECTED_END

import java.io.BufferedReader;
import java.io.File;
import java.io.InputStreamReader;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.concurrent.TimeUnit;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

public class L6W104ParkNegativeEvent {
    static final String PARKER = "w104-parker";

    static String f(long v) {
        return v == Long.MIN_VALUE ? "MIN" : String.valueOf(v);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            child();
            return;
        }
        parent();
    }

    static void child() throws Exception {
        Class<?> k = Class.forName("jdk.internal.misc.Unsafe");
        Object unsafe = k.getMethod("getUnsafe").invoke(null);
        Method park = k.getMethod("park", boolean.class, long.class);
        Path out = Files.createTempFile("l6w104park", ".jfr");
        Throwable[] failed = new Throwable[1];
        try (Recording r = new Recording()) {
            r.enable("jdk.ThreadPark").withThreshold(Duration.ZERO).withoutStackTrace();
            r.start();
            Thread t = new Thread(() -> {
                try {
                    park.invoke(unsafe, false, -5L);
                    park.invoke(unsafe, false, -1_000_000L);
                    park.invoke(unsafe, false, 1L);
                } catch (Throwable e) {
                    failed[0] = e;
                }
            }, PARKER);
            t.start();
            t.join();
            r.stop();
            r.dump(out);
        }
        if (failed[0] != null) {
            System.out.println("park failed: " + failed[0]);
        }
        List<RecordedEvent> mine = new ArrayList<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(out)) {
            if (!e.getEventType().getName().equals("jdk.ThreadPark")) {
                continue;
            }
            RecordedThread th = e.getThread("eventThread");
            if (th != null && PARKER.equals(th.getJavaName())) {
                mine.add(e);
            }
        }
        Files.deleteIfExists(out);
        mine.sort(Comparator.comparing(RecordedEvent::getStartTime));
        System.out.println("events=" + mine.size());
        String[] labels = {"park(false, -5)", "park(false, -1000000)", "park(false, 1)"};
        for (int i = 0; i < mine.size(); i++) {
            RecordedEvent e = mine.get(i);
            String label = i < labels.length ? labels[i] : "extra";
            System.out.println(label + ": timeout=" + f(e.getLong("timeout"))
                    + " until=" + f(e.getLong("until")));
        }
        System.out.println("done");
    }

    static void parent() throws Exception {
        String self = L6W104ParkNegativeEvent.class.getName();
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
        cmd.add("--add-exports");
        cmd.add("java.base/jdk.internal.misc=ALL-UNNAMED");
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
