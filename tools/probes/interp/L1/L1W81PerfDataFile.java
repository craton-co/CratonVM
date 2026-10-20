// Interpreter round i3 wave 81, lane L1: the jvmstat perf-data file
// `/tmp/hsperfdata_<user>/<pid>` that `jps`, `VirtualMachine.list()` and an
// external attach's `testAttachable` read, against HotSpot
// (docs/internal/fixed-bugs/interpreter-i59-external-disabled-attach-discovery-FIXED-20261011.md).
//
// The probe runs a child of the same VM (the current process's executable,
// else `<java.home>/bin/java`) twice: with default flags (the child returns
// from `main` once its stdin closes) and with -XX:+DisableAttachMechanism
// (the child calls System.exit(0)). While the child runs, the parent reads
// the child's file with plain NIO and walks it as jvmstat's v2.0
// `PerfDataBuffer` does, printing the file's size and permissions, the
// prologue's fixed fields, the first two characters of
// `sun.rt.jvmCapabilities` (the attach bit first), `sun.rt.javaCommand`,
// and whether `java.property.java.vm.name` / `java.rt.vmArgs` are there;
// then whether the file is gone once the child exited. Linux only (HotSpot
// keeps its Windows data in shared memory): elsewhere it prints `skipped`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// child (default)
//   ready
//   file: size=32768 mode=rw------- dir=rwxr-xr-x
//   prologue: magic=cafec0c0 order=1 version=2.0 accessible=1 entryOffset=32 walked=true
//   sun.rt.jvmCapabilities starts 11
//   sun.rt.javaCommand=L1W81PerfDataFile child
//   java.property.java.vm.name present=true
//   java.rt.vmArgs present=true
//   after exit: exists=false
// child (-XX:+DisableAttachMechanism)
//   ready
//   file: size=32768 mode=rw------- dir=rwxr-xr-x
//   prologue: magic=cafec0c0 order=1 version=2.0 accessible=1 entryOffset=32 walked=true
//   sun.rt.jvmCapabilities starts 01
//   sun.rt.javaCommand=L1W81PerfDataFile child-exit
//   java.property.java.vm.name present=true
//   java.rt.vmArgs present=true
//   after exit: exists=false
// HOTSPOT_EXPECTED_END
//
// The block is derived from the host oracle of 2026-10-09 (HotSpot 25.0.4
// on Linux, `m81`/`hsperf.py`: the 32768-byte 0600 file in a 0755 directory,
// prologue `cafec0c0 01 02 00 01 ...`, entry offset 32, the capabilities
// `11...` by default and `01...` under the flag) and the JDK 25 jvmstat
// sources (the walk, `sun.java.command` as the launcher sets it, the file
// unlinked by `perfMemory_exit`). Refreshed on HotSpot 25.0.4+7 during
// i59 retirement; i59-serviceability-gate.py compares the full output.
//
// CratonVM before wave 81 (code read): no file (`file: missing`).
// Wave 81 added opt-in publication; i59 retirement enables it by default.
// CRATONVM_PERF_DATA_FILE=0 is the explicit rollback (formerly default
// off), which the children inherit:
//
//   javac -d /tmp/l1w81perf tools/probes/interp/L1/L1W81PerfDataFile.java
//   java     -cp /tmp/l1w81perf L1W81PerfDataFile
//   cratonvm -cp /tmp/l1w81perf L1W81PerfDataFile
//   CRATONVM_PERF_DATA_FILE=1 cratonvm -cp /tmp/l1w81perf L1W81PerfDataFile
import java.io.BufferedReader;
import java.io.File;
import java.io.InputStreamReader;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermissions;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;

public class L1W81PerfDataFile {
    static void child(boolean exit) throws Exception {
        System.out.println("ready");
        System.out.flush();
        // Wait for the parent to close stdin.
        while (System.in.read() != -1) {
            // drain
        }
        if (exit) {
            System.exit(0);
        }
    }

    static void report(Path file) throws Exception {
        if (!Files.isRegularFile(file)) {
            System.out.println("  file: missing");
            return;
        }
        byte[] bytes = Files.readAllBytes(file);
        String mode = PosixFilePermissions.toString(Files.getPosixFilePermissions(file));
        String dirMode = PosixFilePermissions.toString(Files.getPosixFilePermissions(file.getParent()));
        System.out.println("  file: size=" + bytes.length + " mode=" + mode + " dir=" + dirMode);
        if (bytes.length < 32) {
            return;
        }
        ByteBuffer b = ByteBuffer.wrap(bytes);
        String magic = String.format("%02x%02x%02x%02x", bytes[0], bytes[1], bytes[2], bytes[3]);
        int order = bytes[4];
        b.order(order == 0 ? ByteOrder.BIG_ENDIAN : ByteOrder.LITTLE_ENDIAN);
        int entryOffset = b.getInt(24);
        int numEntries = b.getInt(28);
        Map<String, String> strings = new LinkedHashMap<>();
        boolean walked = true;
        int next = entryOffset;
        int seen = 0;
        while (next < bytes.length) {
            if (next % 4 != 0 || next + 20 > bytes.length) {
                walked = false;
                break;
            }
            int length = b.getInt(next);
            if (length == 0) {
                break;
            }
            if (length < 0 || next + length > bytes.length) {
                walked = false;
                break;
            }
            int nameOffset = b.getInt(next + 4);
            int vectorLength = b.getInt(next + 8);
            byte type = bytes[next + 12];
            int dataOffset = b.getInt(next + 16);
            int nameEnd = next + nameOffset;
            while (nameEnd < next + dataOffset && bytes[nameEnd] != 0) {
                nameEnd++;
            }
            String name = new String(bytes, next + nameOffset, nameEnd - next - nameOffset, StandardCharsets.UTF_8);
            if (type == 'B' && vectorLength > 0) {
                int start = next + dataOffset;
                int end = start;
                while (end < start + vectorLength && bytes[end] != 0) {
                    end++;
                }
                strings.put(name, new String(bytes, start, end - start, StandardCharsets.UTF_8));
            }
            next += length;
            seen++;
        }
        walked = walked && seen == numEntries;
        System.out.println("  prologue: magic=" + magic + " order=" + order + " version=" + bytes[5] + "." + bytes[6]
                + " accessible=" + bytes[7] + " entryOffset=" + entryOffset + " walked=" + walked);
        String caps = strings.get("sun.rt.jvmCapabilities");
        System.out.println("  sun.rt.jvmCapabilities starts "
                + (caps == null ? "<none>" : caps.substring(0, Math.min(2, caps.length()))));
        System.out.println("  sun.rt.javaCommand=" + strings.get("sun.rt.javaCommand"));
        System.out.println("  java.property.java.vm.name present=" + strings.containsKey("java.property.java.vm.name"));
        System.out.println("  java.rt.vmArgs present=" + strings.containsKey("java.rt.vmArgs"));
    }

    static void run(String launcher, String extra) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(launcher);
        if (extra != null) {
            cmd.add(extra);
        }
        cmd.add("-cp");
        cmd.add(System.getProperty("java.class.path"));
        cmd.add("L1W81PerfDataFile");
        cmd.add(extra == null ? "child" : "child-exit");
        System.out.println("child (" + (extra == null ? "default" : extra) + ")");
        Process p = new ProcessBuilder(cmd).redirectError(ProcessBuilder.Redirect.DISCARD).start();
        BufferedReader out = new BufferedReader(new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8));
        String first = out.readLine();
        System.out.println("  " + first);
        Path file = Path.of("/tmp", "hsperfdata_" + System.getProperty("user.name"), String.valueOf(p.pid()));
        report(file);
        p.getOutputStream().close();
        while (out.readLine() != null) {
            // drain
        }
        boolean done = p.waitFor(60, TimeUnit.SECONDS);
        if (!done) {
            p.destroyForcibly();
            System.out.println("  timeout");
            return;
        }
        System.out.println("  after exit: exists=" + Files.exists(file));
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].startsWith("child")) {
            child(args[0].equals("child-exit"));
            return;
        }
        if (!System.getProperty("os.name").startsWith("Linux")) {
            System.out.println("skipped");
            return;
        }
        String home = System.getProperty("java.home");
        String fallback = home + File.separator + "bin" + File.separator + "java";
        String launcher = ProcessHandle.current().info().command().orElse(fallback);
        run(launcher, null);
        run(launcher, "-XX:+DisableAttachMechanism");
    }
}
