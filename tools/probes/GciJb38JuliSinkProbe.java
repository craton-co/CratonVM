// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogManager;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import java.util.logging.SimpleFormatter;

/**
 * gci i38/jb -- JUL under Tomcat's JULI LogManager
 * ({@code gci-i32jh-compatible-jboss-and-juli-console-sinks-are-not-hotspots-RETIRED-UNFIXED-20261018}).
 *
 * <pre>
 *   CP = these classes + tomcat-embed-core-11.0.7.jar unpacked (org/apache/juli/*)
 *   java     -Djava.util.logging.manager=org.apache.juli.ClassLoaderLogManager -Xmx64m GciJb38JuliSinkProbe
 *   cratonvm -Djava.util.logging.manager=org.apache.juli.ClassLoaderLogManager --compatible -Xmx64m GciJb38JuliSinkProbe
 * </pre>
 *
 * HotSpot 25: no {@code catalina.base} and no {@code java.util.logging.config.file}, so JULI configures the system
 * loader from {@code $JAVA_HOME/conf/logging.properties}: the root gets a {@code java.util.logging.ConsoleHandler}
 * (STDERR). Stage {@code a} prints on stderr only. Stage {@code d} replaces the root's handlers with a probe handler
 * that prints {@code H <LEVEL> [<name>] <message>} on STDOUT (deterministic). Stage {@code e} removes every root
 * handler: no record of that stage reaches any stream on HotSpot. CratonVM's {@code --compatible} console sink printed
 * {@code LEVEL [name] msg} on stdout for a record no handler took (the JULI arm of {@code jul_console_sink_applies});
 * an {@code E-} needle on stdout is that sink.
 *
 * Expected stdout (HotSpot 25): the {@code jb38j} lines and the {@code H ...} lines of stage {@code d}; nothing
 * containing {@code JB38J-A-} or {@code JB38J-E-}.
 */
public class GciJb38JuliSinkProbe {
    static final SimpleFormatter FMT = new SimpleFormatter();

    static final class StdoutHandler extends Handler {
        @Override
        public void publish(LogRecord r) {
            if (!isLoggable(r)) {
                return;
            }
            System.out.println("H " + r.getLevel().getName() + " [" + r.getLoggerName() + "] " + FMT.formatMessage(r));
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    }

    static List<String> handlerNames(Logger l) {
        List<String> names = new ArrayList<>();
        for (Handler h : l.getHandlers()) {
            names.add(h.getClass().getName());
        }
        Collections.sort(names);
        return names;
    }

    static void records(Logger a, String stage) {
        a.info("JB38J-" + stage + "-INFO");
        a.warning("JB38J-" + stage + "-WARN");
        a.severe("JB38J-" + stage + "-SEVERE");
        a.fine("JB38J-" + stage + "-FINE");
        a.log(Level.INFO, "JB38J-" + stage + "-PARAM {0}", "p1");
        a.log(Level.INFO, "JB38J-" + stage + "-PARAMS {0} {1}", new Object[] {"p1", "p2"});
        a.log(Level.WARNING, "JB38J-" + stage + "-THROWN", new IllegalStateException("JB38J-" + stage + "-ISE"));
        a.logp(Level.INFO, "GciJb38JuliSinkProbe", "records", "JB38J-" + stage + "-LOGP");
        LogRecord rec = new LogRecord(Level.INFO, "JB38J-" + stage + "-RECORD");
        rec.setLoggerName(a.getName());
        a.log(rec);
        org.apache.juli.logging.Log jl = org.apache.juli.logging.LogFactory.getLog("jb38.juli.log");
        jl.info("JB38J-" + stage + "-JULILOG-INFO");
        jl.error("JB38J-" + stage + "-JULILOG-ERROR");
        jl.debug("JB38J-" + stage + "-JULILOG-DEBUG");
    }

    public static void main(String[] args) {
        System.out.println("jb38j start");
        System.out.println("jb38j manager " + LogManager.getLogManager().getClass().getName());
        Logger root = Logger.getLogger("");
        Logger a = Logger.getLogger("jb38.juli.a");
        System.out.println("jb38j root handlers " + handlerNames(root));
        System.out.println("jb38j a handlers " + handlerNames(a) + " useParent " + a.getUseParentHandlers());

        // a: the JRE default configuration's ConsoleHandler (stderr only).
        records(a, "A");
        System.err.flush();
        System.out.println("jb38j a done");

        // d: a probe handler on the root, printing on stdout.
        for (Handler h : root.getHandlers()) {
            root.removeHandler(h);
        }
        StdoutHandler out = new StdoutHandler();
        out.setLevel(Level.ALL);
        root.addHandler(out);
        System.out.println("jb38j d root handlers " + handlerNames(root));
        records(a, "D");
        System.out.println("jb38j d done");

        // e: no handler anywhere -- HotSpot drops every record.
        root.removeHandler(out);
        System.out.println("jb38j e root handlers " + handlerNames(root));
        records(a, "E");
        System.err.flush();
        System.out.println("jb38j e done");
        System.out.println("jb38j done");
    }
}
