import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

/**
 * gci i26/rv (item J1 of gci-i26rv-i25-default-path-review-findings-20261015):
 * a logger silenced with setUseParentHandlers(false) and no handler of its
 * own prints nothing on HotSpot 25 (Logger.log breaks out of the parent walk
 * before the root's ConsoleHandler). --compatible's console sink (which
 * stands in for that ConsoleHandler) printed every record to stdout as
 * "INFO [gcirv26.q] RV26-SILENCED-1".
 *
 * stdout on HotSpot: exactly one line,
 * "rv26 mode=silenced done useParent(q)=false handlers(q)=0".
 * No logger here reaches the root, so nothing reaches stderr either.
 */
public class GciRv26JulSilencedProbe {
    public static void main(String[] args) {
        Logger q = Logger.getLogger("gcirv26.q");
        q.setUseParentHandlers(false);
        q.info("RV26-SILENCED-1");
        q.log(Level.WARNING, "RV26-SILENCED-2");
        q.log(Level.WARNING, "RV26-SILENCED-3 {0}", "p");
        q.log(Level.WARNING, "RV26-SILENCED-4 {0} {1}", new Object[] {"a", "b"});
        q.logp(Level.SEVERE, "C", "m", "RV26-SILENCED-5");
        q.log(new LogRecord(Level.INFO, "RV26-SILENCED-6"));
        Logger qr = Logger.getLogger("gcirv26.q.r");
        qr.info("RV26-SILENCED-7");
        System.out.println("rv26 mode=silenced done useParent(q)=" + q.getUseParentHandlers()
                + " handlers(q)=" + q.getHandlers().length);
    }
}
