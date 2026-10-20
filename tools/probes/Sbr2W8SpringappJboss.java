import org.jboss.logging.Logger;

/**
 * SBR2 W8 SPRINGAPP: org.jboss.logging.Slf4jLocationAwareLogger.doLog (registered native over a real class).
 * Needs jboss-logging, slf4j-api 2.x, logback-classic/core 1.3.x on the class path and a logback.xml whose
 * pattern is "%level %logger{0} [%thread] %msg%n" (no timestamps), level TRACE.
 */
public class Sbr2W8SpringappJboss {
    public static void main(String[] a) {
        Logger l = Logger.getLogger(Sbr2W8SpringappJboss.class);
        System.out.println("impl = " + l.getClass().getName());
        System.out.println("enabled = " + l.isTraceEnabled() + l.isDebugEnabled() + l.isInfoEnabled());
        l.trace("trace msg");
        l.debug("debug msg");
        l.info("info msg");
        l.warn("warn msg");
        l.error("error msg");
        l.fatal("fatal msg");
        l.infov("v {0} and {1}", "x", 42);
        l.infof("f %s and %d", "y", 7);
        l.warnf(new RuntimeException("boom"), "with thrown %s", "z");
        l.error("error with thrown", new IllegalStateException("ise"));
        l.log(Logger.Level.DEBUG, "generic level");
        l.log(Logger.Level.TRACE, (Object) "generic trace", new Object[] { 1 }, (Throwable) null);
        l.tracef("tracef %s", "t");
        l.debugv("debugv {0}", "d");
        l.info(null);
        l.info("obj", (Throwable) null);
        l.infof("no args");
        try { l.infof("bad %d", "notanumber"); } catch (RuntimeException e) { System.out.println("bad -> " + e.getClass().getName()); }
        Logger.getLogger("named.logger").info("from named");
        Logger.getLogger("named.logger").log("fq.Class", Logger.Level.INFO, "fqcn msg", null, null);
        System.out.println("done");
    }
}
