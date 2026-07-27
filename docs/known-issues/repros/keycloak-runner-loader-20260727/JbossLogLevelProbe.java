import org.jboss.logging.Logger;

/**
 * Does CratonVM honour jboss-logging level filtering? HotSpot prints only the
 * INFO line; a VM whose doLog/doLogf natives emit unconditionally prints all
 * four, with %d/%b left unsubstituted.
 */
public class JbossLogLevelProbe {
    public static void main(String[] args) {
        Logger l = Logger.getLogger("org.hibernate.orm.boot");
        System.out.println("impl=" + l.getClass().getName());
        System.out.println("isTraceEnabled=" + l.isTraceEnabled()
                + " isDebugEnabled=" + l.isDebugEnabled()
                + " isInfoEnabled=" + l.isInfoEnabled());
        l.trace("MARKER-TRACE plain");
        l.tracef("MARKER-TRACEF %d", 42);
        l.debugf("MARKER-DEBUGF %b", true);
        l.info("MARKER-INFO plain");
        l.infof("MARKER-INFOF %d/%s", 7, "seven");
        System.out.println("done");
    }
}
