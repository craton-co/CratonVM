import org.apache.kafka.common.utils.AppInfoParser;
import org.apache.kafka.common.serialization.StringSerializer;
import org.apache.kafka.common.serialization.StringDeserializer;
import org.apache.kafka.common.config.ConfigDef;
import java.util.Properties;
public class KafkaProbe {
    public static void main(String[] args) {
        // Read Kafka version.
        System.out.println("Kafka version: " + AppInfoParser.getVersion());
        System.out.println("Kafka commit: " + AppInfoParser.getCommitId());

        // Exercise the serializer round-trip — used by every Kafka producer/consumer.
        StringSerializer ser = new StringSerializer();
        java.util.Map<String, Object> props = new java.util.HashMap<>();
        ser.configure(props, false);
        byte[] bytes = ser.serialize("probe-topic", "hello kafka");
        if (bytes == null || bytes.length == 0) {
            System.out.println("FAIL: serializer produced empty bytes");
            System.exit(1);
        }
        StringDeserializer de = new StringDeserializer();
        de.configure(props, false);
        String round = de.deserialize("probe-topic", bytes);
        if (!"hello kafka".equals(round)) {
            System.out.println("FAIL: serializer round-trip lost text: '" + round + "'");
            System.exit(1);
        }
        System.out.println("Serializer round-trip OK: " + round);

        // Verify the producer ConfigDef enumerates the canonical settings.
        ConfigDef def = new ConfigDef();
        def.define("bootstrap.servers", ConfigDef.Type.LIST, ConfigDef.Importance.HIGH, "doc");
        def.define("acks", ConfigDef.Type.STRING, "1", ConfigDef.Importance.MEDIUM, "doc");
        if (def.configKeys().size() != 2) {
            System.out.println("FAIL: ConfigDef registered " + def.configKeys().size() + " keys, expected 2");
            System.exit(1);
        }
        System.out.println("ConfigDef OK: " + def.configKeys().keySet());

        System.out.println("OK");
        System.exit(0);
    }
}
