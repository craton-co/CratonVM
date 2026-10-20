/**
 * gcf f2/r (2026-10-01): the {@code gc,exit} heap summary HotSpot 25 Serial
 * prints once at VM exit under {@code -Xlog:gc+exit} / {@code -Xlog:gc*}
 * (gcf-f1r-xlog-exit-heap-summary-is-tag-gc-exit-and-not-printed).
 *
 * <p>Allocates 4,000 8 KiB arrays (young collections), calls
 * {@code System.gc()} once, prints {@code done}. Mode {@code exit} ends through
 * {@code System.exit(0)} (the launcher's other exit arm), mode {@code throw}
 * through an uncaught exception. Judge the {@code [gc,exit]} lines by shape,
 * not bytes (sizes and addresses differ): {@code Heap}, {@code  DefNew     total},
 * {@code   eden space}, {@code   from space}, {@code   to   space},
 * {@code  Tenured    total}, {@code   the  space}, {@code  Metaspace       used},
 * each exactly once.
 */
public class GcfXlogExitProbe {
    static Object keep;

    public static void main(String[] args) {
        for (int i = 0; i < 4000; i++) {
            keep = new byte[8192];
        }
        System.gc();
        System.out.println("done");
        String mode = args.length > 0 ? args[0] : "";
        if (mode.equals("exit")) {
            System.exit(0);
        }
        if (mode.equals("throw")) {
            throw new IllegalStateException("exit through an uncaught exception");
        }
    }
}
