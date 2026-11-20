package cratonvm;

import java.io.File;
import java.util.logging.Level;
import java.util.logging.Logger;

/**
 * Exercises the bytecode shape from Conscrypt NativeLibraryLoader.log at bci 22:
 * Logger.log(Level, String, Object[]) with File values in the argument array.
 */
public final class ConscryptLoggerDispatch {
    private static final Logger LOGGER = Logger.getLogger("cratonvm.conscrypt.dispatch");

    private static void log(String message, Object first, Object second) {
        LOGGER.log(Level.FINE, message, new Object[] { first, second });
    }

    public static void main(String[] args) {
        File workDir = new File("conscrypt-workdir");
        for (int i = 0; i < 20000; i++) {
            log("-D{0}: {1}", "org.conscrypt.native.workdir", workDir);
        }
        System.out.println("CONSCRYPT_LOGGER_DISPATCH_OK");
    }
}
