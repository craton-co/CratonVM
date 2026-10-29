// SPDX-License-Identifier: Apache-2.0
// Regenerate with: javac --release 17 -d . JitCompileDecision.java
package cratonvm.jfr;
@jdk.jfr.Name("cratonvm.JitCompileDecision")
@jdk.jfr.Label("JIT Compile Decision")
@jdk.jfr.Category({"Java Virtual Machine", "Compiler"})
@jdk.jfr.Enabled(false)
@jdk.jfr.StackTrace(false)
public final class JitCompileDecision extends jdk.jfr.Event {
    public String method, door, outcome, reason;
    public int bailBci, bailOpcode, bytecodeSize;
    public static void initialize() {}
}
