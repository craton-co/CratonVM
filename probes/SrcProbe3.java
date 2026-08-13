// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Discriminator for the `--jdk-only` `LogRecord` source-pair defect
// (jul-logrecord-infercaller-is-inert-under-jdk-only-20260812.md).
//
// Two candidate causes produce the SAME null symptom, so reading cannot tell
// them apart:
//
//   (2) `setSourceClassName`/`setSourceMethodName` write somewhere the real
//       getter never reads  -> section A round-trip FAILS.
//   (1) `StackWalker` does not present the `java.util.logging.Logger` frames
//       `LogRecord$CallerFinder` walks for -> section A round-trip PASSES but
//       section C shows no `java.util.logging.Logger` frame.
//
// Run all three arms and diff:
//   java     --add-opens java.logging/java.util.logging=ALL-UNNAMED -cp probes SrcProbe3
//   cratonvm --real-jdk --add-opens=java.logging/java.util.logging=ALL-UNNAMED -cp probes SrcProbe3
//   cratonvm --jdk-only --add-opens=java.logging/java.util.logging=ALL-UNNAMED -cp probes SrcProbe3
//
// USE THE `=` SPELLING ON CRATONVM. The space-separated form HotSpot accepts
// (`--add-opens M/P=T`) makes CratonVM's launcher swallow the following `-cp`,
// and the run dies with "Could not find or load main class" -- which reads as
// a broken probe rather than a mis-parsed flag. Without add-opens, section
// A2/A3/A4 fails soft with InaccessibleObjectException; A4 is the line that
// located this defect, so a soft-failed run answers nothing.
import java.util.List;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import java.util.stream.Collectors;

public class SrcProbe3 {

    /** Reads the pair exactly where `SimpleFormatter.format` reads it. */
    static final class Probe extends Handler {
        @Override
        public void publish(LogRecord r) {
            System.out.println("B during-publish class=" + r.getSourceClassName()
                    + " method=" + r.getSourceMethodName());
            // D: does an explicit write stick on a record the LOGGER built?
            r.setSourceClassName("D_CLASS");
            r.setSourceMethodName("d_method");
            System.out.println("D publish-set-then-get class=" + r.getSourceClassName()
                    + " method=" + r.getSourceMethodName());
            // C: what stack does StackWalker present at inferCaller time?
            System.out.println("C frames=" + frames());
        }

        @Override
        public void flush() {}

        @Override
        public void close() {}
    }

    static List<String> frames() {
        return StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.map(f -> f.getClassName() + "." + f.getMethodName())
                        .collect(Collectors.toList()));
    }

    public static void main(String[] args) throws Exception {
        // A: plain setter/getter round-trip on a bytecode-constructed record.
        // This is the DECISIVE test for cause (2). HotSpot prints A_CLASS/a_method.
        LogRecord rec = new LogRecord(Level.WARNING, "roundtrip");
        rec.setSourceClassName("A_CLASS");
        rec.setSourceMethodName("a_method");
        System.out.println("A roundtrip class=" + rec.getSourceClassName()
                + " method=" + rec.getSourceMethodName());

        // A2: the same round-trip through the raw reflective field, to show
        // whether the setter reached the REAL declared field or a side slot.
        // Needs `--add-opens java.logging/java.util.logging=ALL-UNNAMED`;
        // fail soft so the decisive sections still run without it.
        LogRecord fresh = new LogRecord(Level.WARNING, "fresh");
        try {
            java.lang.reflect.Field f = LogRecord.class.getDeclaredField("sourceClassName");
            f.setAccessible(true);
            System.out.println("A2 field-after-set=" + f.get(rec));
            java.lang.reflect.Field nic = LogRecord.class.getDeclaredField("needToInferCaller");
            nic.setAccessible(true);
            System.out.println("A3 needToInferCaller-after-set=" + nic.get(rec));
            // A4: a FRESH record, untouched -- what does the real ctor leave?
            System.out.println("A4 fresh needToInferCaller=" + nic.get(fresh)
                    + " sourceClassName=" + f.get(fresh));
        } catch (Throwable t) {
            System.out.println("A2 reflect-unavailable " + t.getClass().getName());
        }

        // B/C/D: the real vector -- a Logger call reaching a Handler.
        Logger log = Logger.getLogger("srcprobe3.one");
        log.setUseParentHandlers(false);
        log.addHandler(new Probe());
        log.warning("MARK3");

        System.out.println("DONE SrcProbe3");
    }
}
