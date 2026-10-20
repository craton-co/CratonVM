// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.function.Consumer;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogManager;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

/**
 * gci i40/jb -- does any dispatch door reach a {@code --compatible} JUL native that asks the console-sink predicate
 * ({@code logmanager.rs} {@code jul_console_sink_applies})? The J4 attribution row of {@code vi40-jb.list}
 * ({@code gci-i32jh-compatible-jboss-and-juli-console-sinks-are-not-hotspots-RETIRED-UNFIXED-20261018}; Batch 9b: the i38 probe's
 * doors never asked it).
 *
 * <pre>
 *   CP = these classes + tomcat-embed-core-11.0.7.jar unpacked (the i38 cpj dir)
 *   java -Djava.util.logging.manager=org.apache.juli.ClassLoaderLogManager -Xmx64m GciJb40JuliDoorsProbe
 * </pre>
 *
 * Every root handler is removed first, so on HotSpot 25 no record of this probe reaches any stream: stdout is the
 * {@code jb40j} lines only. Each record goes through a different door: a direct {@code invokevirtual}, an inherited
 * method on a subclass receiver (JULI's {@code RootLogger}, an anonymous {@code Logger} subclass), reflection, a
 * {@code MethodHandle}, a method reference, the {@code log} / {@code logp} overloads, a hot loop through one call
 * site (compiled caller), and JULI's {@code DirectJDKLog}. Where a door reaches a JUL native that falls back to the
 * console sink, {@code CRATONVM_JULI_NO_CONSOLE_SINK=0} prints {@code LEVEL [name] JB40J-...} on stdout and
 * {@code CRATONVM_DBG_JUL=1} prints {@code [JUL-DBG] console-sink asked: manager=juli ...} on stderr.
 */
public class GciJb40JuliDoorsProbe {
    static List<String> handlerNames(Logger l) {
        List<String> names = new ArrayList<>();
        for (Handler h : l.getHandlers()) {
            names.add(h.getClass().getName());
        }
        Collections.sort(names);
        return names;
    }

    static void hot(Logger l, String m) {
        l.info(m);
    }

    public static void main(String[] args) throws Throwable {
        System.out.println("jb40j start");
        System.out.println("jb40j manager " + LogManager.getLogManager().getClass().getName());
        Logger root = Logger.getLogger("");
        for (Handler h : root.getHandlers()) {
            root.removeHandler(h);
        }
        System.out.println("jb40j root " + root.getClass().getName() + " handlers " + handlerNames(root));
        Logger a = Logger.getLogger("jb40.juli.a");
        System.out.println("jb40j a handlers " + handlerNames(a) + " useParent " + a.getUseParentHandlers());

        a.info("JB40J-E-DIRECT");
        root.info("JB40J-E-ROOT");
        Logger sub = new Logger("jb40.juli.sub", null) {
        };
        System.out.println("jb40j sub added " + LogManager.getLogManager().addLogger(sub));
        sub.info("JB40J-E-SUB");

        Method info = Logger.class.getMethod("info", String.class);
        info.invoke(a, "JB40J-E-REFLECT");
        MethodHandle mh = MethodHandles.lookup()
                .findVirtual(Logger.class, "info", MethodType.methodType(void.class, String.class));
        mh.invokeExact(a, "JB40J-E-MH");
        Consumer<String> ref = a::info;
        ref.accept("JB40J-E-REF");

        a.warning("JB40J-E-WARN");
        a.severe("JB40J-E-SEVERE");
        a.log(Level.INFO, "JB40J-E-LOG");
        a.log(Level.INFO, "JB40J-E-PARAM {0}", "p1");
        a.log(Level.INFO, "JB40J-E-PARAMS {0} {1}", new Object[] {"p1", "p2"});
        a.log(Level.WARNING, "JB40J-E-THROWN", new IllegalStateException("JB40J-E-ISE"));
        a.log(Level.INFO, () -> "JB40J-E-SUPPLIER");
        a.logp(Level.INFO, "GciJb40JuliDoorsProbe", "main", "JB40J-E-LOGP");
        LogRecord rec = new LogRecord(Level.INFO, "JB40J-E-RECORD");
        rec.setLoggerName(a.getName());
        a.log(rec);
        System.out.println("jb40j doors done");

        for (int i = 0; i < 3000; i++) {
            hot(a, "JB40J-E-HOT");
        }
        System.out.println("jb40j hot done");

        org.apache.juli.logging.Log jl = org.apache.juli.logging.LogFactory.getLog("jb40.juli.log");
        jl.info("JB40J-E-JULILOG-INFO");
        jl.error("JB40J-E-JULILOG-ERROR");
        System.err.flush();
        System.out.println("jb40j done");
    }
}
