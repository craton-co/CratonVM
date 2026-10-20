// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import com.sun.tools.attach.VirtualMachine;
import java.nio.file.Files;
import java.nio.file.Path;
public class I59Serviceability {
    public static void main(String[] args) throws Exception {
        if (args.length > 0 && args[0].equals("kill")) {
            System.out.println(ProcessHandle.of(Long.parseLong(args[1])).orElseThrow().destroyForcibly());
            return;
        }
        if (args.length > 0 && args[0].equals("list")) {
            for (var vm : VirtualMachine.list())
                if (vm.id().equals(args[1])) System.out.println(vm.displayName());
            return;
        }
        if (args.length > 0 && args[0].equals("attach")) {
            try { var vm = VirtualMachine.attach(args[1]); vm.detach(); System.out.println("attach ok"); }
            catch (Throwable t) { System.out.println(t.getClass().getName() + ": " + t.getMessage()); }
            return;
        }
        if (args.length > 0 && args[0].equals("register")) {
            for (String name : new String[]{"HUP", "INT", "TERM", "USR1"}) {
                try {
                    sun.misc.Signal.handle(new sun.misc.Signal(name), sun.misc.SignalHandler.SIG_IGN);
                    System.out.println(name+": accepted");
                } catch (Throwable t) {
                    System.out.println(name+": "+t.getClass().getName()+": "+t.getMessage());
                }
            }
            return;
        }
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook")));
        if (args.length > 0 && args[0].equals("handle"))
            sun.misc.Signal.handle(new sun.misc.Signal("TERM"), s -> System.out.println("handled"));
        sun.misc.SignalHandler previous = null;
        if (args.length > 0 && (args[0].equals("ignore") || args[0].equals("restore")))
            previous = sun.misc.Signal.handle(new sun.misc.Signal("TERM"), sun.misc.SignalHandler.SIG_IGN);
        if (args.length > 0 && args[0].equals("default"))
            sun.misc.Signal.handle(new sun.misc.Signal("TERM"), sun.misc.SignalHandler.SIG_DFL);
        System.out.println("ready");
        System.out.flush();
        if (args.length > 0 && args[0].equals("restore")) {
            System.in.read();
            sun.misc.Signal.handle(new sun.misc.Signal("TERM"), previous);
            System.out.println("restored");
            System.out.flush();
        }
        Thread.sleep(120_000);
    }
}
