import com.hazelcast.instance.BuildInfo;
import com.hazelcast.instance.BuildInfoProvider;
import com.hazelcast.internal.util.UuidUtil;
import com.hazelcast.config.Config;
import com.hazelcast.config.NetworkConfig;
import java.util.UUID;
public class HazelcastProbe {
    public static void main(String[] args) {
        BuildInfo bi = BuildInfoProvider.getBuildInfo();
        System.out.println("Hazelcast version: " + bi.getVersion());
        System.out.println("Build: " + bi.getBuild());

        // Verify Config object initialization (full graph: NetworkConfig,
        // SerializationConfig, ManagementCenterConfig, etc.).
        Config cfg = new Config();
        cfg.setClusterName("probe-cluster");
        if (!"probe-cluster".equals(cfg.getClusterName())) {
            System.out.println("FAIL: cluster name not set"); System.exit(1);
        }
        NetworkConfig nc = cfg.getNetworkConfig();
        if (nc == null) { System.out.println("FAIL: NetworkConfig null"); System.exit(1); }
        System.out.println("NetworkConfig port: " + nc.getPort());

        // Round-trip a UUID via Hazelcast's UuidUtil.
        UUID u1 = UuidUtil.newSecureUUID();
        long m = u1.getMostSignificantBits();
        long l = u1.getLeastSignificantBits();
        UUID u2 = new UUID(m, l);
        if (!u1.equals(u2)) { System.out.println("FAIL: UUID mismatch"); System.exit(1); }
        System.out.println("UUID OK: " + u1);

        System.out.println("OK");
        System.exit(0);
    }
}
