import java.io.EOFException;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.PrintStream;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.io.UnsupportedEncodingException;
import java.io.ByteArrayOutputStream;
import java.io.ByteArrayInputStream;
import java.io.ObjectOutputStream;
import java.io.ObjectInputStream;
import java.lang.reflect.InvocationTargetException;
import java.util.ConcurrentModificationException;
import java.util.NoSuchElementException;

/** L8 tail — `java.lang.Throwable` and the exception hierarchy.
 *
 *  The largest single group left in the unowned `--jdk-only` surface: 117
 *  bridge-with-code rows across 42 classes, of which `Throwable` itself is 16
 *  and the other 41 are two to five each. They are one batch rather than 42
 *  because they are one REGISTRATION SET — the subclasses register the same
 *  four constructor shapes and inherit everything else from `Throwable`, so a
 *  defect in the shared code is a defect in all of them, and a probe that asks
 *  `Throwable` forty-two times is the cheapest way to find out which.
 *
 *  WHAT IS AND IS NOT COMPARABLE HERE. A stack trace's CONTENT is not a
 *  cross-VM invariant: frame lists, line numbers and the internal frames above
 *  `main` all differ legitimately between HotSpot and CratonVM, and a probe
 *  that printed them would be 100% noise. So every stack-trace question here is
 *  a SHAPE question the specification actually fixes:
 *
 *    * the top frame of a trace captured in a named method is that method, in
 *      this class, in this file — `getClassName`, `getMethodName`,
 *      `getFileName` are asked; `getLineNumber` is NOT;
 *    * `printStackTrace` is compared on its NON-INDENTED lines only, which is
 *      the header, the `Caused by:` chain and the `Suppressed:` markers — the
 *      `\tat ` frames and the `\t... N more` elisions are dropped;
 *    * a trace's LENGTH is asked as `> 0`, never as a number.
 *
 *  Everything else — messages, cause chains, suppression, `toString` format,
 *  the refusals `initCause` and `addSuppressed` owe, `StackTraceElement`'s
 *  value semantics, serialization round trips — is exact-value comparable and
 *  is compared exactly.
 *
 *  DETERMINISM: no timing, no identity hash codes, no default locale beyond
 *  what `getLocalizedMessage` is specified to do (which is `getMessage`).
 */
public class ThrowableFamilySweep {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static final char[] HEX = "0123456789abcdef".toCharArray();

    static String esc(String s) {
        if (s == null) {
            return "null";
        }
        char[] out = new char[s.length() * 6];
        int n = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) {
                out[n++] = '\\';
                out[n++] = 'u';
                out[n++] = HEX[(c >> 12) & 0xf];
                out[n++] = HEX[(c >> 8) & 0xf];
                out[n++] = HEX[(c >> 4) & 0xf];
                out[n++] = HEX[c & 0xf];
            } else {
                out[n++] = c;
            }
        }
        return new String(out, 0, n);
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName();
        }
        System.out.print(esc(tag));
        System.out.print(" |");
        System.out.print(esc(v));
        System.out.println("|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    // ---------------------------------------------------------------- helpers

    /** The stable half of a `printStackTrace`: every line that is not a frame.
     *
     *  `Throwable.printStackTrace` writes the header, then one `\tat F` per
     *  frame, then `\tSuppressed: X` / `Caused by: X` recursively with `\t... N
     *  more` elisions. The frames and the elision counts are VM-specific; the
     *  headers and the chain markers are not, and they are the thing that says
     *  whether the cause and suppression walk is right. */
    static String heads(Throwable t) {
        StringWriter sw = new StringWriter();
        PrintWriter pw = new PrintWriter(sw);
        t.printStackTrace(pw);
        pw.flush();
        String all = sw.toString();
        String out = "";
        int i = 0;
        while (i < all.length()) {
            int e = all.indexOf('\n', i);
            if (e < 0) {
                e = all.length();
            }
            String line = all.substring(i, e);
            if (line.endsWith("\r")) {
                line = line.substring(0, line.length() - 1);
            }
            String trimmed = line;
            int k = 0;
            while (k < trimmed.length() && (trimmed.charAt(k) == '\t' || trimmed.charAt(k) == ' ')) {
                k++;
            }
            trimmed = trimmed.substring(k);
            if (!trimmed.startsWith("at ") && !trimmed.startsWith("... ") && !trimmed.isEmpty()) {
                out = out + trimmed + " / ";
            }
            i = e + 1;
        }
        return out;
    }

    /** `printStackTrace(PrintStream)` through the same filter, so the two
     *  overloads are asked the SAME question and a divergence between them is
     *  a row rather than a shrug. */
    static String headsStream(Throwable t) {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        PrintStream ps;
        try {
            ps = new PrintStream(bos, true, "UTF-8");
        } catch (UnsupportedEncodingException e) {
            return "THREW " + e.getClass().getName();
        }
        t.printStackTrace(ps);
        ps.flush();
        String all;
        try {
            all = bos.toString("UTF-8");
        } catch (UnsupportedEncodingException e) {
            return "THREW " + e.getClass().getName();
        }
        String out = "";
        int i = 0;
        while (i < all.length()) {
            int e = all.indexOf('\n', i);
            if (e < 0) {
                e = all.length();
            }
            String line = all.substring(i, e);
            if (line.endsWith("\r")) {
                line = line.substring(0, line.length() - 1);
            }
            int k = 0;
            while (k < line.length() && (line.charAt(k) == '\t' || line.charAt(k) == ' ')) {
                k++;
            }
            line = line.substring(k);
            if (!line.startsWith("at ") && !line.startsWith("... ") && !line.isEmpty()) {
                out = out + line + " / ";
            }
            i = e + 1;
        }
        return out;
    }

    /** The name of a throwable's class, or `null`. Used instead of the object
     *  itself everywhere a cause is read, because a `Throwable`'s `toString`
     *  already carries the message and the identity never should. */
    static String cn(Throwable t) {
        return t == null ? "null" : t.getClass().getName();
    }

    // ------------------------------------------------- 1. the shared core, x42

    /** Every question `Throwable` answers, asked of one instance. Called once
     *  per class in the family, so a defect in the inherited implementation is
     *  42 rows and a defect in one subclass's constructor is 4. */
    static void core(String tag, Throwable t) {
        p(tag + " class", () -> t.getClass().getName());
        p(tag + " message", () -> t.getMessage());
        p(tag + " localized", () -> t.getLocalizedMessage());
        p(tag + " message==localized", () -> {
            String a = t.getMessage();
            String b = t.getLocalizedMessage();
            return a == null ? b == null : a.equals(b);
        });
        p(tag + " toString", () -> t.toString());
        p(tag + " cause class", () -> cn(t.getCause()));
        p(tag + " cause is not self", () -> t.getCause() != t);
        p(tag + " suppressed count", () -> t.getSuppressed().length);
        // NOT `getSuppressed() != getSuppressed()`: the JDK returns the shared
        // constant `EMPTY_THROWABLE_ARRAY` when nothing is suppressed, so the
        // identity answer is `false` there for a reason the specification does
        // not fix, and a probe that asked it would be 280 rows of noise. The
        // copy rule only BINDS on a non-empty list, where
        // `suppressed array is a copy` below asks it exactly.
        p(tag + " cause depth", () -> {
            int n = 0;
            Throwable c = t.getCause();
            while (c != null && n < 8) {
                n++;
                c = c.getCause();
            }
            return n;
        });
        p(tag + " trace nonempty", () -> t.getStackTrace().length > 0);
        p(tag + " trace is fresh array", () -> t.getStackTrace() != t.getStackTrace());
        p(tag + " instanceof Throwable", () -> t instanceof Throwable);
        p(tag + " heads", () -> heads(t));
    }

    /** The four constructor shapes every one of these classes declares. The
     *  `(Throwable)` shape is the interesting one: the JDK specifies its
     *  message as `cause.toString()`, not `null` and not the cause's own
     *  message, and a VM that gets that wrong is wrong for all 42. */
    interface Ctors {
        Throwable none();

        Throwable msg(String m);

        Throwable both(String m, Throwable c);

        Throwable cause(Throwable c);
    }

    static void family(String name, Ctors c) {
        Throwable seed = new IllegalStateException("seed");
        core("[" + name + "] ()", c.none());
        core("[" + name + "] (msg)", c.msg("m"));
        core("[" + name + "] (null-msg)", c.msg(null));
        core("[" + name + "] (msg,cause)", c.both("m", seed));
        core("[" + name + "] (msg,null-cause)", c.both("m", null));
        core("[" + name + "] (cause)", c.cause(seed));
        core("[" + name + "] (null-cause)", c.cause(null));
        // The documented derivation, asked directly rather than through the
        // message row above, so a failure names itself.
        p("[" + name + "] (cause) message is cause.toString", () -> {
            Throwable t = c.cause(seed);
            return seed.toString().equals(t.getMessage());
        });
        p("[" + name + "] (null-cause) message is null", () -> c.cause(null).getMessage());
        // Whether the constructor left `cause` at the JDK's `cause == this`
        // sentinel decides whether a LATER `initCause` is legal. Several of
        // these classes chain to `super((Throwable) null)` deliberately, and
        // HotSpot then refuses the first `initCause` on them, so this is a
        // per-CONSTRUCTOR fact and not an inherited one.
        p("[" + name + "] () initCause afterwards", () -> {
            Throwable t = c.none();
            t.initCause(new Error("later"));
            return cn(t.getCause());
        });
        p("[" + name + "] (msg) initCause afterwards", () -> {
            Throwable t = c.msg("m");
            t.initCause(new Error("later"));
            return cn(t.getCause());
        });
        p("[" + name + "] (msg,null-cause) initCause afterwards", () -> {
            Throwable t = c.both("m", null);
            t.initCause(new Error("later"));
            return cn(t.getCause());
        });
    }

    // ------------------------------------------------------- the family itself

    static void families() {
        family("Throwable", new Ctors() {
            public Throwable none() {
                return new Throwable();
            }

            public Throwable msg(String m) {
                return new Throwable(m);
            }

            public Throwable both(String m, Throwable x) {
                return new Throwable(m, x);
            }

            public Throwable cause(Throwable x) {
                return new Throwable(x);
            }
        });
        family("Exception", new Ctors() {
            public Throwable none() {
                return new Exception();
            }

            public Throwable msg(String m) {
                return new Exception(m);
            }

            public Throwable both(String m, Throwable x) {
                return new Exception(m, x);
            }

            public Throwable cause(Throwable x) {
                return new Exception(x);
            }
        });
        family("RuntimeException", new Ctors() {
            public Throwable none() {
                return new RuntimeException();
            }

            public Throwable msg(String m) {
                return new RuntimeException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new RuntimeException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new RuntimeException(x);
            }
        });
        family("Error", new Ctors() {
            public Throwable none() {
                return new Error();
            }

            public Throwable msg(String m) {
                return new Error(m);
            }

            public Throwable both(String m, Throwable x) {
                return new Error(m, x);
            }

            public Throwable cause(Throwable x) {
                return new Error(x);
            }
        });
        family("LinkageError", new Ctors() {
            public Throwable none() {
                return new LinkageError();
            }

            public Throwable msg(String m) {
                return new LinkageError(m);
            }

            public Throwable both(String m, Throwable x) {
                return new LinkageError(m, x);
            }

            public Throwable cause(Throwable x) {
                return new LinkageError(m(x), x);
            }
        });
        family("IllegalArgumentException", new Ctors() {
            public Throwable none() {
                return new IllegalArgumentException();
            }

            public Throwable msg(String m) {
                return new IllegalArgumentException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new IllegalArgumentException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new IllegalArgumentException(x);
            }
        });
        family("IllegalStateException", new Ctors() {
            public Throwable none() {
                return new IllegalStateException();
            }

            public Throwable msg(String m) {
                return new IllegalStateException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new IllegalStateException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new IllegalStateException(x);
            }
        });
        family("UnsupportedOperationException", new Ctors() {
            public Throwable none() {
                return new UnsupportedOperationException();
            }

            public Throwable msg(String m) {
                return new UnsupportedOperationException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new UnsupportedOperationException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new UnsupportedOperationException(x);
            }
        });
        family("SecurityException", new Ctors() {
            public Throwable none() {
                return new SecurityException();
            }

            public Throwable msg(String m) {
                return new SecurityException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new SecurityException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new SecurityException(x);
            }
        });
        family("IOException", new Ctors() {
            public Throwable none() {
                return new IOException();
            }

            public Throwable msg(String m) {
                return new IOException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new IOException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new IOException(x);
            }
        });
        family("ReflectiveOperationException", new Ctors() {
            public Throwable none() {
                return new ReflectiveOperationException();
            }

            public Throwable msg(String m) {
                return new ReflectiveOperationException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new ReflectiveOperationException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new ReflectiveOperationException(x);
            }
        });
        family("ClassNotFoundException", new Ctors() {
            public Throwable none() {
                return new ClassNotFoundException();
            }

            public Throwable msg(String m) {
                return new ClassNotFoundException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new ClassNotFoundException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new ClassNotFoundException(m(x), x);
            }
        });
        family("NoSuchMethodException", new Ctors() {
            public Throwable none() {
                return new NoSuchMethodException();
            }

            public Throwable msg(String m) {
                return new NoSuchMethodException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new NoSuchMethodException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new NoSuchMethodException(m(x)), x);
            }
        });
        family("NoSuchFieldException", new Ctors() {
            public Throwable none() {
                return new NoSuchFieldException();
            }

            public Throwable msg(String m) {
                return new NoSuchFieldException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new NoSuchFieldException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new NoSuchFieldException(m(x)), x);
            }
        });
        family("CloneNotSupportedException", new Ctors() {
            public Throwable none() {
                return new CloneNotSupportedException();
            }

            public Throwable msg(String m) {
                return new CloneNotSupportedException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new CloneNotSupportedException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new CloneNotSupportedException(m(x)), x);
            }
        });
        family("InterruptedException", new Ctors() {
            public Throwable none() {
                return new InterruptedException();
            }

            public Throwable msg(String m) {
                return new InterruptedException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new InterruptedException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new InterruptedException(m(x)), x);
            }
        });
        family("ClassCastException", new Ctors() {
            public Throwable none() {
                return new ClassCastException();
            }

            public Throwable msg(String m) {
                return new ClassCastException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new ClassCastException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new ClassCastException(m(x)), x);
            }
        });
        family("NullPointerException", new Ctors() {
            public Throwable none() {
                return new NullPointerException();
            }

            public Throwable msg(String m) {
                return new NullPointerException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new NullPointerException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new NullPointerException(m(x)), x);
            }
        });
        family("ArithmeticException", new Ctors() {
            public Throwable none() {
                return new ArithmeticException();
            }

            public Throwable msg(String m) {
                return new ArithmeticException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new ArithmeticException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new ArithmeticException(m(x)), x);
            }
        });
        family("NumberFormatException", new Ctors() {
            public Throwable none() {
                return new NumberFormatException();
            }

            public Throwable msg(String m) {
                return new NumberFormatException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new NumberFormatException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new NumberFormatException(m(x)), x);
            }
        });
        family("NegativeArraySizeException", new Ctors() {
            public Throwable none() {
                return new NegativeArraySizeException();
            }

            public Throwable msg(String m) {
                return new NegativeArraySizeException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new NegativeArraySizeException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new NegativeArraySizeException(m(x)), x);
            }
        });
        family("IndexOutOfBoundsException", new Ctors() {
            public Throwable none() {
                return new IndexOutOfBoundsException();
            }

            public Throwable msg(String m) {
                return new IndexOutOfBoundsException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new IndexOutOfBoundsException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new IndexOutOfBoundsException(m(x)), x);
            }
        });
        family("ArrayIndexOutOfBoundsException", new Ctors() {
            public Throwable none() {
                return new ArrayIndexOutOfBoundsException();
            }

            public Throwable msg(String m) {
                return new ArrayIndexOutOfBoundsException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new ArrayIndexOutOfBoundsException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new ArrayIndexOutOfBoundsException(m(x)), x);
            }
        });
        family("StringIndexOutOfBoundsException", new Ctors() {
            public Throwable none() {
                return new StringIndexOutOfBoundsException();
            }

            public Throwable msg(String m) {
                return new StringIndexOutOfBoundsException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new StringIndexOutOfBoundsException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new StringIndexOutOfBoundsException(m(x)), x);
            }
        });
        family("ConcurrentModificationException", new Ctors() {
            public Throwable none() {
                return new ConcurrentModificationException();
            }

            public Throwable msg(String m) {
                return new ConcurrentModificationException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new ConcurrentModificationException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new ConcurrentModificationException(x);
            }
        });
        family("NoSuchElementException", new Ctors() {
            public Throwable none() {
                return new NoSuchElementException();
            }

            public Throwable msg(String m) {
                return new NoSuchElementException(m);
            }

            public Throwable both(String m, Throwable x) {
                return new NoSuchElementException(m, x);
            }

            public Throwable cause(Throwable x) {
                return new NoSuchElementException(x);
            }
        });
        family("EOFException", new Ctors() {
            public Throwable none() {
                return new EOFException();
            }

            public Throwable msg(String m) {
                return new EOFException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new EOFException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new EOFException(m(x)), x);
            }
        });
        family("FileNotFoundException", new Ctors() {
            public Throwable none() {
                return new FileNotFoundException();
            }

            public Throwable msg(String m) {
                return new FileNotFoundException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new FileNotFoundException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new FileNotFoundException(m(x)), x);
            }
        });
        family("UnsupportedEncodingException", new Ctors() {
            public Throwable none() {
                return new UnsupportedEncodingException();
            }

            public Throwable msg(String m) {
                return new UnsupportedEncodingException(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new UnsupportedEncodingException(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new UnsupportedEncodingException(m(x)), x);
            }
        });
        family("StackOverflowError", new Ctors() {
            public Throwable none() {
                return new StackOverflowError();
            }

            public Throwable msg(String m) {
                return new StackOverflowError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new StackOverflowError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new StackOverflowError(m(x)), x);
            }
        });
        family("OutOfMemoryError", new Ctors() {
            public Throwable none() {
                return new OutOfMemoryError();
            }

            public Throwable msg(String m) {
                return new OutOfMemoryError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new OutOfMemoryError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new OutOfMemoryError(m(x)), x);
            }
        });
        family("NoSuchMethodError", new Ctors() {
            public Throwable none() {
                return new NoSuchMethodError();
            }

            public Throwable msg(String m) {
                return new NoSuchMethodError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new NoSuchMethodError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new NoSuchMethodError(m(x)), x);
            }
        });
        family("NoSuchFieldError", new Ctors() {
            public Throwable none() {
                return new NoSuchFieldError();
            }

            public Throwable msg(String m) {
                return new NoSuchFieldError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new NoSuchFieldError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new NoSuchFieldError(m(x)), x);
            }
        });
        family("AbstractMethodError", new Ctors() {
            public Throwable none() {
                return new AbstractMethodError();
            }

            public Throwable msg(String m) {
                return new AbstractMethodError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new AbstractMethodError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new AbstractMethodError(m(x)), x);
            }
        });
        family("IllegalAccessError", new Ctors() {
            public Throwable none() {
                return new IllegalAccessError();
            }

            public Throwable msg(String m) {
                return new IllegalAccessError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new IllegalAccessError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new IllegalAccessError(m(x)), x);
            }
        });
        family("IncompatibleClassChangeError", new Ctors() {
            public Throwable none() {
                return new IncompatibleClassChangeError();
            }

            public Throwable msg(String m) {
                return new IncompatibleClassChangeError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new IncompatibleClassChangeError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new IncompatibleClassChangeError(m(x)), x);
            }
        });
        family("VerifyError", new Ctors() {
            public Throwable none() {
                return new VerifyError();
            }

            public Throwable msg(String m) {
                return new VerifyError(m);
            }

            public Throwable both(String m, Throwable x) {
                return init(new VerifyError(m), x);
            }

            public Throwable cause(Throwable x) {
                return init(new VerifyError(m(x)), x);
            }
        });
        family("AssertionError", new Ctors() {
            public Throwable none() {
                return new AssertionError();
            }

            public Throwable msg(String m) {
                return new AssertionError((Object) m);
            }

            public Throwable both(String m, Throwable x) {
                return new AssertionError(m, x);
            }

            public Throwable cause(Throwable x) {
                return new AssertionError((Object) x);
            }
        });
        family("ExceptionInInitializerError", new Ctors() {
            public Throwable none() {
                return new ExceptionInInitializerError();
            }

            public Throwable msg(String m) {
                return new ExceptionInInitializerError(m);
            }

            public Throwable both(String m, Throwable x) {
                return new ExceptionInInitializerError(x);
            }

            public Throwable cause(Throwable x) {
                return x == null
                        ? new ExceptionInInitializerError((Exception) null)
                        : new ExceptionInInitializerError(new Exception(x));
            }
        });
        family("InvocationTargetException", new Ctors() {
            public Throwable none() {
                return new InvocationTargetException(null);
            }

            public Throwable msg(String m) {
                return new InvocationTargetException(null, m);
            }

            public Throwable both(String m, Throwable x) {
                return new InvocationTargetException(x, m);
            }

            public Throwable cause(Throwable x) {
                return new InvocationTargetException(x);
            }
        });
    }

    /** `initCause` where the class declares no `(String, Throwable)` shape.
     *  Deliberately a shared helper: `initCause` is one of the sixteen rows
     *  under test, so routing half the family through it doubles its coverage
     *  for free. */
    static Throwable init(Throwable t, Throwable cause) {
        if (cause != null) {
            t.initCause(cause);
        }
        return t;
    }

    static String m(Throwable x) {
        return x == null ? null : x.toString();
    }

    // -------------------------------------------------- 2. the cause protocol

    static void causeProtocol() {
        p("initCause once", () -> {
            Throwable t = new Throwable("t");
            t.initCause(new IllegalStateException("c"));
            return cn(t.getCause());
        });
        p("initCause returns this", () -> {
            Throwable t = new Throwable("t");
            return t.initCause(new IllegalStateException("c")) == t;
        });
        p("initCause null once", () -> {
            Throwable t = new Throwable("t");
            t.initCause(null);
            return cn(t.getCause());
        });
        p("initCause twice", () -> {
            Throwable t = new Throwable("t");
            t.initCause(new IllegalStateException("a"));
            t.initCause(new IllegalStateException("b"));
            return "no throw";
        });
        p("initCause twice, second is null", () -> {
            Throwable t = new Throwable("t");
            t.initCause(null);
            t.initCause(null);
            return "no throw";
        });
        p("initCause after (msg,cause) ctor", () -> {
            Throwable t = new Throwable("t", new IllegalStateException("c"));
            t.initCause(new IllegalStateException("d"));
            return "no throw";
        });
        p("initCause after (msg,null-cause) ctor", () -> {
            Throwable t = new Throwable("t", null);
            t.initCause(new IllegalStateException("d"));
            return "no throw";
        });
        p("initCause self", () -> {
            Throwable t = new Throwable("t");
            t.initCause(t);
            return "no throw";
        });
        p("getCause of an uncaused throwable", () -> cn(new Throwable("t").getCause()));
        p("getCause of a self-referential chain", () -> {
            Throwable a = new Throwable("a");
            Throwable b = new Throwable("b", a);
            return cn(b.getCause().getCause());
        });
        p("three-deep chain heads", () -> {
            Throwable a = new IllegalStateException("innermost");
            Throwable b = new IOException("middle", a);
            Throwable c = new RuntimeException("outer", b);
            return heads(c);
        });
        p("three-deep chain heads via PrintStream", () -> {
            Throwable a = new IllegalStateException("innermost");
            Throwable b = new IOException("middle", a);
            Throwable c = new RuntimeException("outer", b);
            return headsStream(c);
        });
        p("printStackTrace overloads agree", () -> {
            Throwable a = new IllegalStateException("innermost");
            Throwable c = new RuntimeException("outer", a);
            return heads(c).equals(headsStream(c));
        });
        // A cycle in the cause chain: the JDK's printStackTrace carries a
        // `dejaVu` set precisely so this terminates. A VM without it hangs or
        // overflows, so the row is worth its weight.
        p("cyclic cause chain terminates", () -> {
            Throwable a = new Throwable("a");
            Throwable b = new Throwable("b", a);
            a.initCause(b);
            String h = heads(b);
            return h.length() > 0;
        });
    }

    // --------------------------------------------------- 3. suppression

    static class Closer implements AutoCloseable {
        final String name;

        Closer(String name) {
            this.name = name;
        }

        public void close() {
            throw new IllegalStateException("close " + name);
        }
    }

    static void suppression() {
        p("addSuppressed one", () -> {
            Throwable t = new Throwable("t");
            t.addSuppressed(new IllegalStateException("s"));
            return t.getSuppressed().length + " " + cn(t.getSuppressed()[0]);
        });
        p("addSuppressed keeps order", () -> {
            Throwable t = new Throwable("t");
            t.addSuppressed(new IllegalStateException("1"));
            t.addSuppressed(new IOException("2"));
            t.addSuppressed(new Error("3"));
            String out = "";
            for (Throwable s : t.getSuppressed()) {
                out = out + s.getMessage() + ",";
            }
            return out;
        });
        p("addSuppressed null", () -> {
            Throwable t = new Throwable("t");
            t.addSuppressed(null);
            return "no throw";
        });
        p("addSuppressed self", () -> {
            Throwable t = new Throwable("t");
            t.addSuppressed(t);
            return "no throw";
        });
        p("addSuppressed duplicate is kept", () -> {
            Throwable t = new Throwable("t");
            Throwable s = new IllegalStateException("s");
            t.addSuppressed(s);
            t.addSuppressed(s);
            return t.getSuppressed().length;
        });
        p("getSuppressed on a fresh throwable", () -> new Throwable("t").getSuppressed().length);
        p("suppressed array is a copy", () -> {
            Throwable t = new Throwable("t");
            t.addSuppressed(new IllegalStateException("s"));
            Throwable[] a = t.getSuppressed();
            a[0] = null;
            return cn(t.getSuppressed()[0]);
        });
        p("try-with-resources suppression", () -> {
            try {
                try (Closer c = new Closer("A")) {
                    throw new IOException("body");
                }
            } catch (Throwable e) {
                return e.getMessage() + " sup=" + e.getSuppressed().length + " "
                        + (e.getSuppressed().length > 0 ? e.getSuppressed()[0].getMessage() : "-");
            }
        });
        p("try-with-resources two resources", () -> {
            try {
                try (Closer a = new Closer("A"); Closer b = new Closer("B")) {
                    throw new IOException("body");
                }
            } catch (Throwable e) {
                String out = e.getMessage() + " sup=" + e.getSuppressed().length + " ";
                for (Throwable s : e.getSuppressed()) {
                    out = out + s.getMessage() + ",";
                }
                return out;
            }
        });
        p("try-with-resources, close throws alone", () -> {
            try {
                try (Closer a = new Closer("A")) {
                    // no body failure: the close exception is PRIMARY
                }
                return "no throw";
            } catch (Throwable e) {
                return e.getMessage() + " sup=" + e.getSuppressed().length;
            }
        });
        p("suppressed heads", () -> {
            Throwable t = new RuntimeException("primary");
            t.addSuppressed(new IllegalStateException("sup1"));
            t.addSuppressed(new IOException("sup2", new Error("sup2cause")));
            return heads(t);
        });
        p("suppressed and cause together", () -> {
            Throwable t = new RuntimeException("primary", new IOException("cause"));
            t.addSuppressed(new IllegalStateException("sup"));
            return heads(t);
        });
    }

    // ------------------------------------------------- 4. the stack trace

    static void traces() {
        p("top frame class", () -> new Throwable().getStackTrace()[0].getClassName());
        p("top frame method", () -> new Throwable().getStackTrace()[0].getMethodName());
        p("top frame file", () -> new Throwable().getStackTrace()[0].getFileName());
        p("top frame is not native", () -> new Throwable().getStackTrace()[0].isNativeMethod());
        p("top frame line is positive", () -> new Throwable().getStackTrace()[0].getLineNumber() > 0);
        p("trace has main below", () -> {
            StackTraceElement[] st = new Throwable().getStackTrace();
            for (StackTraceElement e : st) {
                if (e.getMethodName().equals("main")
                        && e.getClassName().equals("ThrowableFamilySweep")) {
                    return true;
                }
            }
            return false;
        });
        p("fillInStackTrace returns this", () -> {
            Throwable t = new Throwable("t");
            return t.fillInStackTrace() == t;
        });
        p("fillInStackTrace refills", () -> {
            Throwable t = new Throwable("t");
            t.setStackTrace(new StackTraceElement[0]);
            t.fillInStackTrace();
            return t.getStackTrace().length > 0;
        });
        p("setStackTrace empty", () -> {
            Throwable t = new Throwable("t");
            t.setStackTrace(new StackTraceElement[0]);
            return t.getStackTrace().length;
        });
        p("setStackTrace explicit", () -> {
            Throwable t = new Throwable("t");
            t.setStackTrace(new StackTraceElement[] {
                new StackTraceElement("C", "m", "C.java", 7),
            });
            StackTraceElement[] st = t.getStackTrace();
            return st.length + " " + st[0];
        });
        p("setStackTrace copies its argument", () -> {
            Throwable t = new Throwable("t");
            StackTraceElement[] a = new StackTraceElement[] {
                new StackTraceElement("C", "m", "C.java", 7),
            };
            t.setStackTrace(a);
            a[0] = new StackTraceElement("D", "n", "D.java", 9);
            return t.getStackTrace()[0].toString();
        });
        p("setStackTrace null", () -> {
            new Throwable("t").setStackTrace(null);
            return "no throw";
        });
        p("setStackTrace with a null element", () -> {
            new Throwable("t").setStackTrace(new StackTraceElement[] {null});
            return "no throw";
        });
        // The class alone cannot say WHICH check fired, and the two refusals
        // are different rules with different messages: `clone()` on a null
        // array against the explicit `stackTrace[i]` null scan.
        p("setStackTrace null message", () -> {
            try {
                new Throwable("t").setStackTrace(null);
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("setStackTrace null element message", () -> {
            try {
                new Throwable("t").setStackTrace(new StackTraceElement[] {
                    new StackTraceElement("C", "m", "C.java", 1), null,
                });
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("setStackTrace rejects before it stores", () -> {
            Throwable t = new Throwable("t");
            t.setStackTrace(new StackTraceElement[] {
                new StackTraceElement("KEEP", "m", "C.java", 1),
            });
            try {
                t.setStackTrace(new StackTraceElement[] {null});
            } catch (Throwable ignored) {
                // expected
            }
            return t.getStackTrace().length + " " + t.getStackTrace()[0].getClassName();
        });
        p("getStackTrace after setStackTrace empty then heads", () -> {
            Throwable t = new Throwable("t");
            t.setStackTrace(new StackTraceElement[0]);
            return heads(t);
        });
        p("empty trace printStackTrace has a header", () -> {
            Throwable t = new IllegalStateException("boom");
            t.setStackTrace(new StackTraceElement[0]);
            return heads(t).startsWith("java.lang.IllegalStateException: boom");
        });
    }

    // ------------------------------------------- 5. StackTraceElement itself

    static void steElements() {
        StackTraceElement a = new StackTraceElement("p.C", "m", "C.java", 7);
        StackTraceElement b = new StackTraceElement("p.C", "m", "C.java", 7);
        StackTraceElement nat = new StackTraceElement("p.C", "m", null, -2);
        StackTraceElement unk = new StackTraceElement("p.C", "m", null, -1);
        p("ste toString", () -> a.toString());
        p("ste className", () -> a.getClassName());
        p("ste methodName", () -> a.getMethodName());
        p("ste fileName", () -> a.getFileName());
        p("ste lineNumber", () -> a.getLineNumber());
        p("ste isNativeMethod", () -> a.isNativeMethod());
        p("ste equals", () -> a.equals(b));
        p("ste hashCode agrees", () -> a.hashCode() == b.hashCode());
        p("ste equals null", () -> a.equals(null));
        p("ste equals other", () -> a.equals("x"));
        p("ste native toString", () -> nat.toString());
        p("ste native isNativeMethod", () -> nat.isNativeMethod());
        p("ste unknown toString", () -> unk.toString());
        p("ste unknown isNativeMethod", () -> unk.isNativeMethod());
        p("ste zero line toString", () -> new StackTraceElement("p.C", "m", "C.java", 0).toString());
        p("ste getModuleName", () -> a.getModuleName());
        p("ste getClassLoaderName", () -> a.getClassLoaderName());
        p("ste null class", () -> new StackTraceElement(null, "m", "f", 1).toString());
        p("ste null method", () -> new StackTraceElement("p.C", null, "f", 1).toString());
        p("ste from a real trace has a class name", () -> {
            StackTraceElement e = new Throwable().getStackTrace()[0];
            return e.getClassName() != null && !e.getClassName().isEmpty();
        });
    }

    // ---------------------------------------- 6. the toString/message format

    static void formats() {
        p("toString no message", () -> new IllegalStateException().toString());
        p("toString empty message", () -> new IllegalStateException("").toString());
        p("toString with message", () -> new IllegalStateException("boom").toString());
        p("toString message with colon", () -> new IllegalStateException("a: b").toString());
        p("toString message with newline", () -> new IllegalStateException("a\nb").toString());
        p("toString of a cause-only", () -> new RuntimeException(new IOException("io")).toString());
        p("nested cause-only message", () ->
                new RuntimeException(new RuntimeException(new IOException("io"))).getMessage());
        p("toString of Throwable base", () -> new Throwable().toString());
        p("toString of an anonymous subclass", () -> new Throwable("x") {}.toString());
        p("getMessage of an anonymous subclass", () -> new Throwable("x") {}.getMessage());
        p("AssertionError(int)", () -> new AssertionError(1).toString());
        p("AssertionError(long)", () -> new AssertionError(1L).toString());
        p("AssertionError(char)", () -> new AssertionError('c').toString());
        p("AssertionError(boolean)", () -> new AssertionError(true).toString());
        p("AssertionError(float)", () -> new AssertionError(1.5f).toString());
        p("AssertionError(double)", () -> new AssertionError(1.5d).toString());
        p("AssertionError(null Object)", () -> new AssertionError((Object) null).toString());
        p("AssertionError(Throwable) cause", () -> cn(new AssertionError(new IOException("io")).getCause()));
        p("AssertionError(String,Throwable) cause",
            () -> cn(new AssertionError("m", new IOException("io")).getCause()));
        p("ExceptionInInitializerError getException", () -> {
            ExceptionInInitializerError e = new ExceptionInInitializerError(new IOException("io"));
            return cn(e.getException());
        });
        p("ExceptionInInitializerError getCause", () -> {
            ExceptionInInitializerError e = new ExceptionInInitializerError(new IOException("io"));
            return cn(e.getCause());
        });
        p("ExceptionInInitializerError exception==cause", () -> {
            ExceptionInInitializerError e = new ExceptionInInitializerError(new IOException("io"));
            return e.getException() == e.getCause();
        });
        p("ExceptionInInitializerError(String) getException", () -> {
            ExceptionInInitializerError e = new ExceptionInInitializerError("m");
            return cn(e.getException());
        });
        p("InvocationTargetException getTargetException", () -> {
            InvocationTargetException e = new InvocationTargetException(new IOException("io"));
            return cn(e.getTargetException());
        });
        p("InvocationTargetException getCause", () -> {
            InvocationTargetException e = new InvocationTargetException(new IOException("io"));
            return cn(e.getCause());
        });
        p("InvocationTargetException target==cause", () -> {
            InvocationTargetException e = new InvocationTargetException(new IOException("io"));
            return e.getTargetException() == e.getCause();
        });
        p("InvocationTargetException(null) cause",
            () -> cn(new InvocationTargetException(null).getCause()));
        p("InvocationTargetException heads", () -> {
            InvocationTargetException e = new InvocationTargetException(new IOException("io"), "wrapped");
            return heads(e);
        });
        p("ClassNotFoundException getException", () -> {
            ClassNotFoundException e = new ClassNotFoundException("c", new IOException("io"));
            return cn(e.getException());
        });
        p("ClassNotFoundException getCause", () -> {
            ClassNotFoundException e = new ClassNotFoundException("c", new IOException("io"));
            return cn(e.getCause());
        });
        p("ClassNotFoundException(String) getException",
            () -> cn(new ClassNotFoundException("c").getException()));
        p("ClassNotFoundException(String) getCause",
            () -> cn(new ClassNotFoundException("c").getCause()));
    }

    // ------------------------------------------- 7. exceptions the VM throws

    /** The messages of exceptions the VM RAISES, as opposed to ones the probe
     *  constructs. This is the half a constructor-only sweep cannot see: the
     *  message text is produced by the runtime, and it is where two VMs most
     *  easily disagree. */
    static void thrown() {
        p("1/0", () -> {
            int z = 0;
            try {
                return 1 / z;
            } catch (ArithmeticException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("1L/0L", () -> {
            long z = 0;
            try {
                return 1L / z;
            } catch (ArithmeticException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("1%0", () -> {
            int z = 0;
            try {
                return 1 % z;
            } catch (ArithmeticException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("array index", () -> {
            int[] a = new int[2];
            try {
                return a[5];
            } catch (ArrayIndexOutOfBoundsException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("array negative index", () -> {
            int[] a = new int[2];
            int i = -1;
            try {
                return a[i];
            } catch (ArrayIndexOutOfBoundsException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("negative array size", () -> {
            int n = -1;
            try {
                return new int[n].length;
            } catch (NegativeArraySizeException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("class cast", () -> {
            Object o = "s";
            try {
                return (Integer) o;
            } catch (ClassCastException e) {
                return e.getClass().getName();
            }
        });
        p("array store", () -> {
            Object[] a = new String[1];
            try {
                a[0] = Integer.valueOf(1);
                return "no throw";
            } catch (ArrayStoreException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("charAt out of range", () -> {
            try {
                return "ab".charAt(9);
            } catch (StringIndexOutOfBoundsException e) {
                return e.getClass().getName();
            }
        });
        p("substring out of range", () -> {
            try {
                return "ab".substring(0, 9);
            } catch (StringIndexOutOfBoundsException e) {
                return e.getClass().getName();
            }
        });
        p("Integer.parseInt", () -> {
            try {
                return Integer.parseInt("zz");
            } catch (NumberFormatException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("Integer.parseInt empty", () -> {
            try {
                return Integer.parseInt("");
            } catch (NumberFormatException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("Integer.parseInt null", () -> {
            try {
                return Integer.parseInt(null);
            } catch (NumberFormatException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("Long.parseLong overflow", () -> {
            try {
                return Long.parseLong("99999999999999999999");
            } catch (NumberFormatException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("Class.forName missing", () -> {
            try {
                return Class.forName("no.such.Klass").getName();
            } catch (ClassNotFoundException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("Class.forName missing cause", () -> {
            try {
                Class.forName("no.such.Klass");
                return "no throw";
            } catch (ClassNotFoundException e) {
                return cn(e.getCause());
            }
        });
        p("Object.clone on a non-Cloneable", () -> {
            try {
                return new Object().getClass().getDeclaredMethod("clone").getName();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("NoSuchMethodException from reflection", () -> {
            try {
                return String.class.getMethod("noSuchMethodHere").getName();
            } catch (NoSuchMethodException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("NoSuchFieldException from reflection", () -> {
            try {
                return String.class.getField("noSuchFieldHere").getName();
            } catch (NoSuchFieldException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("NPE on a null field read", () -> {
            String s = null;
            try {
                return s.length();
            } catch (NullPointerException e) {
                return e.getClass().getName();
            }
        });
        p("NPE on a null array read", () -> {
            int[] a = null;
            try {
                return a[0];
            } catch (NullPointerException e) {
                return e.getClass().getName();
            }
        });
        p("NPE on a null array length", () -> {
            int[] a = null;
            try {
                return a.length;
            } catch (NullPointerException e) {
                return e.getClass().getName();
            }
        });
        p("NPE is catchable as RuntimeException", () -> {
            String s = null;
            try {
                return s.length();
            } catch (RuntimeException e) {
                return e instanceof NullPointerException;
            }
        });
        p("thrown exception has a trace", () -> {
            try {
                int z = 0;
                return 1 / z;
            } catch (ArithmeticException e) {
                return e.getStackTrace().length > 0;
            }
        });
        p("thrown exception's top frame is here", () -> {
            try {
                int z = 0;
                return 1 / z;
            } catch (ArithmeticException e) {
                StackTraceElement s = e.getStackTrace()[0];
                return s.getClassName() + "." + s.getMethodName();
            }
        });
        p("rethrow preserves the trace length", () -> {
            Throwable first;
            try {
                int z = 0;
                int unused = 1 / z;
                return "no throw";
            } catch (ArithmeticException e) {
                first = e;
            }
            int before = first.getStackTrace().length;
            try {
                throw first;
            } catch (Throwable e) {
                return e.getStackTrace().length == before;
            }
        });
        p("catch by supertype", () -> {
            try {
                throw new FileNotFoundException("f");
            } catch (IOException e) {
                return e.getClass().getName();
            }
        });
        p("multicatch", () -> {
            try {
                throw new NumberFormatException("n");
            } catch (IllegalStateException | IllegalArgumentException e) {
                return e.getClass().getName();
            }
        });
        p("finally runs after a throw", () -> {
            String[] out = new String[] {""};
            try {
                try {
                    throw new IllegalStateException("x");
                } finally {
                    out[0] = "finally";
                }
            } catch (IllegalStateException e) {
                return out[0] + "+" + e.getMessage();
            }
        });
    }

    // --------------------------------------------------- 8. serialization

    static void serial() {
        p("Throwable round trip message", () -> {
            Throwable t = new IllegalStateException("boom", new IOException("io"));
            byte[] b = ser(t);
            Throwable r = (Throwable) deser(b);
            return r.getClass().getName() + " | " + r.getMessage() + " | " + cn(r.getCause())
                    + " | " + r.getCause().getMessage();
        });
        p("Throwable round trip preserves the trace length", () -> {
            Throwable t = new IllegalStateException("boom");
            int before = t.getStackTrace().length;
            Throwable r = (Throwable) deser(ser(t));
            return r.getStackTrace().length == before;
        });
        p("Throwable round trip preserves the top frame", () -> {
            Throwable t = new IllegalStateException("boom");
            StackTraceElement a = t.getStackTrace()[0];
            StackTraceElement b = ((Throwable) deser(ser(t))).getStackTrace()[0];
            return a.getClassName().equals(b.getClassName())
                    && a.getMethodName().equals(b.getMethodName());
        });
        p("Throwable round trip preserves suppression", () -> {
            Throwable t = new IllegalStateException("boom");
            t.addSuppressed(new IOException("sup"));
            Throwable r = (Throwable) deser(ser(t));
            return r.getSuppressed().length + " "
                    + (r.getSuppressed().length > 0 ? r.getSuppressed()[0].getMessage() : "-");
        });
        p("round trip of a doubly-suppressed throwable", () -> {
            Throwable t = new IllegalStateException("boom");
            t.addSuppressed(new IOException("s1"));
            t.addSuppressed(new EOFException("s2"));
            Throwable r = (Throwable) deser(ser(t));
            String out = r.getSuppressed().length + " ";
            for (Throwable s : r.getSuppressed()) {
                out = out + s.getClass().getName() + ":" + s.getMessage() + ",";
            }
            return out;
        });
        p("round trip of suppression plus a cause", () -> {
            Throwable t = new IllegalStateException("boom", new IOException("cause"));
            t.addSuppressed(new EOFException("sup"));
            Throwable r = (Throwable) deser(ser(t));
            return cn(r.getCause()) + " / " + r.getSuppressed().length;
        });
        p("StackTraceElement round trip", () -> {
            StackTraceElement e = new StackTraceElement("p.C", "m", "C.java", 7);
            return deser(ser(e)).toString();
        });
        p("round-tripped throwable prints the same heads", () -> {
            Throwable t = new IllegalStateException("boom", new IOException("io"));
            String a = heads(t);
            String b = heads((Throwable) deser(ser(t)));
            return a.equals(b);
        });
    }

    static byte[] ser(Object o) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        ObjectOutputStream oos = new ObjectOutputStream(bos);
        oos.writeObject(o);
        oos.close();
        return bos.toByteArray();
    }

    static Object deser(byte[] b) throws Exception {
        return new ObjectInputStream(new ByteArrayInputStream(b)).readObject();
    }

    // -------------------------------------------------------------------

    public static void main(String[] args) {
        sect("families", ThrowableFamilySweep::families);
        sect("cause", ThrowableFamilySweep::causeProtocol);
        sect("suppression", ThrowableFamilySweep::suppression);
        sect("traces", ThrowableFamilySweep::traces);
        sect("ste", ThrowableFamilySweep::steElements);
        sect("formats", ThrowableFamilySweep::formats);
        sect("thrown", ThrowableFamilySweep::thrown);
        sect("serial", ThrowableFamilySweep::serial);
        System.out.println("rows " + rows);
        System.out.println("DONE ThrowableFamilySweep");
    }
}
