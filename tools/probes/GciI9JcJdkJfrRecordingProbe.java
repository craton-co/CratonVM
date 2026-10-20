// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Arrays;
import jdk.jfr.Recording;

/**
 * gci i9/jc (2026-10-13): run another probe's {@code main} under a recording
 * started through the JDK's OWN {@code jdk.jfr} API ({@code new Recording()}),
 * not {@code -XX:StartFlightRecording}, and dump it to a file for a host-java
 * checker ({@code GciI5gG1oJfrCycleCheck}, {@code GcfF3cJfrPhaseOrder},
 * {@code GciI9JcJfrNestingCheck}).
 * ({@code docs/known-issues/gc/gci-i8jf-jfr-cycle-rows-never-reach-the-jdk-jfr-backend-20261013.md}:
 * on CratonVM such a recording is served by the {@code jdk.jfr} backend,
 * which had no concurrent-cycle rows.)
 *
 * <pre>
 *   java -Djc9.out=/path/x.jfr -cp &lt;classes&gt; GciI9JcJdkJfrRecordingProbe &lt;WorkloadClass&gt; [args...]
 * </pre>
 * The dump path is the system property {@code jc9.out} (not an argument, so a
 * battery row can give CratonVM and HotSpot different files through their own
 * flags). The recording enables, with a 0 ms threshold, the GC collection and
 * phase events the checkers read. stdout is the workload's own stdout,
 * unchanged (so a row compares it with HotSpot's byte for byte); this class
 * writes only to stderr ({@code [jc9] dumped events-file=<path>}).
 */
public final class GciI9JcJdkJfrRecordingProbe {
    private static final String[] EVENTS = {
        "jdk.GarbageCollection",
        "jdk.YoungGarbageCollection",
        "jdk.OldGarbageCollection",
        "jdk.GCPhasePause",
        "jdk.GCPhasePauseLevel1",
        "jdk.GCPhaseConcurrent",
        "jdk.GCPhaseConcurrentLevel1",
    };

    public static void main(String[] args) throws Throwable {
        String out = System.getProperty("jc9.out");
        if (out == null || args.length < 1) {
            System.err.println("[jc9] usage: -Djc9.out=<file.jfr> GciI9JcJdkJfrRecordingProbe <WorkloadClass> [args...]");
            System.exit(2);
        }
        Method main = Class.forName(args[0]).getMethod("main", String[].class);
        String[] rest = Arrays.copyOfRange(args, 1, args.length);
        try (Recording recording = new Recording()) {
            recording.setName("gci-i9-jc");
            for (String event : EVENTS) {
                recording.enable(event).withThreshold(Duration.ZERO);
            }
            recording.start();
            try {
                main.invoke(null, (Object) rest);
            } catch (InvocationTargetException e) {
                throw e.getCause();
            }
            recording.stop();
            recording.dump(Path.of(out));
        }
        System.err.println("[jc9] dumped events-file=" + out);
    }
}
