// Installed-library smoke test (C++): gateway.hpp compiles against the
// installed headers, and a call throws the wrapper's transport error.
#include <gateway/gateway.hpp>

#include <iostream>

int main() {
    gateway::Client client({"http://127.0.0.1:1", "gw_smoke"});
    try {
        client.chat({"smoke", {gateway::Message::user("hi")}});
    } catch (const gateway::TransportError &) {
        std::cout << "ok: c++ wrapper " << gw_version() << " installs and runs\n";
        return 0;
    } catch (const std::exception &other) {
        std::cerr << "expected a transport error, got " << other.what() << "\n";
        return 1;
    }
    std::cerr << "expected a transport error, the call succeeded\n";
    return 1;
}
