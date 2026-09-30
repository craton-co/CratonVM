import java.io.EOFException;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.util.Locale;
import java.util.function.Supplier;

/**
 * Behavioural cover, and a retained-heap reading, for the synthetic slot model
 * of the THROWABLE hierarchy.
 *
 * `synthetic_stub_fields` gives `java.lang.Throwable` three fabricated slots --
 * `detailMessage`, `cause`, `suppressedExceptions`, which is exactly
 * `synthetic_throwable_slot`'s map. Every subclass INHERITS them through
 * `jdk_superclass`; none re-declares them. It used to be the other way round,
 * and because the count in that table is a class's OWN fields appended after
 * its parent's, re-declaring compounded with depth:
 *
 * <pre>
 *                                 before   after   model
 *   java/lang/Throwable                2       3       3
 *   java/lang/Exception                4       3       3
 *   java/io/IOException                6       3       3
 *   java/io/FileNotFoundException      8       3       3
 * </pre>
 *
 * In real-JDK mode that sum is applied as a FLOOR over the real class, whose
 * `Throwable` already declares six, so `IOException` was floored to ten against
 * a real six. `ClassStore::build_compact_layout` refuses a compact layout to any
 * PADDED class -- a padded slot has no descriptor, so its oop-map entry would be
 * a guess -- which put every exception object this VM allocates on the legacy
 * uniform 16-byte tagged cell.
 *
 * Lowering a floor fails SILENTLY: an out-of-range `set_field` is dropped, not
 * raised. So the checks below read back, through the public surface, every slot
 * the model names, at several depths of the hierarchy -- a lost write shows up
 * here as a null message or a missing suppressed exception, not as an error.
 *
 * Run in BOTH modes; the synthetic-JDK arm needs a binary built with
 * `--features synthetic-jdk`. Exits non-zero if anything mismatched.
 */
public class ThrowableSlotFloor {
    static int failures = 0;
    static Object[] keep;

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;

        section("message", ThrowableSlotFloor::messages);
        section("cause", ThrowableSlotFloor::causes);
        section("suppressed", ThrowableSlotFloor::suppressed);
        section("catch matching", ThrowableSlotFloor::catchMatching);
        section("toString", ThrowableSlotFloor::toStrings);

        System.out.println();
        System.out.println(String.format("%-34s %10s", "throwable (retained)", "bytes"));
        row("Throwable", n, () -> new Throwable("m"));
        row("Exception", n, () -> new Exception("m"));
        row("RuntimeException", n, () -> new RuntimeException("m"));
        row("IllegalStateException", n, () -> new IllegalStateException("m"));
        row("IOException", n, () -> new IOException("m"));
        row("FileNotFoundException", n, () -> new FileNotFoundException("m"));
        row("EOFException", n, () -> new EOFException("m"));
        System.out.println();

        System.out.println(failures == 0 ? "PASS ThrowableSlotFloor"
                                         : "FAIL ThrowableSlotFloor (" + failures + ")");
        System.out.println("THROWFLOOR_END");
        if (failures != 0) System.exit(1);
    }

    /** A throwing section must not hide the sections behind it. */
    static void section(String name, Runnable body) {
        try {
            body.run();
        } catch (Throwable t) {
            failures++;
            System.out.println("  ERROR " + name + ": " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        }
    }

    /** `detailMessage`, at four depths of the chain. */
    static void messages() {
        check("Throwable message", "m", new Throwable("m").getMessage());
        check("Exception message", "m", new Exception("m").getMessage());
        check("RuntimeException message", "m", new RuntimeException("m").getMessage());
        check("IOException message", "m", new IOException("m").getMessage());
        check("FileNotFoundException message", "m",
                new FileNotFoundException("m").getMessage());
        check("EOFException message", "m", new EOFException("m").getMessage());
        check("IllegalStateException message", "m",
                new IllegalStateException("m").getMessage());
        // The no-message forms must answer null, not the empty string: a slot
        // that reads back as `Int(0)` rather than `Object(None)` is the shape
        // the descriptor-coercion census exists to find.
        check("Throwable no message", "null", String.valueOf(new Throwable().getMessage()));
        check("IOException no message", "null", String.valueOf(new IOException().getMessage()));
    }

    /** `cause`, both constructor-supplied and `initCause`, and a two-deep chain. */
    static void causes() {
        Throwable root = new IllegalStateException("root");
        check("cause via ctor", "root", new IOException("wrap", root).getCause().getMessage());
        IOException viaInit = new IOException("wrap");
        viaInit.initCause(root);
        check("cause via initCause", "root", viaInit.getCause().getMessage());
        check("absent cause", "null", String.valueOf(new IOException("x").getCause()));
        // Two deep: the inner cause is reached through the outer's slot, so a
        // lost write at either level shows here and nowhere else.
        Throwable outer = new RuntimeException("outer", new Exception("middle", root));
        check("cause depth 2", "root", outer.getCause().getCause().getMessage());
        // `initCause` twice must refuse -- it reads the slot to decide.
        IllegalStateException once = new IllegalStateException("a");
        once.initCause(root);
        boolean refused = false;
        try {
            once.initCause(new Exception("b"));
        } catch (IllegalStateException expected) {
            refused = true;
        }
        check("initCause refuses twice", "true", String.valueOf(refused));
    }

    /**
     * `suppressedExceptions` -- slot 2, which a two-slot `Throwable` could not
     * hold at all: `write_throwable_field`'s own `slot < object_num_fields`
     * guard dropped every `addSuppressed` on a bare synthetic one.
     */
    static void suppressed() {
        Throwable t = new Throwable("main");
        check("suppressed starts empty", "0", String.valueOf(t.getSuppressed().length));
        t.addSuppressed(new IOException("s1"));
        t.addSuppressed(new IllegalStateException("s2"));
        check("suppressed count", "2", String.valueOf(t.getSuppressed().length));
        check("suppressed first", "s1", t.getSuppressed()[0].getMessage());
        check("suppressed second", "s2", t.getSuppressed()[1].getMessage());
        // Deep in the hierarchy, where the compounding was worst.
        FileNotFoundException deep = new FileNotFoundException("deep");
        deep.addSuppressed(new EOFException("eof"));
        check("suppressed on FNFE", "1", String.valueOf(deep.getSuppressed().length));
        check("suppressed on FNFE msg", "eof", deep.getSuppressed()[0].getMessage());
        // try-with-resources is the path that actually produces one.
        Throwable caught = null;
        try (AutoCloseable bad = () -> { throw new IllegalStateException("close"); }) {
            throw new IOException("body");
        } catch (Throwable e) {
            caught = e;
        }
        check("twr primary", "body", caught.getMessage());
        check("twr suppressed", "1", String.valueOf(caught.getSuppressed().length));
        check("twr suppressed msg", "close", caught.getSuppressed()[0].getMessage());
    }

    /**
     * The class chain itself -- a stub whose parent edge moved would miss here
     * even with every slot intact.
     */
    static void catchMatching() {
        check("FNFE is IOException", "true",
                String.valueOf(new FileNotFoundException("x") instanceof IOException));
        check("EOFException is IOException", "true",
                String.valueOf(new EOFException("x") instanceof IOException));
        check("IOException is Exception", "true",
                String.valueOf(new IOException("x") instanceof Exception));
        check("ISE is RuntimeException", "true",
                String.valueOf(new IllegalStateException("x") instanceof RuntimeException));
        String where = "none";
        try {
            throw new FileNotFoundException("x");
        } catch (IOException e) {
            where = "IOException";
        }
        check("caught as IOException", "IOException", where);
        String where2 = "none";
        try {
            throw new IllegalStateException("x");
        } catch (RuntimeException e) {
            where2 = "RuntimeException";
        }
        check("caught as RuntimeException", "RuntimeException", where2);
    }

    /** `toString` reads the message slot through a different door from `getMessage`. */
    static void toStrings() {
        check("Throwable toString", "java.lang.Throwable: m", new Throwable("m").toString());
        check("IOException toString", "java.io.IOException: m", new IOException("m").toString());
        check("no-message toString", "java.io.IOException", new IOException().toString());
    }

    static void row(String name, int n, Supplier<Throwable> make) {
        try {
            for (int i = 0; i < 2000; i++) {
                Throwable ignored = make.get();
            }
            Runtime rt = Runtime.getRuntime();
            Object[] hold = new Object[n];
            keep = hold;
            settle();
            long h0 = rt.totalMemory() - rt.freeMemory();
            for (int i = 0; i < n; i++) {
                hold[i] = make.get();
            }
            settle();
            long retained = (rt.totalMemory() - rt.freeMemory()) - h0;
            if (keep.length != n) {
                throw new IllegalStateException("unreachable");
            }
            System.out.println(
                    String.format(Locale.ROOT, "%-34s %10.1f", name, retained / (double) n));
            keep = null;
        } catch (Throwable t) {
            System.out.println(String.format("%-34s %10s", name, "ERROR"));
        }
    }

    static void settle() {
        for (int i = 0; i < 3; i++) {
            System.gc();
            try {
                Thread.sleep(40);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        }
    }

    static void check(String what, String expected, String actual) {
        if (!expected.equals(actual)) {
            System.out.println("  MISMATCH " + what + ": expected <" + expected
                    + "> got <" + actual + ">");
            failures++;
        }
    }
}
