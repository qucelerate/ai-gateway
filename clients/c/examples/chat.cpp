// A chat call and a streamed one through the C++ wrapper.
//
//   GATEWAY_URL=https://gateway.example.com GATEWAY_API_KEY=gw_... ./gateway_chat [model] [prompt]
#include <gateway/gateway.hpp>

#include <cstdlib>
#include <iostream>

int main(int argc, char **argv) {
    const char *base_url = std::getenv("GATEWAY_URL");
    const char *api_key = std::getenv("GATEWAY_API_KEY");
    if (!base_url || !api_key) {
        std::cerr << "set GATEWAY_URL and GATEWAY_API_KEY\n";
        return 2;
    }
    std::string model = argc > 1 ? argv[1] : "chat-default";
    std::string prompt = argc > 2 ? argv[2] : "Reply with the single word: pong";
    try {
        gateway::Client client({base_url, api_key});
        gateway::ChatRequest request{model, {gateway::Message::user(prompt)}};
        request.max_tokens = 16;

        auto response = client.chat(request);
        std::cout << "chat: " << response.content() << " (request " << response.request_id << ")\n";

        auto stream = client.chat_stream(request);
        std::cout << "stream: ";
        while (auto event = stream.next()) {
            if (auto *delta = std::get_if<gateway::ChoiceDelta>(&*event); delta && delta->content_delta) {
                std::cout << *delta->content_delta << std::flush;
            } else if (auto *done = std::get_if<gateway::ResponseCompleted>(&*event); done && done->usage &&
                                                                                      done->usage->output_tokens) {
                std::cout << " [" << *done->usage->output_tokens << " output tokens]";
            }
        }
        std::cout << " (request " << stream.request_id() << ")\n";
    } catch (const gateway::Error &error) {
        std::cerr << gw_status_name(error.status()) << ": " << error.what();
        if (!error.code().empty()) {
            std::cerr << " [" << error.code() << "]";
        }
        if (!error.request_id().empty()) {
            std::cerr << " (request " << error.request_id() << ")";
        }
        std::cerr << "\n";
        return 1;
    }
    return 0;
}
