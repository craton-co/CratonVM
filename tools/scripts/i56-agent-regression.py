#!/usr/bin/env python3
"""HotSpot oracle for the real Mockito/JaCoCo residuals of i56.

Pass dependency JARs explicitly: this runner does not substitute agents or
download dependencies. A JaCoCo run passes only if HotSpot reads its execution
data and finds the executed CoverageTarget probes.
"""
import argparse
import os
import pathlib
import subprocess
import sys
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("cvm", "jdk", "work", "mockito-core", "byte-buddy", "byte-buddy-agent",
                 "objenesis", "jacoco-agent", "jacoco-core"):
        parser.add_argument("--" + name, type=pathlib.Path, required=True)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    classes = work / "classes"
    agent_classes = work / "agent-classes"
    classes.mkdir(exist_ok=True)
    agent_classes.mkdir(exist_ok=True)
    suffix = ".exe" if sys.platform == "win32" else ""
    java = args.jdk.resolve() / "bin" / ("java" + suffix)
    javac = args.jdk.resolve() / "bin" / ("javac" + suffix)
    cvm = args.cvm.resolve()
    mock_cp = os.pathsep.join(str(p.resolve()) for p in (
        classes, args.mockito_core, args.byte_buddy, args.byte_buddy_agent, args.objenesis))
    sources = {
        "MockitoDriver": '''import static org.mockito.Mockito.*;
public class MockitoDriver {
 public static final class Service { public String value() { return "real"; } }
 public static void main(String[] args) {
  System.out.println("allowAttachSelf=" + System.getProperty("jdk.attach.allowAttachSelf"));
  Service service = mock(Service.class); when(service.value()).thenReturn("mocked");
  System.out.println("mock=" + service.value()); verify(service).value();
  System.out.println("verified=true");
 }
}''',
        "CoverageTarget": '''public class CoverageTarget {
 static int compute(int value) { return value > 0 ? value * 7 : -1; }
 public static void main(String[] args) { System.out.println("coverage-result=" + compute(6)); }
}''',
        "CoverageAudit": '''import java.io.File;
import org.jacoco.core.tools.ExecFileLoader;
import org.jacoco.core.data.ExecutionData;
public class CoverageAudit {
 public static void main(String[] args) throws Exception {
  ExecFileLoader file = new ExecFileLoader(); file.load(new File(args[0]));
  ExecutionData found = null;
  for (ExecutionData row : file.getExecutionDataStore().getContents())
   if (row.getName().equals("CoverageTarget")) found = row;
  if (found == null) throw new AssertionError("no CoverageTarget execution data");
  int hits = 0; for (boolean probe : found.getProbes()) if (probe) hits++;
  if (hits == 0) throw new AssertionError("no executed coverage probes");
  System.out.println("coverage-probes=" + hits + "/" + found.getProbes().length);
 }
}''',
        "LockReopen": '''import java.io.*;
import java.nio.channels.*;
import java.nio.file.*;
public class LockReopen {
 public static void main(String[] args) throws Exception {
  Path path = Path.of(args[0]); Files.writeString(path, "start");
  for (int i = 0; i < 3; i++) {
   FileLock lock;
   try (FileOutputStream output = new FileOutputStream(path.toFile(), true)) {
    lock = output.getChannel().lock();
    output.getChannel().position(0); output.write('!');
    System.out.println("lock-valid=" + lock.isValid());
   }
   System.out.println("after-close-valid=" + lock.isValid());
  }
  System.out.println("append=" + Files.readString(path));
  Path positional = Path.of(args[0] + "-positional");
  Files.writeString(positional, "abc");
  try (FileOutputStream output = new FileOutputStream(positional.toFile(), true)) {
   output.getChannel().write(java.nio.ByteBuffer.wrap(new byte[]{88}), 0);
   output.write(89);
  }
  System.out.println("append-positional=" + Files.readString(positional));
 }
}''',
        "ResourceAgent": '''import java.lang.instrument.Instrumentation;
public class ResourceAgent {
 public static void premain(String args, Instrumentation instrumentation) throws Exception {
  try (java.io.InputStream input = ClassLoader.getSystemClassLoader()
    .getResourceAsStream("ResourceAgent.class")) {
   if (input == null || input.readAllBytes().length == 0)
    throw new AssertionError("agent JAR resource absent from system search");
  }
  if (System.getProperty("java.class.path").contains("resource-agent.jar"))
   throw new AssertionError("agent append changed java.class.path");
  System.out.println("agent-resource=present");
 }
}''',
    }
    for name, source in sources.items():
        (work / (name + ".java")).write_text(source, encoding="utf-8")
    compile_cp = mock_cp + os.pathsep + str(args.jacoco_core.resolve())
    subprocess.run([str(javac), "-cp", compile_cp, "-d", str(classes),
                    *(str(work / (name + ".java")) for name in sources if name != "ResourceAgent")], check=True)
    subprocess.run([str(javac), "-d", str(agent_classes), str(work / "ResourceAgent.java")], check=True)
    resource_agent = work / "resource-agent.jar"
    with zipfile.ZipFile(resource_agent, "w") as jar:
        jar.writestr("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\nPremain-Class: ResourceAgent\n\n")
        jar.write(agent_classes / "ResourceAgent.class", "ResourceAgent.class")

    def run(label, launcher, flags, cp, cls, tail=()):
        result = subprocess.run([str(launcher), *flags, "-cp", cp, cls, *map(str, tail)],
                                cwd=work, capture_output=True, timeout=240)
        (work / (label + ".out")).write_bytes(result.stdout)
        (work / (label + ".err")).write_bytes(result.stderr)
        if result.returncode:
            raise RuntimeError(f"{label}: exit {result.returncode}; see {work}")
        return result.stdout.decode().replace("\r\n", "\n")

    targets = [("hotspot", java, ())]
    for mode in ("--jdk-only", "--compatible"):
        targets.extend([(mode[2:] + "-jit", cvm, (mode,)),
                        (mode[2:] + "-interp", cvm, (mode, "--nojit"))])
    expected = {}
    checks = 0
    for label, launcher, flags in targets:
        data = work / (label + ".exec")
        data.unlink(missing_ok=True)
        rows = {
            "mockito": run(label + "-mockito", launcher, flags, mock_cp, "MockitoDriver"),
            "locks": run(label + "-locks", launcher, flags, str(classes), "LockReopen", (work / (label + ".txt"),)),
            "agent-resource": run(label + "-agent-resource", launcher,
                                  (*flags, "-javaagent:" + str(resource_agent)), str(classes), "CoverageTarget"),
            "jacoco": run(label + "-jacoco", launcher,
                          (*flags, "-javaagent:" + str(args.jacoco_agent.resolve()) + "=destfile=" + str(data)),
                          str(classes), "CoverageTarget") + run(label + "-coverage-audit", java, (),
                          str(classes) + os.pathsep + str(args.jacoco_core.resolve()), "CoverageAudit", (data,)),
        }
        if label == "hotspot":
            expected = rows
            continue
        for name, actual in rows.items():
            if actual != expected[name]:
                raise RuntimeError(f"{label}-{name}: differs from HotSpot: {actual!r} != {expected[name]!r}")
            checks += 1
            print("PASS " + label + "-" + name, flush=True)
    print(f"I56-AGENTS-OK {checks} fresh-process oracle comparisons", flush=True)


if __name__ == "__main__":
    main()
