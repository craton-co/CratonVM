// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- the texts of a failed static
// initializer and of every later use of the class (JDK-8048190: the
// NoClassDefFoundError carries an ExceptionInInitializerError whose message is
// "Exception <class>[: <detailMessage>] [in thread "<name>"]"):
//
//   plain       RuntimeException("boom") in <clinit>
//   nomsg       an exception with no message
//   override    an exception whose getMessage() is overridden: HotSpot reads
//               the detailMessage FIELD (java_lang_Throwable::message)
//   error       an Error is not wrapped on the first use
//   thread      the failure happens in a thread named "init-worker"; the
//               later use in main still names "init-worker"
//   npe         an implicit NullPointerException in <clinit>: its JEP 358
//               text is not a detailMessage on HotSpot, so the recorded
//               error names none
//   vthread     the failure happens in a VIRTUAL thread named "vt-init":
//               HotSpot names the JavaThread's threadObj(), the CARRIER
//               (a ForkJoinPool worker), never the virtual thread's own name
//   frames      the cause's stack trace is the original exception's
//               (<clinit> on top); printed as method names only
//   forname     Class.forName(.., true, ..) on an erroneous class
//
//   $J/javac -d out tools/probes/interp/L9/L9W98InitErrorTexts.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W98InitErrorTexts
//   $J/java -cp out L9W98InitErrorTexts
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// plain first: java.lang.ExceptionInInitializerError cause java.lang.RuntimeException: boom
// plain again: java.lang.NoClassDefFoundError: Could not initialize class L9W98InitErrorTexts$Plain
// plain cause: java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: boom [in thread "main"]
// nomsg first: java.lang.ExceptionInInitializerError cause java.lang.IllegalStateException
// nomsg cause: java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException [in thread "main"]
// override first: java.lang.ExceptionInInitializerError cause L9W98InitErrorTexts$Overriding: overridden
// override cause: java.lang.ExceptionInInitializerError: Exception L9W98InitErrorTexts$Overriding: field [in thread "main"]
// error first: L9W98InitErrorTexts$CustomError: bad
// error cause: java.lang.ExceptionInInitializerError: Exception L9W98InitErrorTexts$CustomError: bad [in thread "main"]
// npe first: java.lang.ExceptionInInitializerError cause java.lang.NullPointerException: Cannot invoke "String.length()" because "L9W98InitErrorTexts$NpeInit.S" is null
// npe again: java.lang.NoClassDefFoundError: Could not initialize class L9W98InitErrorTexts$NpeInit
// npe cause: java.lang.ExceptionInInitializerError: Exception java.lang.NullPointerException [in thread "main"]
// thread first: java.lang.ExceptionInInitializerError cause java.lang.RuntimeException: in worker
// thread cause: java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: in worker [in thread "init-worker"]
// vthread first: java.lang.ExceptionInInitializerError cause java.lang.RuntimeException: in vthread
// vthread names-virtual-thread: false
// vthread names-carrier: true
// frames: L9W98InitErrorTexts$Framed.fail L9W98InitErrorTexts$Framed.<clinit> L9W98InitErrorTexts.touchFramed L9W98InitErrorTexts.attempt L9W98InitErrorTexts.main
// forname: java.lang.NoClassDefFoundError: Could not initialize class L9W98InitErrorTexts$Plain
// HOTSPOT_EXPECTED_END
public class L9W98InitErrorTexts {
    static class Plain {
        static int V;
        static {
            if (true) throw new RuntimeException("boom");
        }
    }

    static class NoMsg {
        static int V;
        static {
            if (true) throw new IllegalStateException();
        }
    }

    static class Overriding extends RuntimeException {
        Overriding() {
            super("field");
        }

        @Override
        public String getMessage() {
            return "overridden";
        }
    }

    static class OverrideInit {
        static int V;
        static {
            if (true) throw new Overriding();
        }
    }

    static class CustomError extends Error {
        CustomError(String m) {
            super(m);
        }
    }

    static class Err {
        static int V;
        static {
            if (true) throw new CustomError("bad");
        }
    }

    static class InThread {
        static int V;
        static {
            if (true) throw new RuntimeException("in worker");
        }
    }

    static class InVThread {
        static int V;
        static {
            if (true) throw new RuntimeException("in vthread");
        }
    }

    static class NpeInit {
        static String S;
        static int V = S.length();
    }

    static class Framed {
        static int V;
        static {
            fail();
        }

        static void fail() {
            throw new RuntimeException("framed");
        }
    }

    interface Touch {
        int run();
    }

    static Throwable attempt(Touch t) {
        try {
            t.run();
            return null;
        } catch (Throwable e) {
            return e;
        }
    }

    static String first(Throwable t) {
        if (t instanceof ExceptionInInitializerError) {
            return t.getClass().getName() + " cause " + t.getCause();
        }
        return String.valueOf(t);
    }

    static void again(String label, Touch t) {
        Throwable second = attempt(t);
        if (!label.equals("nomsg") && !label.equals("override") && !label.equals("error")) {
            System.out.println(label + " again: " + second);
        }
        System.out.println(label + " cause: " + (second == null ? null : second.getCause()));
    }

    static int touchFramed() {
        return Framed.V;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("plain first: " + first(attempt(() -> Plain.V)));
        again("plain", () -> Plain.V);
        System.out.println("nomsg first: " + first(attempt(() -> NoMsg.V)));
        again("nomsg", () -> NoMsg.V);
        System.out.println("override first: " + first(attempt(() -> OverrideInit.V)));
        again("override", () -> OverrideInit.V);
        System.out.println("error first: " + first(attempt(() -> Err.V)));
        again("error", () -> Err.V);

        System.out.println("npe first: " + first(attempt(() -> NpeInit.V)));
        again("npe", () -> NpeInit.V);

        Throwable[] inWorker = new Throwable[1];
        Thread worker = new Thread(() -> inWorker[0] = attempt(() -> InThread.V), "init-worker");
        worker.start();
        worker.join();
        System.out.println("thread first: " + first(inWorker[0]));
        System.out.println("thread cause: " + attempt(() -> InThread.V).getCause());

        Throwable[] inVirtual = new Throwable[1];
        Thread vt = Thread.ofVirtual().name("vt-init").start(() -> inVirtual[0] = attempt(() -> InVThread.V));
        vt.join();
        System.out.println("vthread first: " + first(inVirtual[0]));
        String vtCause = String.valueOf(attempt(() -> InVThread.V).getCause());
        System.out.println("vthread names-virtual-thread: " + vtCause.contains("[in thread \"vt-init\"]"));
        System.out.println("vthread names-carrier: " + vtCause.contains("[in thread \"ForkJoinPool"));

        attempt(L9W98InitErrorTexts::touchFramed);
        Throwable later = attempt(L9W98InitErrorTexts::touchFramed);
        StringBuilder frames = new StringBuilder("frames:");
        for (StackTraceElement e : later.getCause().getStackTrace()) {
            frames.append(' ').append(e.getClassName()).append('.').append(e.getMethodName());
        }
        System.out.println(frames);

        try {
            Class.forName("L9W98InitErrorTexts$Plain", true, L9W98InitErrorTexts.class.getClassLoader());
            System.out.println("forname: ok");
        } catch (Throwable t) {
            System.out.println("forname: " + t);
        }
    }
}
