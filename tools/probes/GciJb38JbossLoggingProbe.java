// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogManager;
import java.util.logging.LogRecord;
import java.util.logging.SimpleFormatter;

/**
 * gci i38/jb -- jboss-logging over the JDK provider and over jboss-logmanager
 * ({@code gci-i32jh-compatible-jboss-and-juli-console-sinks-are-not-hotspots-RETIRED-UNFIXED-20261018}).
 *
 * <pre>
 *   CP = these classes + jboss-logging-3.6.3.Final, jboss-logmanager-3.2.2.Final and
 *        smallrye-common-{constraint,cpu,expression,function,net,os,ref}-2.16.0 unpacked
 *   javac -cp $CP -d $CP GciJb38JbossLoggingProbe.java
 *   java -Dorg.jboss.logging.provider=jdk -Xmx64m GciJb38JbossLoggingProbe jdk
 *   java -Djava.util.logging.manager=org.jboss.logmanager.LogManager -Dorg.jboss.logging.provider=jboss \
 *        -Xmx64m GciJb38JbossLoggingProbe lm
 * </pre>
 *
 * Both arms put one probe handler on the root of the backend (JUL root for {@code jdk}, the jboss-logmanager
 * root for {@code lm}) that prints {@code H <LEVEL> [<name>] <message>} on STDOUT, log through the jboss-logging
 * API (the backends' {@code doLog} / {@code doLogf}) and, for {@code lm}, through
 * {@code org.jboss.logmanager.Logger} directly ({@code log(Level, Supplier)}, {@code log(LogRecord)}, {@code info}
 * -> {@code logRaw}); then remove the root's handlers and log again. HotSpot 25: stage {@code d} prints the
 * {@code H} lines; stage {@code e} prints nothing on any stream. CratonVM's {@code --compatible} stand-ins
 * ({@code native_jboss_logging_logger_do_log*}, {@code native_jboss_logger_log_raw},
 * {@code native_jboss_logger_log_level_supplier}) print {@code LEVEL [name] msg} on stdout instead of (or as well
 * as) running the handler chain: any {@code JB38B-} needle on stdout outside an {@code H} line is that sink.
 */
public class GciJb38JbossLoggingProbe {
    static final SimpleFormatter FMT = new SimpleFormatter();

    static final class StdoutHandler extends Handler {
        @Override
        public void publish(LogRecord r) {
            if (!isLoggable(r)) {
                return;
            }
            String msg = r instanceof org.jboss.logmanager.ExtLogRecord
                    ? ((org.jboss.logmanager.ExtLogRecord) r).getFormattedMessage()
                    : FMT.formatMessage(r);
            System.out.println("H " + r.getLevel().getName() + " [" + r.getLoggerName() + "] " + msg);
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    }

    static List<String> names(Handler[] hs) {
        List<String> out = new ArrayList<>();
        for (Handler h : hs) {
            out.add(h.getClass().getName());
        }
        Collections.sort(out);
        return out;
    }

    static void jbossLogging(String stage) {
        org.jboss.logging.Logger l = org.jboss.logging.Logger.getLogger("jb38.jbl");
        l.info("JB38B-" + stage + "-INFO");
        l.infof("JB38B-" + stage + "-INFOF %s", "p1");
        l.infov("JB38B-" + stage + "-INFOV {0}", "p1");
        l.warn("JB38B-" + stage + "-WARN");
        l.error("JB38B-" + stage + "-ERROR", new IllegalStateException("JB38B-" + stage + "-ISE"));
        l.debug("JB38B-" + stage + "-DEBUG");
        l.log(org.jboss.logging.Logger.Level.INFO, "JB38B-" + stage + "-LOG");
    }

    static void logManagerDirect(String stage) {
        org.jboss.logmanager.Logger m = org.jboss.logmanager.LogContext.getLogContext().getLogger("jb38.lm");
        m.info("JB38B-" + stage + "-LM-INFO");
        final String s = "JB38B-" + stage + "-LM-SUPPLIER";
        m.log(Level.INFO, () -> s);
        LogRecord rec = new LogRecord(Level.WARNING, "JB38B-" + stage + "-LM-RECORD");
        rec.setLoggerName("jb38.lm");
        m.log(rec);
        m.fine("JB38B-" + stage + "-LM-FINE");
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "jdk";
        System.out.println("jb38b mode=" + mode + " start");
        System.out.println("jb38b manager " + LogManager.getLogManager().getClass().getName());
        StdoutHandler out = new StdoutHandler();
        out.setLevel(Level.ALL);
        if (mode.equals("lm")) {
            org.jboss.logmanager.Logger root = org.jboss.logmanager.LogContext.getLogContext().getLogger("");
            System.out.println("jb38b root handlers " + names(root.getHandlers()));
            root.setHandlers(new Handler[] {out});
            System.out.println("jb38b d root handlers " + names(root.getHandlers()));
            jbossLogging("D");
            logManagerDirect("D");
            System.out.println("jb38b d done");
            root.setHandlers(new Handler[0]);
            System.out.println("jb38b e root handlers " + names(root.getHandlers()));
            jbossLogging("E");
            logManagerDirect("E");
        } else {
            java.util.logging.Logger root = java.util.logging.Logger.getLogger("");
            System.out.println("jb38b root handlers " + names(root.getHandlers()));
            for (Handler h : root.getHandlers()) {
                root.removeHandler(h);
            }
            root.addHandler(out);
            System.out.println("jb38b d root handlers " + names(root.getHandlers()));
            jbossLogging("D");
            System.out.println("jb38b d done");
            root.removeHandler(out);
            System.out.println("jb38b e root handlers " + names(root.getHandlers()));
            jbossLogging("E");
        }
        System.err.flush();
        System.out.println("jb38b e done");
        System.out.println("jb38b mode=" + mode + " done");
    }
}
