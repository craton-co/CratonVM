import com.rabbitmq.client.ConnectionFactory;
import com.rabbitmq.client.AMQP;
public class RabbitMQProbe {
    public static void main(String[] args) {
        try {
            ConnectionFactory f = new ConnectionFactory();
            f.setHost("localhost");
            f.setPort(5672);
            f.setUsername("guest");
            System.out.println("CF: host=" + f.getHost() + " port=" + f.getPort());
            // Build a BasicProperties — exercises the AMQP command stack.
            AMQP.BasicProperties props = new AMQP.BasicProperties.Builder()
                .contentType("text/plain")
                .deliveryMode(2)
                .build();
            System.out.println("Props: contentType=" + props.getContentType() + " deliveryMode=" + props.getDeliveryMode());
            System.out.println("OK");
        } catch (Throwable t) { t.printStackTrace(); System.exit(1); }
        System.exit(0);
    }
}
