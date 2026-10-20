#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# End-to-end measurement of CratonVM's attach mechanism on Linux, against
# HotSpot (interpreter round i2 wave 59, lane L1). It needs a Linux host: the
# attach listener is a Unix-domain socket (`/tmp/.java_pid<pid>`).
#
# What it runs, each against a HotSpot target and a CratonVM target (both
# modes), with the wave-58/59 switches off, each alone, and all on:
#
#   A. an external HotSpot client: `VirtualMachine.attach(<target pid>)`,
#      `loadAgent` with a good JAR, a JAR without `Agent-Class`, a JAR whose
#      `agentmain` throws, a missing JAR; `loadAgentPath` of a missing
#      library; `getSystemProperties` (a value holding `\n`, `\`, `=` and a
#      non-ASCII char); `getAgentProperties`;
#   B. `jcmd <pid> VM.version` and `jcmd <pid> JVMTI.agent_load <good jar>`;
#   C. ByteBuddy-style self-attach through a child `java` with
#      `-Djdk.attach.allowAttachSelf=false` (the probe
#      `tools/probes/interp/L1/L1W59AttachListenerOps.java` does exactly what
#      `ByteBuddyAgent.install()` does without the property: it spawns
#      `<java.home>/bin/java`, which attaches back); and, when BB_AGENT_JAR
#      names a byte-buddy-agent jar, `ByteBuddyAgent.install()` itself;
#   D. `-XX:+DisableAttachMechanism`: `jcmd <pid> VM.version` must fail and,
#      with `CRATONVM_ATTACH_LISTENER_LAZY=1`, the target must survive the
#      client's `SIGQUIT` (`alive=yes`).
#
# Usage:
#   CVM=/path/to/cratonvm JAVA_HOME=/path/to/jdk25 \
#     [BB_AGENT_JAR=/path/to/byte-buddy-agent-1.17.7.jar] \
#     tools/scripts/attach-end-to-end.sh [workdir]
#
# Compare the CratonVM rows with the HotSpot rows; each block prints
# `== <label>` then the client's lines.

set -u
CVM=${CVM:?set CVM to the cratonvm binary}
JAVA_HOME=${JAVA_HOME:?set JAVA_HOME to a HotSpot 25 JDK}
JAVA="$JAVA_HOME/bin/java"
JAVAC="$JAVA_HOME/bin/javac"
JCMD="$JAVA_HOME/bin/jcmd"
WORK=${1:-/tmp/cvm-attach-e2e}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
rm -rf "$WORK" && mkdir -p "$WORK/cls" && cd "$WORK" || exit 1

cat > Target.java <<'EOF'
import java.io.*;
import java.lang.instrument.Instrumentation;
import java.util.jar.*;
public class Target {
    public static class Agent {
        public static void agentmain(String a, Instrumentation i) { System.out.println("agentmain args=" + a + " inst=" + (i != null)); }
    }
    public static class Thrower {
        public static void agentmain(String a, Instrumentation i) { throw new IllegalStateException("boom"); }
    }
    static void jar(String path, String agentClass) throws Exception {
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        if (agentClass != null) m.getMainAttributes().put(new Attributes.Name("Agent-Class"), agentClass);
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(path), m)) { }
    }
    public static void main(String[] a) throws Exception {
        jar("good.jar", "Target$Agent"); jar("plain.jar", null); jar("thrower.jar", "Target$Thrower");
        System.setProperty("e2e.custom", "a=b c\né\\ #x");
        try (FileWriter w = new FileWriter("target.pid")) { w.write(Long.toString(ProcessHandle.current().pid())); }
        System.out.println("ready");
        Thread.sleep(Long.parseLong(a.length > 0 ? a[0] : "30000"));
    }
}
EOF
cat > Client.java <<'EOF'
import com.sun.tools.attach.VirtualMachine;
import java.nio.file.*;
import java.util.*;
public class Client {
    static String vis(String s) { return s == null ? "null" : s.replace("\\", "\\\\").replace("\n", "\\n"); }
    public static void main(String[] a) throws Exception {
        String pid = Files.readString(Path.of("target.pid")).trim();
        String dir = Path.of("").toAbsolutePath().toString();
        VirtualMachine vm;
        try { vm = VirtualMachine.attach(pid); System.out.println("attach ok"); }
        catch (Throwable t) { System.out.println("attach: " + t.getClass().getName() + " [" + t.getMessage() + "]"); return; }
        for (String[] r : new String[][] {{"good", "good.jar", null}, {"good opts", "good.jar", "k=v"}, {"no Agent-Class", "plain.jar", null}, {"throws", "thrower.jar", null}, {"missing", "absent.jar", null}}) {
            try { vm.loadAgent(dir + "/" + r[1], r[2]); System.out.println(r[0] + ": loaded"); }
            catch (Throwable t) { System.out.println(r[0] + ": " + t.getClass().getName() + " [" + t.getMessage() + "]"); }
        }
        try { vm.loadAgentPath(dir + "/absent.so", null); System.out.println("agentpath: loaded"); }
        catch (Throwable t) { System.out.println("agentpath: " + t.getClass().getName() + " [" + t.getMessage() + "]"); }
        try { Properties p = vm.getSystemProperties(); System.out.println("props custom=[" + vis(p.getProperty("e2e.custom")) + "] count=" + (p.size() > 20)); }
        catch (Throwable t) { System.out.println("props: " + t.getClass().getName() + " [" + t.getMessage() + "]"); }
        try { System.out.println("agentProps keys=" + new TreeSet<>(vm.getAgentProperties().stringPropertyNames())); }
        catch (Throwable t) { System.out.println("agentProps: " + t.getClass().getName() + " [" + t.getMessage() + "]"); }
        vm.detach();
    }
}
EOF
cat > BBDriver.java <<'EOF'
public class BBDriver {
    public static void main(String[] a) {
        System.out.println("allowAttachSelf=" + System.getProperty("jdk.attach.allowAttachSelf"));
        try { System.out.println("install ok inst=" + (net.bytebuddy.agent.ByteBuddyAgent.install() != null)); }
        catch (Throwable t) { System.out.println("install failed: " + t.getClass().getName() + " [" + t.getMessage() + "]"); }
    }
}
EOF
"$JAVAC" -d cls Target.java Client.java || exit 1
"$JAVAC" -d cls "$REPO/tools/probes/interp/L1/L1W59AttachListenerOps.java" \
    "$REPO/tools/probes/interp/L1/L1W59DisableAttachMechanism.java" || exit 1
if [ -n "${BB_AGENT_JAR:-}" ]; then "$JAVAC" -d cls -cp "$BB_AGENT_JAR" BBDriver.java || exit 1; fi

ALL_ON="CRATONVM_ATTACH_LISTENER_LOAD=1 CRATONVM_ATTACH_LISTENER_PROPS=1 CRATONVM_ATTACH_LISTENER_LAZY=1 CRATONVM_ATTACH_SELF_UNSET=1"

# run_target <label> <env words> <launcher words...>: start Target, run the
# external client and jcmd against it, then stop it.
run_target() {
    local label=$1 envw=$2; shift 2
    echo "== $label"
    rm -f target.pid
    env $envw "$@" -cp cls Target 40000 > target.out 2> target.err &
    local tpid=$!
    for _ in $(seq 1 100); do [ -s target.pid ] && break; sleep 0.2; done
    local pid; pid=$(cat target.pid 2>/dev/null)
    echo "socket before attach: $([ -e /tmp/.java_pid$pid ] && echo true || echo false)"
    "$JAVA" -cp cls Client
    echo "jcmd VM.version: $("$JCMD" "$pid" VM.version 2>&1 | sed -n 2p)"
    echo "jcmd JVMTI.agent_load: $("$JCMD" "$pid" JVMTI.agent_load "$WORK/good.jar" 2>&1 | tail -n +2 | tr '\n' ' ')"
    echo "target saw: $(grep agentmain target.out | tr '\n' ' ')"
    kill "$tpid" 2>/dev/null; wait "$tpid" 2>/dev/null
}

# run_disabled <label> <env words> <launcher words...>: a target with
# -XX:+DisableAttachMechanism must refuse jcmd and survive it.
run_disabled() {
    local label=$1 envw=$2; shift 2
    echo "== $label"
    rm -f target.pid
    env $envw "$@" -XX:+DisableAttachMechanism -cp cls Target 40000 > target.out 2> target.err &
    local tpid=$!
    for _ in $(seq 1 100); do [ -s target.pid ] && break; sleep 0.2; done
    local pid; pid=$(cat target.pid 2>/dev/null)
    echo "jcmd VM.version: $(timeout 30 "$JCMD" "$pid" VM.version 2>&1 | tail -n 1)"
    echo "alive=$(kill -0 "$tpid" 2>/dev/null && echo yes || echo no)"
    kill "$tpid" 2>/dev/null; wait "$tpid" 2>/dev/null
}

run_target "hotspot" "" "$JAVA"
run_disabled "hotspot disabled" "" "$JAVA"
for mode in --jdk-only --compatible; do
    run_target "cratonvm $mode switches off" "" "$CVM" $mode
    run_target "cratonvm $mode LOAD" "CRATONVM_ATTACH_LISTENER_LOAD=1" "$CVM" $mode
    run_target "cratonvm $mode LOAD+PROPS" "CRATONVM_ATTACH_LISTENER_LOAD=1 CRATONVM_ATTACH_LISTENER_PROPS=1" "$CVM" $mode
    run_target "cratonvm $mode all on" "$ALL_ON" "$CVM" $mode
    run_disabled "cratonvm $mode disabled, lazy off" "" "$CVM" $mode
    run_disabled "cratonvm $mode disabled, all on" "$ALL_ON" "$CVM" $mode
done

# C. ByteBuddy-style self-attach through a child `java`.
echo "== probe L1W59AttachListenerOps (hotspot)"
"$JAVA" -cp cls L1W59AttachListenerOps
for mode in --jdk-only --compatible; do
    echo "== probe L1W59AttachListenerOps (cratonvm $mode, all on)"
    env $ALL_ON "$CVM" $mode -cp cls L1W59AttachListenerOps
    echo "== probe L1W59AttachListenerOps (cratonvm $mode, all on, -Djdk.attach.allowAttachSelf=false)"
    env $ALL_ON "$CVM" $mode -Djdk.attach.allowAttachSelf=false -cp cls L1W59AttachListenerOps
done
if [ -n "${BB_AGENT_JAR:-}" ]; then
    echo "== ByteBuddyAgent.install (hotspot, default)"
    "$JAVA" -cp "cls:$BB_AGENT_JAR" BBDriver
    for mode in --jdk-only --compatible; do
        echo "== ByteBuddyAgent.install (cratonvm $mode, switches off)"
        "$CVM" $mode -cp "cls:$BB_AGENT_JAR" BBDriver
        echo "== ByteBuddyAgent.install (cratonvm $mode, all on)"
        env $ALL_ON "$CVM" $mode -cp "cls:$BB_AGENT_JAR" BBDriver
        echo "== ByteBuddyAgent.install (cratonvm $mode, all on, -Djdk.attach.allowAttachSelf=false)"
        env $ALL_ON "$CVM" $mode -Djdk.attach.allowAttachSelf=false -cp "cls:$BB_AGENT_JAR" BBDriver
    done
fi
