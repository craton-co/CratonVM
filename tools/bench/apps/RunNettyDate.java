import io.netty.handler.codec.DateFormatterBenchmark;

public class RunNettyDate {
    public static void main(String[] args) throws Exception {
        DateFormatterBenchmark bench = new DateFormatterBenchmark();
        long checksum = 0;
        for (int i = 0; i < 500_000; i++) {
            checksum += bench.parseHttpHeaderDateFormatter().getTime();
            checksum += bench.parseHttpHeaderDateFormat().getTime();
            checksum += bench.formatHttpHeaderDateFormatter().length();
            checksum += bench.formatHttpHeaderDateFormat().length();
        }
        System.out.println("CHECKSUM=" + checksum);
    }
}
