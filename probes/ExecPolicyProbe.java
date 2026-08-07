import java.io.*;

/**
 * Does the SecurityManager.checkExec gate cover BOTH spawn entry points?
 *
 * Runtime.exec is documented to consult checkExec, and so is
 * ProcessBuilder.start -- they are the same gate, and a policy that only
 * covers one of them is not a policy. Prints, for each entry point, whether
 * the spawn was refused and whether the child ran anyway (the marker file is
 * the ground truth: a refusal that still forks is worse than no gate).
 *
 * Not runnable on HotSpot 25 (setSecurityManager throws UOE there); this is a
 * CratonVM-internal consistency check between two of its own natives.
 */
public class ExecPolicyProbe {

    static class DenyExec extends SecurityManager {
        @Override
        public void checkExec(String cmd) {
            throw new SecurityException("denied by probe: " + cmd);
        }
        @Override
        public void checkPermission(java.security.Permission p) {
            // allow everything else, including re-setting the SM
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = new File(System.getProperty("java.io.tmpdir"), "execpolicy" + System.nanoTime());
        dir.mkdirs();
        File marker1 = new File(dir, "ran-exec");
        File marker2 = new File(dir, "ran-pb");
        File script = new File(dir, "touch.sh");
        try (FileWriter fw = new FileWriter(script)) {
            fw.write("#!/bin/sh\ntouch \"$1\"\n");
        }
        script.setExecutable(true);

        System.setSecurityManager(new DenyExec());

        String execKind;
        try {
            Runtime.getRuntime().exec(new String[] { script.getAbsolutePath(),
                    marker1.getAbsolutePath() });
            execKind = "NOT_REFUSED";
        } catch (SecurityException e) {
            execKind = "SecurityException";
        } catch (Throwable t) {
            execKind = t.getClass().getName();
        }

        String pbKind;
        try {
            new ProcessBuilder(script.getAbsolutePath(), marker2.getAbsolutePath()).start();
            pbKind = "NOT_REFUSED";
        } catch (SecurityException e) {
            pbKind = "SecurityException";
        } catch (Throwable t) {
            pbKind = t.getClass().getName();
        }

        System.setSecurityManager(null);
        Thread.sleep(500);

        System.out.println("EXEC_REFUSED_AS=" + execKind);
        System.out.println("EXEC_CHILD_RAN=" + marker1.exists());
        System.out.println("PB_REFUSED_AS=" + pbKind);
        System.out.println("PB_CHILD_RAN=" + marker2.exists());
        System.out.println("PROBE_DONE");
    }
}
