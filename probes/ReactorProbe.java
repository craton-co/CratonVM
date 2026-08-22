import reactor.core.publisher.Flux;
import reactor.core.publisher.Mono;
import org.springframework.core.io.buffer.*;
import org.springframework.core.ResolvableType;
import org.springframework.http.MediaType;
import org.springframework.http.codec.json.JacksonJsonEncoder;
import org.springframework.http.codec.json.JacksonJsonDecoder;
import org.springframework.web.testfixture.xml.Pojo;

import java.util.*;

/**
 * Socket-free reactive-path throughput probe.
 *
 * `ExchangeProbe` reproduces the real WebClient gap but needs a MockWebServer per
 * iteration, and on a loaded shared host that turns into `ClosedChannelException`
 * and blown `block()` deadlines — the run fails rather than measures. These arms
 * exercise the same machinery (Reactor operator assembly + subscription, the
 * WebFlux codecs, DataBuffer handling) with no sockets at all, so the numbers are
 * deterministic and the only variable is the VM.
 *
 * Each arm's loop lives in its own method so invocation-count tier-up applies
 * (a loop in main is measured interpreted or as OSR, never as an ordinary
 * compiled method).
 */
public class ReactorProbe {
    static Object sink;
    static long lsink;

    static final DefaultDataBufferFactory BUFFERS = DefaultDataBufferFactory.sharedInstance;
    static final JacksonJsonEncoder ENCODER = new JacksonJsonEncoder();
    static final JacksonJsonDecoder DECODER = new JacksonJsonDecoder();
    static final ResolvableType POJO_TYPE = ResolvableType.forClass(Pojo.class);

    // --- 1. operator assembly only: build a pipeline, never subscribe -------
    static void assembleOnly(int n) {
        for (int i = 0; i < n; i++) {
            sink = Flux.range(0, 8).map(x -> x + 1).filter(x -> x > 2)
                    .flatMap(x -> Mono.just(x * 2)).collectList();
        }
    }

    // --- 2. assemble + subscribe + block: the full operator path ------------
    static void assembleAndRun(int n) {
        for (int i = 0; i < n; i++) {
            lsink += Flux.range(0, 8).map(x -> x + 1).filter(x -> x > 2)
                    .flatMap(x -> Mono.just(x * 2)).collectList().block().size();
        }
    }

    // --- 3. Mono chain: the shape a WebClient exchange assembles ------------
    static void monoChain(int n) {
        for (int i = 0; i < n; i++) {
            lsink += Mono.just("Hello Spring!")
                    .map(String::length)
                    .flatMap(x -> Mono.just(x + 1))
                    .map(x -> x * 2)
                    .defaultIfEmpty(0)
                    .block();
        }
    }

    // --- 4. encode a POJO to DataBuffers through the WebFlux codec ----------
    static void encodePojo(int n) {
        Pojo p = new Pojo("foofoo", "barbar");
        for (int i = 0; i < n; i++) {
            List<DataBuffer> out = ENCODER.encode(Mono.just(p), BUFFERS, POJO_TYPE,
                    MediaType.APPLICATION_JSON, Collections.emptyMap()).collectList().block();
            for (DataBuffer b : out) { lsink += b.readableByteCount(); DataBufferUtils.release(b); }
        }
    }

    // --- 5. decode DataBuffers back to a POJO ------------------------------
    static void decodePojo(int n) {
        byte[] json = "{\"foo\":\"foofoo\",\"bar\":\"barbar\"}".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        for (int i = 0; i < n; i++) {
            DataBuffer b = BUFFERS.wrap(json);
            Object o = DECODER.decodeToMono(Mono.just(b), POJO_TYPE,
                    MediaType.APPLICATION_JSON, Collections.emptyMap()).block();
            if (o == null) throw new IllegalStateException("decode returned null");
            sink = o;
        }
    }

    // --- 6. control: a plain loop with no reactive types at all -------------
    static void control(int n) {
        for (int i = 0; i < n; i++) lsink += i ^ (i << 3);
    }

    interface Arm { void run(int n); }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 2;
        String[] names = {"control", "assemble only", "assemble+run", "mono chain",
                          "encode pojo", "decode pojo"};
        Arm[] arms = {ReactorProbe::control, ReactorProbe::assembleOnly,
                      ReactorProbe::assembleAndRun, ReactorProbe::monoChain,
                      ReactorProbe::encodePojo, ReactorProbe::decodePojo};
        for (int pass = 0; pass < passes; pass++) {
            System.out.println("--- pass " + pass + " ---");
            for (int k = 0; k < names.length; k++) {
                arms[k].run(Math.min(n, 2000));                 // warm
                long t0 = System.nanoTime();
                arms[k].run(n);
                long d = System.nanoTime() - t0;
                System.out.printf("%-16s %10.0f ns/op%n", names[k], (double) d / n);
                System.out.flush();
            }
        }
        System.out.println("PROBE-DONE " + lsink + " " + (sink != null));
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
