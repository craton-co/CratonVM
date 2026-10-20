// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 120, lane L5: the `java.util.logging` surface the
// `jdk-logging` HotSpot-alignment row moves onto the JDK's own bodies under
// `--compatible` (`i52-L5-proposal-a-compatible-row-per-retired-shadow-family`,
// "Wave 120"; row default OFF). Every line must be HotSpot's under
// `--jdk-only`, and under `--compatible` with the row on; with it off,
// `--compatible` is the VM's stand-ins (a line that differs there is what the
// row fixes). Nothing is printed through a console handler (stderr is not
// compared), and the file handler writes to a temp file it reads back.
//
// Run: javac -d . L5W120JulFamily.java
//      java -cp . L5W120JulFamily                                      (HotSpot)
//      cratonvm --jdk-only -cp . L5W120JulFamily
//      cratonvm --compatible -cp . L5W120JulFamily
//      CRATONVM_COMPAT_JDK_LOGGING=1 cratonvm --compatible -cp . L5W120JulFamily
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// same-logger true parent l5w120 grand-parent-is-root true
// level-null null effective-from-parent FINE
// loggable-fine true loggable-finest false
// records 2 [INFO a.b, WARNING x=7]
// source null null
// parse INFO 800 SEVERE custom-int 333
// parse-bad java.lang.IllegalArgumentException: Bad level "NOPE"
// level-compare true intValue 900
// names-contain true global global
// formatted [FINE] fmt one two
// file-lines 1 contains-msg true
// filter-dropped 1
// handler-level-off 0
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.logging.FileHandler;
import java.util.logging.Formatter;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogManager;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

public class L5W120JulFamily {
    static final class Capture extends Handler {
        final List<LogRecord> records = new ArrayList<>();

        @Override
        public void publish(LogRecord r) {
            if (isLoggable(r)) {
                records.add(r);
            }
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    }

    public static void main(String[] args) throws Exception {
        Logger parent = Logger.getLogger("l5w120");
        parent.setUseParentHandlers(false);
        Logger child = Logger.getLogger("l5w120.child");
        System.out.println("same-logger " + (child == Logger.getLogger("l5w120.child")) + " parent "
                + child.getParent().getName() + " grand-parent-is-root "
                + (parent.getParent() == Logger.getLogger("")));

        parent.setLevel(Level.FINE);
        Level eff = null;
        for (Logger l = child; l != null; l = l.getParent()) {
            if (l.getLevel() != null) {
                eff = l.getLevel();
                break;
            }
        }
        System.out.println("level-null " + child.getLevel() + " effective-from-parent " + eff);
        System.out.println("loggable-fine " + child.isLoggable(Level.FINE) + " loggable-finest "
                + child.isLoggable(Level.FINEST));

        Capture cap = new Capture();
        cap.setLevel(Level.ALL);
        parent.addHandler(cap);
        child.info("a.b");
        child.log(Level.WARNING, "x={0}", 7);
        child.finest("dropped");
        List<String> shown = new ArrayList<>();
        for (LogRecord r : cap.records) {
            String msg = r.getParameters() == null ? r.getMessage()
                    : java.text.MessageFormat.format(r.getMessage(), r.getParameters());
            shown.add(r.getLevel() + " " + msg);
        }
        System.out.println("records " + cap.records.size() + " " + shown);
        // The source is inferred lazily, at the first getter call, from the
        // stack THEN: the logging call has returned, so JDK 25 finds no caller
        // frame and answers null (a stand-in that recorded it eagerly differs).
        LogRecord first = cap.records.get(0);
        System.out.println("source " + first.getSourceClassName() + " " + first.getSourceMethodName());

        System.out.println("parse " + Level.parse("INFO") + " " + Level.parse("800").intValue() + " "
                + Level.parse("1000") + " custom-int " + Level.parse("333").intValue());
        try {
            Level.parse("NOPE");
            System.out.println("parse-bad ok");
        } catch (IllegalArgumentException e) {
            System.out.println("parse-bad " + e);
        }
        System.out.println("level-compare " + (Level.WARNING.intValue() > Level.INFO.intValue())
                + " intValue " + Level.WARNING.intValue());

        List<String> names = Collections.list(LogManager.getLogManager().getLoggerNames());
        System.out.println("names-contain " + names.contains("l5w120.child") + " global "
                + Logger.getGlobal().getName());

        Formatter f = new Formatter() {
            @Override
            public String format(LogRecord r) {
                return "[" + r.getLevel() + "] " + formatMessage(r);
            }
        };
        LogRecord rec = new LogRecord(Level.FINE, "fmt {0} {1}");
        rec.setParameters(new Object[] {"one", "two"});
        System.out.println("formatted " + f.format(rec));

        File dir = Files.createTempDirectory("l5w120jul").toFile();
        String pattern = new File(dir, "log%g.txt").getPath();
        FileHandler fh = new FileHandler(pattern, false);
        fh.setFormatter(new Formatter() {
            @Override
            public String format(LogRecord r) {
                return r.getLevel() + ":" + r.getMessage() + "\n";
            }
        });
        Logger fl = Logger.getLogger("l5w120.file");
        fl.setUseParentHandlers(false);
        fl.addHandler(fh);
        fl.info("to-file");
        fh.close();
        List<String> lines = Files.readAllLines(new File(dir, "log0.txt").toPath());
        System.out.println("file-lines " + lines.size() + " contains-msg " + lines.get(0).contains("to-file"));

        Capture filtered = new Capture();
        filtered.setFilter(r -> !r.getMessage().startsWith("skip"));
        Logger fl2 = Logger.getLogger("l5w120.filter");
        fl2.setUseParentHandlers(false);
        fl2.addHandler(filtered);
        fl2.info("skip me");
        fl2.info("keep me");
        System.out.println("filter-dropped " + filtered.records.size());

        Capture off = new Capture();
        off.setLevel(Level.OFF);
        Logger fl3 = Logger.getLogger("l5w120.off");
        fl3.setUseParentHandlers(false);
        fl3.addHandler(off);
        fl3.severe("nothing");
        System.out.println("handler-level-off " + off.records.size());

        for (File x : dir.listFiles()) {
            x.delete();
        }
        dir.delete();
    }
}
