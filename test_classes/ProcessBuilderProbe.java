// SPDX-License-Identifier: Apache-2.0
// Probe for the "[ARRAY-LEN-GUARD] non-array object class=java/util/ArrayList"
// bug surfaced by jline / aesh InfoCmp.getInfoCmp(String) and
// ExecHelper.exec(boolean, String...). Both paths construct a
// `new ProcessBuilder(new String[]{ ... })` and immediately call
// `start()`. The native ProcessBuilder.<init>([Ljava/lang/String;)V
// shim stored the String[] directly, but a separate native or
// reflection layer wrapped it in an ArrayList, and the consumer
// then did `arraylength` on the ArrayList — triggering the guard.
public class ProcessBuilderProbe {
    public static void main(String[] args) throws Exception {
        // (1) ProcessBuilder ctor that accepts varargs — what JLine uses.
        String[] cmd = { "cmd.exe", "/c", "echo", "hello" };
        ProcessBuilder pb = new ProcessBuilder(cmd);
        // (2) command() must return a List view, not the raw array.
        java.util.List<String> view = pb.command();
        if (view == null) {
            throw new AssertionError("ProcessBuilder.command() returned null");
        }
        if (view.size() != cmd.length) {
            throw new AssertionError("ProcessBuilder.command().size()=" + view.size() + " (expected " + cmd.length + ")");
        }
        System.out.println("ok command(): size=" + view.size());

        // (3) Round-trip: command(List) then command().
        java.util.List<String> alt = new java.util.ArrayList<>();
        alt.add("ls");
        alt.add("-la");
        pb.command(alt);
        java.util.List<String> view2 = pb.command();
        if (view2.size() != 2) {
            throw new AssertionError("after command(List) size=" + view2.size());
        }
        System.out.println("ok command(List): size=" + view2.size());

        // (4) Iterate — exercise the path that does arraylength internally.
        int n = 0;
        for (String s : pb.command()) {
            if (s == null) throw new AssertionError("null command element at " + n);
            n++;
        }
        if (n != 2) throw new AssertionError("iterated " + n + " (expected 2)");
        System.out.println("ok iterate: n=" + n);

        System.out.println("ProcessBuilderProbe PASS");
    }
}
