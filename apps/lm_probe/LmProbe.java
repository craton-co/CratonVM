import java.util.logging.LogManager;
import java.util.logging.Logger;

public class LmProbe {
    public static void main(String[] args) throws Exception {
        LogManager lm = LogManager.getLogManager();
        Logger logger = lm.getLogger("test");
        if (logger == null) {
            // Default JUL behaviour: "test" logger does not exist until requested via Logger.getLogger
            logger = Logger.getLogger("test");
        }
        logger.info("hello");
        System.out.println("LmProbe: PASS lm=" + (lm != null) + " logger=" + (logger != null));
    }
}
