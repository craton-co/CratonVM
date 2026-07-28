import org.springframework.aot.hint.*;
import org.springframework.aot.nativex.FileNativeConfigurationWriter;
import java.nio.file.*;
public class NativeCfgProbe {
  public static void main(String[] a) throws Exception {
    Path tmp = Files.createTempDirectory("ncfg");
    FileNativeConfigurationWriter g = new FileNativeConfigurationWriter(tmp);
    RuntimeHints hints = new RuntimeHints();
    hints.reflection().registerType(String.class, b -> b.onReachableType(Integer.class)
        .withMembers(MemberCategory.ACCESS_PUBLIC_FIELDS));
    g.write(hints);
    Path f = tmp.resolve("META-INF").resolve("native-image").resolve("reachability-metadata.json");
    System.out.println("--- " + f + " ---");
    System.out.println(Files.readString(f));
  }
}
