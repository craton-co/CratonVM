import java.io.IOException;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.stream.Stream;

/**
 * Opens a jar {@link FileSystem} per jar, walks every entry and drops it
 * unclosed, round after round -- what Spring's in-process {@code javac} tests
 * do through {@code JavacFileManager}. {@code jdk.nio.zipfs.ZipFileSystem} has
 * a {@code finalize()}, so every dropped one is a finalizable object whose
 * entry index stays reachable until a collection cycle has run its finalizer:
 * the G1 remark traces that index from the finalizer roots.
 *
 * <p>Usage: {@code ZipFsFinalizerChurn <jar-list-file> <rounds>}. Prints one
 * line per round (entries walked, which is deterministic) and the elapsed time
 * on stderr, so stdout can be diffed against HotSpot.
 *
 * <p>Page: {@code docs/feature-designs/jit-ir-optimizations.md}.
 */
public class ZipFsFinalizerChurn {
    public static void main(String[] args) throws IOException {
        List<String> jars = Files.readAllLines(Path.of(args[0]));
        int rounds = Integer.parseInt(args[1]);
        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            long entries = 0;
            for (String jar : jars) {
                if (jar.isBlank()) {
                    continue;
                }
                // The Path form, as javac's FSInfo opens a jar: not registered
                // with the provider, so nothing but finalize() closes it.
                FileSystem fs = FileSystems.newFileSystem(Path.of(jar), Map.of());
                try (Stream<Path> walk = Files.walk(fs.getPath("/"))) {
                    entries += walk.count();
                }
                // Deliberately not closed: javac's file manager leaves it to finalize().
            }
            System.out.println("round " + r + " entries " + entries);
        }
        System.err.printf("elapsed_ms %d%n", (System.nanoTime() - t0) / 1_000_000);
    }
}
