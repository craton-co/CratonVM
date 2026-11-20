import com.thoughtworks.qdox.JavaProjectBuilder;
import com.thoughtworks.qdox.model.JavaSource;
import java.io.StringReader;
import java.nio.file.Files;
import java.nio.file.Paths;

/**
 * Single-shot, single-thread reproducer: parse one file's content with a
 * fresh JavaProjectBuilder, exactly the way Spring's
 * SourceFile.getClassName(String) does. Used to rule OUT a single-threaded
 * QDox defect (it always succeeds, both on HotSpot and CratonVM) before
 * reaching for the concurrent variant (QdoxConcProbe.java) that reproduces
 * the failure. See qdox-parser-static-table-race-under-concurrent-first-use-20260905.md.
 */
public class QdoxProbe2 {
    public static void main(String[] args) throws Exception {
        String source = new String(Files.readAllBytes(Paths.get(args[0])));
        JavaProjectBuilder builder = new JavaProjectBuilder();
        try {
            JavaSource js = builder.addSource(new StringReader(source));
            System.out.println("OK classes=" + js.getClasses().size()
                    + " name=" + js.getClasses().get(0).getBinaryName());
        } catch (Throwable t) {
            System.out.println("FAILED: " + t);
            t.printStackTrace(System.out);
        }
    }
}
