// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Predicts the post-fix answer for W7-56-infercaller-strict.md WITHOUT a
// rebuild.
//
// SrcProbe3 proved the `--jdk-only` source pair is null because the shadow
// `LogRecord.getSourceClassName` is the real getter with `inferCaller()`
// deleted -- not because `StackWalker` or the setters are broken. Retiring the
// shadow makes the REAL `inferCaller()` run, and the question that remains is
// whether `LogRecord$CallerFinder` -- run at ITS depth, inside a Formatter
// invoked by a real StreamHandler.publish, not at Handler.publish depth where
// SrcProbe3 sampled -- finds the caller on our StackWalker.
//
// So run CallerFinder's own algorithm, transcribed from JDK 25
// java.util.logging.LogRecord$CallerFinder, from exactly that depth. Whatever
// this prints is what the retired-shadow build will stamp into the record.
//
//   java -cp probes SrcProbe4
//   cratonvm --java-home <jdk> --real-jdk -cp probes SrcProbe4
//   cratonvm --java-home <jdk> --jdk-only -cp probes SrcProbe4
import java.util.Optional;
import java.util.function.Predicate;
import java.util.logging.Formatter;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import java.util.logging.SimpleFormatter;
import java.util.logging.StreamHandler;

public class SrcProbe4 {

    /** JDK 25 `LogRecord$CallerFinder`, transcribed. */
    static final class CallerFinder implements Predicate<StackWalker.StackFrame> {
        private static final StackWalker WALKER =
                StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

        Optional<StackWalker.StackFrame> get() {
            return WALKER.walk(s -> s.filter(this).findFirst());
        }

        private boolean lookingForLogger = true;

        @Override
        public boolean test(StackWalker.StackFrame t) {
            final String cname = t.getClassName();
            if (lookingForLogger) {
                lookingForLogger = !isLoggerImplFrame(cname);
                return false;
            }
            return !isFilteredFrame(t);
        }

        private boolean isLoggerImplFrame(String cname) {
            return cname.equals("java.util.logging.Logger")
                    || cname.startsWith("sun.util.logging.PlatformLogger");
        }

        // `Logger.isFilteredFrame` is package-private; this is its documented
        // effect -- skip the logging and reflection infrastructure.
        private boolean isFilteredFrame(StackWalker.StackFrame t) {
            String c = t.getClassName();
            return c.startsWith("java.util.logging.")
                    || c.startsWith("sun.util.logging.")
                    || c.startsWith("java.lang.reflect.")
                    || c.startsWith("jdk.internal.reflect.")
                    || c.startsWith("java.lang.invoke.");
        }
    }

    /**
     * Runs the walk at inferCaller's real depth: a Formatter called by a real
     * StreamHandler.publish, which is called by Logger.log.
     */
    static final class Probe extends Formatter {
        @Override
        public String format(LogRecord r) {
            Optional<StackWalker.StackFrame> f = new CallerFinder().get();
            System.out.println("E callerfinder="
                    + f.map(x -> x.getClassName() + " " + x.getMethodName()).orElse("EMPTY"));
            return new SimpleFormatter().format(r);
        }
    }

    public static void main(String[] args) {
        Logger log = Logger.getLogger("srcprobe4.one");
        log.setUseParentHandlers(false);
        StreamHandler h = new StreamHandler(System.out, new Probe());
        h.setLevel(Level.ALL);
        log.addHandler(h);
        log.warning("MARK4");
        h.flush();
        System.out.println("DONE SrcProbe4");
    }
}
