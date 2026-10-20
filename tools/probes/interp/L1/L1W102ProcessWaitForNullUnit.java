// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L1: `Process.waitFor(long, null)`.
//
// Every JDK 25 body -- Windows and UNIX `ProcessImpl.waitFor(long, TimeUnit)`
// and the default `Process.waitFor(long, TimeUnit)` a subclass inherits --
// starts with `unit.toNanos(timeout); // throw NPE before other conditions`.
// The `--compatible` stand-in (native-io/src/process.rs,
// `native_process_wait_for_timeout`) read a null unit as milliseconds: an
// exited child answered `true`, a zero timeout `false`, a running child
// waited. `--jdk-only` runs the JDK bodies and always matched. Fixed, not
// built or run.
//
// Portable child: `cmd /c exit 3` / `ping -n 30 127.0.0.1` on Windows,
// `sh -c "exit 3"` / `sleep 30` elsewhere. No arguments, no setup:
//   cratonvm --compatible L1W102ProcessWaitForNullUnit
//   cratonvm L1W102ProcessWaitForNullUnit
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources and the
// CountDownLatch.await(1, null) text measured in L1W71CountDownLatchBodies)
// exited waitFor(1, null): java.lang.NullPointerException: Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "unit" is null
// running waitFor(0, null): java.lang.NullPointerException: Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "unit" is null
// running waitFor(50, null): java.lang.NullPointerException: Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "unit" is null
// subclass waitFor(1, null): java.lang.NullPointerException: Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "unit" is null
// done
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.io.OutputStream;
import java.util.concurrent.TimeUnit;

public class L1W102ProcessWaitForNullUnit {
    static final boolean WINDOWS = System.getProperty("os.name").startsWith("Windows");

    interface Call {
        Object run() throws Exception;
    }

    static void row(String label, Call c) {
        String out;
        try {
            out = String.valueOf(c.run());
        } catch (Exception e) {
            out = e.toString();
        }
        System.out.println(label + ": " + out);
    }

    /** A Process subclass that inherits Process.waitFor(long, TimeUnit). */
    static final class Done extends Process {
        public OutputStream getOutputStream() {
            return OutputStream.nullOutputStream();
        }

        public InputStream getInputStream() {
            return InputStream.nullInputStream();
        }

        public InputStream getErrorStream() {
            return InputStream.nullInputStream();
        }

        public int waitFor() {
            return 0;
        }

        public int exitValue() {
            return 0;
        }

        public void destroy() {
        }
    }

    public static void main(String[] args) throws Exception {
        Process exited = (WINDOWS
                ? new ProcessBuilder("cmd", "/c", "exit", "3")
                : new ProcessBuilder("sh", "-c", "exit 3")).start();
        exited.waitFor();
        row("exited waitFor(1, null)", () -> exited.waitFor(1, null));

        Process running = (WINDOWS
                ? new ProcessBuilder("ping", "-n", "30", "127.0.0.1")
                : new ProcessBuilder("sleep", "30"))
                .redirectOutput(ProcessBuilder.Redirect.DISCARD).start();
        row("running waitFor(0, null)", () -> running.waitFor(0, null));
        row("running waitFor(50, null)", () -> running.waitFor(50, null));
        running.destroyForcibly();
        running.waitFor(60, TimeUnit.SECONDS);

        Process subclass = new Done();
        row("subclass waitFor(1, null)", () -> subclass.waitFor(1, null));
        System.out.println("done");
    }
}
