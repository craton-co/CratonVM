/**
 * gcj j1/ob: the launcher's typed {@code -XX:<Name>=<value>} table on the
 * Generational collector (HotSpot twin {@code -XX:+UseSerialGC}).
 *
 * <p>A refusal row never reaches {@code main}: HotSpot 25 prints
 * {@code Improperly specified VM option '<Name>=<value>'} (or a range line)
 * and the two {@code Error:} lines on stderr, nothing on stdout, exit 1. An
 * accepted row prints the one deterministic line below. The stderr text of a
 * refusal row is compared by hand against HotSpot's {@code .hse}.
 */
public class Gcj1ObValueFlags {
    public static void main(String[] args) {
        System.out.println("gcj1ob value-flags started");
    }
}
