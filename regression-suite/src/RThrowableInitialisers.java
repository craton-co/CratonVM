// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;
import java.util.function.Supplier;
import java.util.regex.Pattern;
import java.util.regex.PatternSyntaxException;

/**
 * Regression: `Throwable`'s two field initialisers -- `cause = this` and
 * `suppressedExceptions = SUPPRESSED_SENTINEL` -- are in place on every
 * throwable that reaches user code the way HotSpot has them, whichever way it
 * was built. They are read as DECISIONS: `initCause` refuses unless
 * `cause == this`, and `addSuppressed` silently drops unless the list is
 * non-null.
 *
 * Under `--compatible` most of these constructors are NATIVE shadows of
 * `Throwable.<init>` that write the fields themselves; under `--jdk-only` the
 * real bytecode runs. The VM also builds throwables of its own (implicit
 * exceptions, regex syntax errors), and until 2026-09-24 it decided whether a
 * constructor had set `cause` by reading the slot back and taking an `Int(0)`
 * as "never written". That only works for a LEGACY 16-byte field cell: a
 * COMPACT reference slot of zeroes -- every real `Throwable` is compact --
 * reads `Object(None)`, byte-for-byte a written null, so the marker never
 * fired. The VM now seeds `cause = this` before any constructor runs
 * (`exceptions::seed_throwable_cause`), which is only right if EVERY
 * constructor then overwrites it exactly as HotSpot's does. The sweep below is
 * that claim, one line per public constructor of the shadowed family
 * (`native-builtins` `THROWABLE_FAMILY_CLASSES` whose parameters are all
 * String / Throwable / int / long / boolean; generated from JDK 25 by
 * reflection, 157 of them).
 *
 * HotSpot is the oracle: every `CK` line is diffed against it.
 */
public class RThrowableInitialisers {

    static int checks = 0;

    static String verdict(Throwable t) {
        String cause;
        try {
            t.initCause(new RuntimeException("seed"));
            cause = "accept";
        } catch (IllegalStateException e) {
            cause = "refuse";
        }
        t.addSuppressed(new RuntimeException("s"));
        checks++;
        return "initCause=" + cause + " suppressed=" + t.getSuppressed().length;
    }

    static void ctor(String sig, Supplier<Throwable> s) {
        Throwable t;
        try {
            t = s.get();
        } catch (Throwable x) {
            System.out.println("CK " + sig + " CTOR-THREW " + x.getClass().getName());
            checks++;
            return;
        }
        System.out.println("CK " + sig + " " + verdict(t));
    }

    static void caught(String what, Throwable t) {
        System.out.println("CK " + what + " " + t.getClass().getName() + " " + verdict(t));
    }

    static int npe(Object o) { return o.hashCode(); }
    static int div(int a, int b) { return a / b; }
    static int index(int[] a, int i) { return a[i]; }
    static Object cast(Object o) { return (Integer) o; }
    static void store(Object[] a) { a[0] = "x"; }
    static int[] negative(int n) { return new int[n]; }

    /** A user subclass: its bytecode constructor chains to a shadowed one. */
    static class UserException extends Exception {
        UserException() { super(); }
        UserException(String m) { super(m); }
        UserException(String m, Throwable c) { super(m, c); }
    }

    public static void main(String[] args) throws Exception {
        // 1. Every public constructor of the shadowed family.
        ctor("java.lang.Throwable()", () -> new java.lang.Throwable());
        ctor("java.lang.Throwable(String)", () -> new java.lang.Throwable("m"));
        ctor("java.lang.Throwable(String,Throwable)", () -> new java.lang.Throwable("m", new RuntimeException("c")));
        ctor("java.lang.Throwable(Throwable)", () -> new java.lang.Throwable(new RuntimeException("c")));
        ctor("java.lang.Exception()", () -> new java.lang.Exception());
        ctor("java.lang.Exception(String)", () -> new java.lang.Exception("m"));
        ctor("java.lang.Exception(String,Throwable)", () -> new java.lang.Exception("m", new RuntimeException("c")));
        ctor("java.lang.Exception(Throwable)", () -> new java.lang.Exception(new RuntimeException("c")));
        ctor("java.lang.RuntimeException()", () -> new java.lang.RuntimeException());
        ctor("java.lang.RuntimeException(String)", () -> new java.lang.RuntimeException("m"));
        ctor("java.lang.RuntimeException(String,Throwable)", () -> new java.lang.RuntimeException("m", new RuntimeException("c")));
        ctor("java.lang.RuntimeException(Throwable)", () -> new java.lang.RuntimeException(new RuntimeException("c")));
        ctor("java.lang.Error()", () -> new java.lang.Error());
        ctor("java.lang.Error(String)", () -> new java.lang.Error("m"));
        ctor("java.lang.Error(String,Throwable)", () -> new java.lang.Error("m", new RuntimeException("c")));
        ctor("java.lang.Error(Throwable)", () -> new java.lang.Error(new RuntimeException("c")));
        ctor("java.lang.LinkageError()", () -> new java.lang.LinkageError());
        ctor("java.lang.LinkageError(String)", () -> new java.lang.LinkageError("m"));
        ctor("java.lang.LinkageError(String,Throwable)", () -> new java.lang.LinkageError("m", new RuntimeException("c")));
        ctor("java.lang.NoClassDefFoundError()", () -> new java.lang.NoClassDefFoundError());
        ctor("java.lang.NoClassDefFoundError(String)", () -> new java.lang.NoClassDefFoundError("m"));
        ctor("java.lang.SecurityException()", () -> new java.lang.SecurityException());
        ctor("java.lang.SecurityException(String)", () -> new java.lang.SecurityException("m"));
        ctor("java.lang.SecurityException(String,Throwable)", () -> new java.lang.SecurityException("m", new RuntimeException("c")));
        ctor("java.lang.SecurityException(Throwable)", () -> new java.lang.SecurityException(new RuntimeException("c")));
        ctor("java.lang.ReflectiveOperationException()", () -> new java.lang.ReflectiveOperationException());
        ctor("java.lang.ReflectiveOperationException(String)", () -> new java.lang.ReflectiveOperationException("m"));
        ctor("java.lang.ReflectiveOperationException(String,Throwable)", () -> new java.lang.ReflectiveOperationException("m", new RuntimeException("c")));
        ctor("java.lang.ReflectiveOperationException(Throwable)", () -> new java.lang.ReflectiveOperationException(new RuntimeException("c")));
        ctor("java.lang.ClassNotFoundException()", () -> new java.lang.ClassNotFoundException());
        ctor("java.lang.ClassNotFoundException(String)", () -> new java.lang.ClassNotFoundException("m"));
        ctor("java.lang.ClassNotFoundException(String,Throwable)", () -> new java.lang.ClassNotFoundException("m", new RuntimeException("c")));
        ctor("java.lang.NoSuchMethodError()", () -> new java.lang.NoSuchMethodError());
        ctor("java.lang.NoSuchMethodError(String)", () -> new java.lang.NoSuchMethodError("m"));
        ctor("java.lang.NoSuchFieldError()", () -> new java.lang.NoSuchFieldError());
        ctor("java.lang.NoSuchFieldError(String)", () -> new java.lang.NoSuchFieldError("m"));
        ctor("java.lang.NoSuchMethodException()", () -> new java.lang.NoSuchMethodException());
        ctor("java.lang.NoSuchMethodException(String)", () -> new java.lang.NoSuchMethodException("m"));
        ctor("java.lang.NoSuchFieldException()", () -> new java.lang.NoSuchFieldException());
        ctor("java.lang.NoSuchFieldException(String)", () -> new java.lang.NoSuchFieldException("m"));
        ctor("java.lang.CloneNotSupportedException()", () -> new java.lang.CloneNotSupportedException());
        ctor("java.lang.CloneNotSupportedException(String)", () -> new java.lang.CloneNotSupportedException("m"));
        ctor("java.lang.InstantiationException()", () -> new java.lang.InstantiationException());
        ctor("java.lang.InstantiationException(String)", () -> new java.lang.InstantiationException("m"));
        ctor("java.lang.IllegalAccessException()", () -> new java.lang.IllegalAccessException());
        ctor("java.lang.IllegalAccessException(String)", () -> new java.lang.IllegalAccessException("m"));
        ctor("java.lang.reflect.InaccessibleObjectException()", () -> new java.lang.reflect.InaccessibleObjectException());
        ctor("java.lang.reflect.InaccessibleObjectException(String)", () -> new java.lang.reflect.InaccessibleObjectException("m"));
        ctor("java.lang.reflect.InvocationTargetException(Throwable)", () -> new java.lang.reflect.InvocationTargetException(new RuntimeException("c")));
        ctor("java.lang.reflect.InvocationTargetException(Throwable,String)", () -> new java.lang.reflect.InvocationTargetException(new RuntimeException("c"), "m"));
        ctor("java.lang.InterruptedException()", () -> new java.lang.InterruptedException());
        ctor("java.lang.InterruptedException(String)", () -> new java.lang.InterruptedException("m"));
        ctor("java.lang.NullPointerException()", () -> new java.lang.NullPointerException());
        ctor("java.lang.NullPointerException(String)", () -> new java.lang.NullPointerException("m"));
        ctor("java.lang.ArithmeticException()", () -> new java.lang.ArithmeticException());
        ctor("java.lang.ArithmeticException(String)", () -> new java.lang.ArithmeticException("m"));
        ctor("java.lang.ArrayIndexOutOfBoundsException()", () -> new java.lang.ArrayIndexOutOfBoundsException());
        ctor("java.lang.ArrayIndexOutOfBoundsException(int)", () -> new java.lang.ArrayIndexOutOfBoundsException(7));
        ctor("java.lang.ArrayIndexOutOfBoundsException(String)", () -> new java.lang.ArrayIndexOutOfBoundsException("m"));
        ctor("java.lang.IndexOutOfBoundsException()", () -> new java.lang.IndexOutOfBoundsException());
        ctor("java.lang.IndexOutOfBoundsException(int)", () -> new java.lang.IndexOutOfBoundsException(7));
        ctor("java.lang.IndexOutOfBoundsException(String)", () -> new java.lang.IndexOutOfBoundsException("m"));
        ctor("java.lang.IndexOutOfBoundsException(long)", () -> new java.lang.IndexOutOfBoundsException(7L));
        ctor("java.lang.StringIndexOutOfBoundsException()", () -> new java.lang.StringIndexOutOfBoundsException());
        ctor("java.lang.StringIndexOutOfBoundsException(int)", () -> new java.lang.StringIndexOutOfBoundsException(7));
        ctor("java.lang.StringIndexOutOfBoundsException(String)", () -> new java.lang.StringIndexOutOfBoundsException("m"));
        ctor("java.lang.ClassCastException()", () -> new java.lang.ClassCastException());
        ctor("java.lang.ClassCastException(String)", () -> new java.lang.ClassCastException("m"));
        ctor("java.lang.IllegalArgumentException()", () -> new java.lang.IllegalArgumentException());
        ctor("java.lang.IllegalArgumentException(String)", () -> new java.lang.IllegalArgumentException("m"));
        ctor("java.lang.IllegalArgumentException(String,Throwable)", () -> new java.lang.IllegalArgumentException("m", new RuntimeException("c")));
        ctor("java.lang.IllegalArgumentException(Throwable)", () -> new java.lang.IllegalArgumentException(new RuntimeException("c")));
        ctor("java.lang.IllegalStateException()", () -> new java.lang.IllegalStateException());
        ctor("java.lang.IllegalStateException(String)", () -> new java.lang.IllegalStateException("m"));
        ctor("java.lang.IllegalStateException(String,Throwable)", () -> new java.lang.IllegalStateException("m", new RuntimeException("c")));
        ctor("java.lang.IllegalStateException(Throwable)", () -> new java.lang.IllegalStateException(new RuntimeException("c")));
        ctor("java.lang.UnsupportedOperationException()", () -> new java.lang.UnsupportedOperationException());
        ctor("java.lang.UnsupportedOperationException(String)", () -> new java.lang.UnsupportedOperationException("m"));
        ctor("java.lang.UnsupportedOperationException(String,Throwable)", () -> new java.lang.UnsupportedOperationException("m", new RuntimeException("c")));
        ctor("java.lang.UnsupportedOperationException(Throwable)", () -> new java.lang.UnsupportedOperationException(new RuntimeException("c")));
        ctor("java.lang.TypeNotPresentException(String,Throwable)", () -> new java.lang.TypeNotPresentException("m", new RuntimeException("c")));
        ctor("java.lang.StackOverflowError()", () -> new java.lang.StackOverflowError());
        ctor("java.lang.StackOverflowError(String)", () -> new java.lang.StackOverflowError("m"));
        ctor("java.lang.OutOfMemoryError()", () -> new java.lang.OutOfMemoryError());
        ctor("java.lang.OutOfMemoryError(String)", () -> new java.lang.OutOfMemoryError("m"));
        ctor("java.util.NoSuchElementException()", () -> new java.util.NoSuchElementException());
        ctor("java.util.NoSuchElementException(String)", () -> new java.util.NoSuchElementException("m"));
        ctor("java.util.NoSuchElementException(String,Throwable)", () -> new java.util.NoSuchElementException("m", new RuntimeException("c")));
        ctor("java.util.NoSuchElementException(Throwable)", () -> new java.util.NoSuchElementException(new RuntimeException("c")));
        ctor("java.util.InputMismatchException()", () -> new java.util.InputMismatchException());
        ctor("java.util.InputMismatchException(String)", () -> new java.util.InputMismatchException("m"));
        ctor("java.util.MissingResourceException(String,String,String)", () -> new java.util.MissingResourceException("m", "m", "m"));
        ctor("java.util.FormatterClosedException()", () -> new java.util.FormatterClosedException());
        ctor("java.io.IOException()", () -> new java.io.IOException());
        ctor("java.io.IOException(String)", () -> new java.io.IOException("m"));
        ctor("java.io.IOException(String,Throwable)", () -> new java.io.IOException("m", new RuntimeException("c")));
        ctor("java.io.IOException(Throwable)", () -> new java.io.IOException(new RuntimeException("c")));
        ctor("java.io.FileNotFoundException()", () -> new java.io.FileNotFoundException());
        ctor("java.io.FileNotFoundException(String)", () -> new java.io.FileNotFoundException("m"));
        ctor("java.io.UncheckedIOException(IOException)", () -> new java.io.UncheckedIOException(new java.io.IOException("c")));
        ctor("java.io.UncheckedIOException(String,IOException)", () -> new java.io.UncheckedIOException("m", new java.io.IOException("c")));
        ctor("java.io.NotSerializableException()", () -> new java.io.NotSerializableException());
        ctor("java.io.NotSerializableException(String)", () -> new java.io.NotSerializableException("m"));
        ctor("java.io.EOFException()", () -> new java.io.EOFException());
        ctor("java.io.EOFException(String)", () -> new java.io.EOFException("m"));
        ctor("java.io.UnsupportedEncodingException()", () -> new java.io.UnsupportedEncodingException());
        ctor("java.io.UnsupportedEncodingException(String)", () -> new java.io.UnsupportedEncodingException("m"));
        ctor("java.net.MalformedURLException()", () -> new java.net.MalformedURLException());
        ctor("java.net.MalformedURLException(String)", () -> new java.net.MalformedURLException("m"));
        ctor("java.net.UnknownHostException()", () -> new java.net.UnknownHostException());
        ctor("java.net.UnknownHostException(String)", () -> new java.net.UnknownHostException("m"));
        ctor("java.lang.NumberFormatException()", () -> new java.lang.NumberFormatException());
        ctor("java.lang.NumberFormatException(String)", () -> new java.lang.NumberFormatException("m"));
        ctor("java.util.ConcurrentModificationException()", () -> new java.util.ConcurrentModificationException());
        ctor("java.util.ConcurrentModificationException(String)", () -> new java.util.ConcurrentModificationException("m"));
        ctor("java.util.ConcurrentModificationException(String,Throwable)", () -> new java.util.ConcurrentModificationException("m", new RuntimeException("c")));
        ctor("java.util.ConcurrentModificationException(Throwable)", () -> new java.util.ConcurrentModificationException(new RuntimeException("c")));
        ctor("java.util.concurrent.TimeoutException()", () -> new java.util.concurrent.TimeoutException());
        ctor("java.util.concurrent.TimeoutException(String)", () -> new java.util.concurrent.TimeoutException("m"));
        ctor("java.util.concurrent.RejectedExecutionException()", () -> new java.util.concurrent.RejectedExecutionException());
        ctor("java.util.concurrent.RejectedExecutionException(String)", () -> new java.util.concurrent.RejectedExecutionException("m"));
        ctor("java.util.concurrent.RejectedExecutionException(String,Throwable)", () -> new java.util.concurrent.RejectedExecutionException("m", new RuntimeException("c")));
        ctor("java.util.concurrent.RejectedExecutionException(Throwable)", () -> new java.util.concurrent.RejectedExecutionException(new RuntimeException("c")));
        ctor("java.util.concurrent.CancellationException()", () -> new java.util.concurrent.CancellationException());
        ctor("java.util.concurrent.CancellationException(String)", () -> new java.util.concurrent.CancellationException("m"));
        ctor("java.util.concurrent.CompletionException(String,Throwable)", () -> new java.util.concurrent.CompletionException("m", new RuntimeException("c")));
        ctor("java.util.concurrent.CompletionException(Throwable)", () -> new java.util.concurrent.CompletionException(new RuntimeException("c")));
        ctor("java.util.concurrent.ExecutionException(String,Throwable)", () -> new java.util.concurrent.ExecutionException("m", new RuntimeException("c")));
        ctor("java.util.concurrent.ExecutionException(Throwable)", () -> new java.util.concurrent.ExecutionException(new RuntimeException("c")));
        ctor("java.util.concurrent.BrokenBarrierException()", () -> new java.util.concurrent.BrokenBarrierException());
        ctor("java.util.concurrent.BrokenBarrierException(String)", () -> new java.util.concurrent.BrokenBarrierException("m"));
        ctor("java.text.ParseException(String,int)", () -> new java.text.ParseException("m", 7));
        ctor("java.lang.NegativeArraySizeException()", () -> new java.lang.NegativeArraySizeException());
        ctor("java.lang.NegativeArraySizeException(String)", () -> new java.lang.NegativeArraySizeException("m"));
        ctor("java.lang.AssertionError()", () -> new java.lang.AssertionError());
        ctor("java.lang.AssertionError(boolean)", () -> new java.lang.AssertionError(true));
        ctor("java.lang.AssertionError(int)", () -> new java.lang.AssertionError(7));
        ctor("java.lang.AssertionError(String,Throwable)", () -> new java.lang.AssertionError("m", new RuntimeException("c")));
        ctor("java.lang.AssertionError(long)", () -> new java.lang.AssertionError(7L));
        ctor("java.lang.MatchException(String,Throwable)", () -> new java.lang.MatchException("m", new RuntimeException("c")));
        ctor("java.lang.IncompatibleClassChangeError()", () -> new java.lang.IncompatibleClassChangeError());
        ctor("java.lang.IncompatibleClassChangeError(String)", () -> new java.lang.IncompatibleClassChangeError("m"));
        ctor("java.lang.IllegalAccessError()", () -> new java.lang.IllegalAccessError());
        ctor("java.lang.IllegalAccessError(String)", () -> new java.lang.IllegalAccessError("m"));
        ctor("java.lang.ExceptionInInitializerError()", () -> new java.lang.ExceptionInInitializerError());
        ctor("java.lang.ExceptionInInitializerError(String)", () -> new java.lang.ExceptionInInitializerError("m"));
        ctor("java.lang.ExceptionInInitializerError(Throwable)", () -> new java.lang.ExceptionInInitializerError(new RuntimeException("c")));
        ctor("java.lang.VerifyError()", () -> new java.lang.VerifyError());
        ctor("java.lang.VerifyError(String)", () -> new java.lang.VerifyError("m"));
        ctor("java.lang.AbstractMethodError()", () -> new java.lang.AbstractMethodError());
        ctor("java.lang.AbstractMethodError(String)", () -> new java.lang.AbstractMethodError("m"));
        ctor("java.lang.InternalError()", () -> new java.lang.InternalError());
        ctor("java.lang.InternalError(String)", () -> new java.lang.InternalError("m"));
        ctor("java.lang.InternalError(String,Throwable)", () -> new java.lang.InternalError("m", new RuntimeException("c")));
        ctor("java.lang.InternalError(Throwable)", () -> new java.lang.InternalError(new RuntimeException("c")));
        ctor("java.lang.UnsatisfiedLinkError()", () -> new java.lang.UnsatisfiedLinkError());
        ctor("java.lang.UnsatisfiedLinkError(String)", () -> new java.lang.UnsatisfiedLinkError("m"));

        // 2. A user subclass, through each super constructor shape.
        ctor("UserException()", () -> new UserException());
        ctor("UserException(String)", () -> new UserException("m"));
        ctor("UserException(String,Throwable)", () -> new UserException("m", new RuntimeException("c")));
        ctor("UserException(String,null)", () -> new UserException("m", null));

        // 3. Throwables the VM builds itself: implicit exceptions.
        try { npe(null); } catch (NullPointerException e) { caught("implicit", e); }
        try { div(1, args.length); } catch (ArithmeticException e) { caught("implicit", e); }
        try { index(new int[1], 3); } catch (ArrayIndexOutOfBoundsException e) { caught("implicit", e); }
        try { cast("s"); } catch (ClassCastException e) { caught("implicit", e); }
        try { store(new Integer[1]); } catch (ArrayStoreException e) { caught("implicit", e); }
        try { negative(-1 - args.length); } catch (NegativeArraySizeException e) { caught("implicit", e); }

        // 4. Regex syntax errors: `String.replaceAll` & co. are native
        //    intrinsics that raise `PatternSyntaxException` from the VM, and
        //    that class has no ()V / (String)V constructor for it to call.
        try { Pattern.compile("("); } catch (PatternSyntaxException e) { caught("Pattern.compile", e); }
        try { "abc".matches("(("); } catch (PatternSyntaxException e) { caught("String.matches", e); }
        try { "abc".split("[z"); } catch (PatternSyntaxException e) { caught("String.split", e); }
        try { "abc".replaceAll("(", "x"); } catch (PatternSyntaxException e) { caught("String.replaceAll", e); }
        try { "abc".replaceFirst("(", "x"); } catch (PatternSyntaxException e) { caught("String.replaceFirst", e); }

        // 5. No constructor at all: HotSpot leaves both fields null, so
        //    `initCause` refuses and `addSuppressed` drops.
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) f.get(null);
        caught("allocateInstance", (Throwable) u.allocateInstance(IllegalStateException.class));
        caught("allocateInstance", (Throwable) u.allocateInstance(RuntimeException.class));
        caught("allocateInstance", (Throwable) u.allocateInstance(UserException.class));

        System.out.println("PASS RThrowableInitialisers (" + checks + " checks)");
    }
}
