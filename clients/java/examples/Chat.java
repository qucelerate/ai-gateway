import com.qucelerate.aigateway.ChatRequest;
import com.qucelerate.aigateway.ChatResponse;
import com.qucelerate.aigateway.ChatStream;
import com.qucelerate.aigateway.GatewayClient;
import com.qucelerate.aigateway.Message;
import com.qucelerate.aigateway.StreamEvent;
import java.util.List;

/**
 * Sends one chat call and one streamed call and prints the answers. See the
 * README for how to run it.
 */
public class Chat {
    public static void main(String[] args) {
        String model = args.length > 0 ? args[0] : "chat-default";
        String prompt = args.length > 1 ? args[1] : "Say hello.";
        GatewayClient client = GatewayClient.builder(System.getenv("GATEWAY_URL"))
                .apiKey(System.getenv("GATEWAY_API_KEY"))
                .build();
        ChatRequest request = new ChatRequest(model, List.of(Message.user(prompt))).withMaxTokens(256);

        ChatResponse response = client.chat(request);
        System.out.println("chat: " + response.content());
        if (response.usage() != null) {
            System.out.println("usage: " + response.usage());
        }

        try (ChatStream stream = client.chatStream(request)) {
            System.out.print("stream: ");
            for (StreamEvent event : stream) {
                if (event instanceof StreamEvent.ChoiceDelta delta && delta.contentDelta() != null) {
                    System.out.print(delta.contentDelta());
                } else if (event instanceof StreamEvent.ResponseCompleted) {
                    System.out.println();
                }
            }
            System.out.println("request id: " + stream.requestId());
        }
    }
}
