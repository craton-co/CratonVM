/*
 * Interpreter round i4, wave 85, lane L3: RuntimeMXBean.getInputArguments().
 *
 * sun.management.RuntimeImpl.getInputArguments() is VMManagementImpl
 * .getVmArguments(), whose native getVmArguments0 is HotSpot's
 * jmm_GetInputArgumentArray: Arguments::jvm_args_array(), every VM option the
 * launcher passed except -Djava.class.path, -Dsun.java.command and
 * -Dsun.java.launcher*, in order. CratonVM's native answered an empty array
 * in both modes; it now answers VmConfig::jvm_args (recorded by the launcher
 * since wave 84). Launcher-only options (-cp, this launcher's --jdk-only /
 * --compatible) are not VM options.
 *
 * Run (both modes must print the HotSpot lines; the options matter):
 *   javac -d out L3W85InputArguments.java
 *   cratonvm --jdk-only   -Xmx64m -Dfoo=bar -cp out L3W85InputArguments
 *   cratonvm --compatible -Xmx64m -Dfoo=bar -cp out L3W85InputArguments
 *   java                  -Xmx64m -Dfoo=bar -cp out L3W85InputArguments   (HotSpot 25)
 *
 * Expected HotSpot 25 output:
 *   count: 2
 *   arg: -Xmx64m
 *   arg: -Dfoo=bar
 *   same: true
 */
import java.lang.management.ManagementFactory;
import java.util.List;

public class L3W85InputArguments {
    public static void main(String[] args) {
        List<String> in = ManagementFactory.getRuntimeMXBean().getInputArguments();
        System.out.println("count: " + in.size());
        for (String a : in) {
            System.out.println("arg: " + a);
        }
        // The list is cached by VMManagementImpl: a second call answers an
        // equal list.
        System.out.println("same: " + in.equals(ManagementFactory.getRuntimeMXBean().getInputArguments()));
    }
}
