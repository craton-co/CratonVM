// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L3: an IDE-HotSwap-shaped redefinition
// that edits the BODY of a lambda (javac output of the edited class, renamed
// in place and installed with Instrumentation.redefineClasses). The lambda's
// synthetic method (`lambda$make$0`) keeps its name and descriptor, so the
// redefinition is allowed; a lambda object created BEFORE it, whose call site
// was linked to that method, is called after it.
//
//   linked  -- the Supplier created before the redefinition, called after.
//   fresh   -- a Supplier made by a call after the redefinition.
//   warm    -- the same for a lambda called 50 000 times first (compiled).
//
// HotSpot 25 prints (agent; the same with -Xint):
//     linked=lamB fresh=lamB
//     warm-linked=hotB warm-fresh=hotB
// A linked lambda runs the edited body.
// CratonVM, read from the code (not run): expected to match -- a lambda call
// site keeps its implementation method symbolically and re-resolves it
// (`invokedynamic::tests::lambda_impl_handle_is_symbolic_so_impl_redefinition_needs_no_eviction`),
// and the cached templates of the impl method are gated on its class's
// redefine generation (`interpreter::lambda::build_lambda_impl_cached`). A
// measurement for the HotSwap case (editing a lambda is the commonest IDE
// HotSwap edit), not a fix.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W37HotSwapLambdaBody$Agent
//     Can-Redefine-Classes: true
// containing L3W37HotSwapLambdaBody*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W37HotSwapLambdaBody
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.util.function.Supplier;

public class L3W37HotSwapLambdaBody {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The class as first loaded. */
    public static class Tgt {
        public static Supplier<String> make() {
            return () -> "lamA";
        }

        public static Supplier<String> hot() {
            return () -> "hotA";
        }
    }

    /** The edited source, as javac compiled it: renamed to Tgt before use. */
    public static class Dgt {
        public static Supplier<String> make() {
            return () -> "lamB";
        }

        public static Supplier<String> hot() {
            return () -> "hotB";
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W37HotSwapLambdaBody.class
                .getResourceAsStream("L3W37HotSwapLambdaBody$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W37HotSwapLambdaBody$" + donor;
        String to = "L3W37HotSwapLambdaBody$" + target;
        for (int i = 0; i + from.length() <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length() && match; k++) {
                match = b[i + k] == (byte) from.charAt(k);
            }
            if (match) {
                for (int k = 0; k < to.length(); k++) {
                    b[i + k] = (byte) to.charAt(k);
                }
            }
        }
        return b;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        Supplier<String> linked = Tgt.make();
        linked.get();
        Supplier<String> hot = Tgt.hot();
        int n = 0;
        for (int k = 0; k < 50_000; k++) {
            n += hot.get().length();
        }
        i.redefineClasses(new ClassDefinition(Tgt.class, renamed(bytesOf("Dgt"), "Dgt", "Tgt")));
        System.out.println("linked=" + linked.get() + " fresh=" + Tgt.make().get());
        System.out.println("warm-linked=" + hot.get() + " warm-fresh=" + Tgt.hot().get()
                + (n == 200_000 ? "" : " n=" + n));
    }
}
