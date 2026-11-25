// Installed-jar smoke test: the released jar resolves as a Maven dependency,
// and a call reaches its HTTP stack (a closed port: a transport error).
import com.qucelerate.aigateway.ChatRequest;
import com.qucelerate.aigateway.GatewayClient;
import com.qucelerate.aigateway.Message;
import com.qucelerate.aigateway.TransportException;
import java.util.List;

public final class Smoke {
    public static void main(String[] args) {
        GatewayClient client = GatewayClient.builder("http://127.0.0.1:1").apiKey("gw_smoke").build();
        try {
            client.chat(new ChatRequest("smoke", List.of(Message.user("hi"))));
        } catch (TransportException expected) {
            System.out.println("ok: java client installs and runs");
            return;
        } catch (RuntimeException other) {
            System.err.println("expected a transport error, got " + other);
            System.exit(1);
        }
        System.err.println("expected a transport error, the call succeeded");
        System.exit(1);
    }
}
