import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.util.concurrent.CompletableFuture;

/** Does sendAsync return BEFORE the request finishes, and does .get() still work? */
public class AsyncSendProbe {
    public static void main(String[] a) throws Exception {
        HttpClient c = HttpClient.newHttpClient();
        HttpRequest r = HttpRequest.newBuilder(URI.create("http://127.0.0.1:1/")).build();

        CompletableFuture<HttpResponse<String>> f =
                c.sendAsync(r, HttpResponse.BodyHandlers.ofString());
        System.out.println("class            = " + f.getClass().getName());
        System.out.println("returnedIncomplete = " + !f.isDone());

        // The Spring shape: sendAsync(...).get() must still deliver.
        String outcome;
        try { f.get(); outcome = "returned"; }
        catch (Throwable t) {
            Throwable cause = t.getCause();
            outcome = t.getClass().getSimpleName()
                    + " cause=" + (cause == null ? "null" : cause.getClass().getName());
        }
        System.out.println("get()            = " + outcome);
        System.out.println("doneAfterGet     = " + f.isDone());
        System.out.println("RESULT done");
    }
}
