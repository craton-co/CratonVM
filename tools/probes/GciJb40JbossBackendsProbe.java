// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gci i40/jb -- jboss-logging over its slf4j and log4j2 providers
 * ({@code gci-i32jh-compatible-jboss-and-juli-console-sinks-are-not-hotspots-RETIRED-UNFIXED-20261018}, JBoss half).
 *
 * <pre>
 *   CP = these classes + jboss-logging-3.6.3.Final, slf4j-api-2.0.17, slf4j-simple-2.0.17,
 *        log4j-api-2.25.2 and log4j-core-2.25.2 unpacked (NO jboss-logmanager on this CP)
 *   javac -cp $CP -d $CP GciJb40JbossBackendsProbe.java
 *   java -Dorg.jboss.logging.provider=slf4j -Dorg.slf4j.simpleLogger.logFile=System.out -Xmx64m \
 *        GciJb40JbossBackendsProbe slf4j
 *   java -Dorg.jboss.logging.provider=log4j2 -Xmx64m GciJb40JbossBackendsProbe log4j2
 * </pre>
 *
 * The probe logs through the jboss-logging API, whose concrete backends ({@code Slf4jLogger} or
 * {@code Slf4jLocationAwareLogger}, {@code Log4j2Logger}) implement {@code doLog} / {@code doLogf}. Until gci i40/jb
 * CratonVM's {@code --compatible} registry carried a console stand-in for both methods on every backend
 * ({@code native_jboss_logging_logger_do_log*}), which printed {@code LEVEL [name] msg} on stdout.
 *
 * HotSpot 25: in {@code slf4j} mode slf4j-simple writes {@code [main] INFO jb40.slf - JB40B-...} lines on STDOUT
 * (the {@code logFile=System.out} property; default level INFO, so the DEBUG / TRACE records are dropped). In
 * {@code log4j2} mode log4j-core runs its default configuration (root at ERROR, console on stdout) and every record
 * here is below ERROR: nothing on stdout but the probe's own lines. Neither backend ever prints a line starting
 * {@code <LEVEL> [jb40.}: such a line on stdout is the stand-in sink. No throwable is logged (no stack trace on
 * stdout to compare).
 */
public class GciJb40JbossBackendsProbe {
    static void jbossLogging(org.jboss.logging.Logger l, String stage) {
        l.info("JB40B-" + stage + "-INFO");
        l.infof("JB40B-" + stage + "-INFOF %s", "p1");
        l.infov("JB40B-" + stage + "-INFOV {0}", "p1");
        l.warn("JB40B-" + stage + "-WARN");
        l.warnf("JB40B-" + stage + "-WARNF %s %s", "p1", "p2");
        l.debug("JB40B-" + stage + "-DEBUG");
        l.debugf("JB40B-" + stage + "-DEBUGF %s", "p1");
        l.tracef("JB40B-" + stage + "-TRACEF %s", "p1");
        l.log(org.jboss.logging.Logger.Level.INFO, "JB40B-" + stage + "-LOG");
        l.logf(org.jboss.logging.Logger.Level.WARN, "JB40B-" + stage + "-LOGF %d", 42);
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "slf4j";
        System.out.println("jb40b mode=" + mode + " start");
        String name = mode.equals("log4j2") ? "jb40.l4j2" : "jb40.slf";
        org.jboss.logging.Logger l = org.jboss.logging.Logger.getLogger(name);
        System.out.println("jb40b backend " + l.getClass().getName());
        System.out.println("jb40b info enabled " + l.isInfoEnabled() + " debug enabled " + l.isDebugEnabled());
        jbossLogging(l, "D");
        System.out.flush();
        System.out.println("jb40b d done");
        // A hot loop through one call site, so a compiled caller's door is exercised too (DEBUG: dropped by
        // both backends' default level on HotSpot; the stand-in filtered DEBUG only under its own level model).
        for (int i = 0; i < 3000; i++) {
            l.debugf("JB40B-H-HOT %d", i);
            l.trace("JB40B-H-HOT-TRACE");
        }
        l.info("JB40B-H-AFTER-HOT");
        System.out.flush();
        System.out.println("jb40b h done");
        System.out.println("jb40b mode=" + mode + " done");
    }
}
