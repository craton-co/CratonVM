import org.apache.activemq.ActiveMQConnectionFactory;
import org.apache.activemq.command.ActiveMQQueue;
import org.apache.activemq.command.ActiveMQTextMessage;
import org.apache.activemq.command.ConsumerId;
import org.apache.activemq.command.MessageId;
import org.apache.activemq.command.ProducerId;
import org.apache.activemq.command.SessionId;
import org.apache.activemq.command.ConnectionId;
import org.apache.activemq.openwire.OpenWireFormat;
import org.apache.activemq.openwire.OpenWireFormatFactory;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.nio.ByteBuffer;

public class ActiveMQProbe {
    public static void main(String[] args) throws Exception {
        // 1. Verify ConnectionFactory parses a VM URI.
        ActiveMQConnectionFactory cf = new ActiveMQConnectionFactory("vm://probe-broker?broker.persistent=false&broker.useJmx=false");
        System.out.println("ConnectionFactory: brokerURL=" + cf.getBrokerURL());

        // 2. Build OpenWire command objects (Queue + Message) without
        //    needing a live broker. This exercises the OpenWire id
        //    factories + ActiveMQDestination serialization.
        ActiveMQQueue q = new ActiveMQQueue("probe.queue");
        System.out.println("Queue: name=" + q.getQueueName() + " physical=" + q.getPhysicalName());

        ActiveMQTextMessage msg = new ActiveMQTextMessage();
        ConnectionId connId = new ConnectionId("probe-conn:1");
        SessionId sessId = new SessionId(connId, 1L);
        ProducerId prodId = new ProducerId(sessId, 1L);
        MessageId mid = new MessageId(prodId, 42L);
        msg.setMessageId(mid);
        msg.setDestination(q);
        msg.setText("hello from probe");
        System.out.println("Built message: id=" + msg.getMessageId() + " text=" + msg.getText());

        // 3. Marshal + unmarshal via OpenWire — exercises ActiveMQ's
        //    network-protocol code path without touching the broker.
        OpenWireFormat format = (OpenWireFormat) new OpenWireFormatFactory().createWireFormat();
        org.apache.activemq.util.ByteSequence buf = format.marshal(msg);
        System.out.println("Marshalled bytes: " + buf.getLength());
        ActiveMQTextMessage round = (ActiveMQTextMessage) format.unmarshal(new java.io.DataInputStream(new java.io.ByteArrayInputStream(buf.getData(), buf.getOffset(), buf.getLength())));
        if (round == null || !"hello from probe".equals(round.getText())) {
            System.out.println("FAIL: round-trip lost text: " + (round == null ? "null" : round.getText()));
            System.exit(1);
        }
        System.out.println("Round-trip OK: " + round.getText());

        System.out.println("OK");
        System.exit(0);
    }
}
