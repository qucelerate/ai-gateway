// Runs the shared conformance cases (clients/conformance) against the C
// library through the C++ wrapper, so both layers are covered. Each case gets
// a mock HTTP server on a loopback socket that serves its canned responses in
// order and records the requests.
//
//   conformance <path to clients/conformance>
#include <gateway/gateway.hpp>

#include "json.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstring>
#include <dirent.h>
#include <fstream>
#include <iostream>
#include <map>
#include <sstream>
#include <thread>

namespace {

// ------------------------------------------------------------------ JSON

struct Doc {
    yyjson_doc *doc = nullptr;
    explicit Doc(const std::string &text) {
        doc = yyjson_read(text.data(), text.size(), 0);
    }
    Doc(const Doc &) = delete;
    Doc &operator=(const Doc &) = delete;
    ~Doc() { yyjson_doc_free(doc); }
    yyjson_val *root() const { return yyjson_doc_get_root(doc); }
};

std::string read_file(const std::string &path) {
    std::ifstream in(path, std::ios::binary);
    std::stringstream out;
    out << in.rdbuf();
    return out.str();
}

std::string str(yyjson_val *value) {
    return yyjson_is_str(value) ? std::string(yyjson_get_str(value), yyjson_get_len(value)) : std::string();
}

std::string dump(yyjson_val *value) {
    if (!value) {
        return "<missing>";
    }
    char *text = yyjson_val_write(value, 0, nullptr);
    std::string out = text ? text : "<unwritable>";
    std::free(text);
    return out;
}

bool scalar_equal(yyjson_val *a, yyjson_val *b) {
    if (yyjson_is_num(a) && yyjson_is_num(b)) {
        return yyjson_get_num(a) == yyjson_get_num(b);
    }
    if (yyjson_get_type(a) != yyjson_get_type(b)) {
        return false;
    }
    if (yyjson_is_str(a)) {
        return str(a) == str(b);
    }
    if (yyjson_is_bool(a)) {
        return yyjson_get_bool(a) == yyjson_get_bool(b);
    }
    return yyjson_is_null(a);
}

/** Every key of `expected` is in `actual` with a matching value; arrays match by length and element. */
bool subset(yyjson_val *expected, yyjson_val *actual, bool exact, std::string &path) {
    if (!actual) {
        path += " (missing)";
        return false;
    }
    if (yyjson_is_obj(expected)) {
        if (!yyjson_is_obj(actual) || (exact && yyjson_obj_size(expected) != yyjson_obj_size(actual))) {
            return false;
        }
        size_t i, n;
        yyjson_val *key, *value;
        yyjson_obj_foreach(expected, i, n, key, value) {
            std::string at = path + "." + str(key);
            if (!subset(value, yyjson_obj_getn(actual, yyjson_get_str(key), yyjson_get_len(key)), exact, at)) {
                path = at;
                return false;
            }
        }
        return true;
    }
    if (yyjson_is_arr(expected)) {
        if (!yyjson_is_arr(actual) || yyjson_arr_size(expected) != yyjson_arr_size(actual)) {
            return false;
        }
        for (size_t i = 0; i < yyjson_arr_size(expected); i++) {
            std::string at = path + "[" + std::to_string(i) + "]";
            if (!subset(yyjson_arr_get(expected, i), yyjson_arr_get(actual, i), exact, at)) {
                path = at;
                return false;
            }
        }
        return true;
    }
    return scalar_equal(expected, actual);
}

/** Builds the client's result in wire form. */
struct Builder {
    yyjson_mut_doc *doc = yyjson_mut_doc_new(nullptr);
    Builder() = default;
    Builder(const Builder &) = delete;
    Builder &operator=(const Builder &) = delete;
    ~Builder() { yyjson_mut_doc_free(doc); }

    yyjson_mut_val *obj() { return yyjson_mut_obj(doc); }
    yyjson_mut_val *text(const std::string &value) { return yyjson_mut_strncpy(doc, value.data(), value.size()); }
    yyjson_mut_val *text(const std::optional<std::string> &value) {
        return value ? text(*value) : yyjson_mut_null(doc);
    }
    yyjson_mut_val *num(std::int64_t value) { return yyjson_mut_sint(doc, value); }
    yyjson_mut_val *num(const std::optional<std::int64_t> &value) {
        return value ? num(*value) : yyjson_mut_null(doc);
    }
    void put(yyjson_mut_val *object, const char *key, yyjson_mut_val *value) {
        yyjson_mut_obj_add(object, yyjson_mut_strcpy(doc, key), value);
    }
    yyjson_mut_val *usage(const gateway::Usage &u) {
        yyjson_mut_val *out = obj();
        put(out, "input_tokens", num(u.input_tokens));
        put(out, "output_tokens", num(u.output_tokens));
        put(out, "cached_input_tokens", num(u.cached_input_tokens));
        put(out, "reasoning_tokens", num(u.reasoning_tokens));
        put(out, "other_tokens", num(u.other_tokens));
        put(out, "quality", text(u.quality));
        put(out, "completeness", text(u.completeness));
        return out;
    }
    yyjson_mut_val *usage(const std::optional<gateway::Usage> &u) { return u ? usage(*u) : yyjson_mut_null(doc); }

    /** Serialize and re-read `value` so it can be compared with the case. */
    std::string write(yyjson_mut_val *value) {
        char *out = yyjson_mut_val_write(value, 0, nullptr);
        std::string result = out ? out : "";
        std::free(out);
        return result;
    }
};

std::string chat_result(const gateway::ChatResponse &r) {
    Builder b;
    yyjson_mut_val *root = b.obj();
    b.put(root, "request_id", b.text(r.request_id));
    b.put(root, "id", b.text(r.id));
    b.put(root, "model", b.text(r.model));
    yyjson_mut_val *choices = yyjson_mut_arr(b.doc);
    for (const auto &c : r.choices) {
        yyjson_mut_val *choice = b.obj();
        b.put(choice, "index", b.num(c.index));
        yyjson_mut_val *message = b.obj();
        b.put(message, "role", b.text(c.role));
        b.put(message, "content", b.text(c.content));
        if (!c.tool_calls.empty()) {
            yyjson_mut_val *calls = yyjson_mut_arr(b.doc);
            for (const auto &call : c.tool_calls) {
                yyjson_mut_val *item = b.obj();
                yyjson_mut_val *function = b.obj();
                b.put(item, "id", b.text(call.id));
                b.put(item, "type", b.text(std::string("function")));
                b.put(function, "name", b.text(call.name));
                b.put(function, "arguments", b.text(call.arguments));
                b.put(item, "function", function);
                yyjson_mut_arr_append(calls, item);
            }
            b.put(message, "tool_calls", calls);
        }
        b.put(choice, "message", message);
        b.put(choice, "finish_reason", b.text(c.finish_reason));
        b.put(choice, "refusal", b.text(c.refusal));
        yyjson_mut_arr_append(choices, choice);
    }
    b.put(root, "choices", choices);
    b.put(root, "usage", b.usage(r.usage));
    return b.write(root);
}

std::string embedding_result(const gateway::EmbeddingResponse &r) {
    Builder b;
    yyjson_mut_val *root = b.obj();
    b.put(root, "request_id", b.text(r.request_id));
    b.put(root, "model", b.text(r.model));
    yyjson_mut_val *data = yyjson_mut_arr(b.doc);
    for (const auto &e : r.data) {
        yyjson_mut_val *item = b.obj();
        b.put(item, "index", b.num(e.index));
        yyjson_mut_val *vector = yyjson_mut_arr(b.doc);
        for (float x : e.embedding) {
            yyjson_mut_arr_append(vector, yyjson_mut_real(b.doc, static_cast<double>(x)));
        }
        b.put(item, "embedding", vector);
        yyjson_mut_arr_append(data, item);
    }
    b.put(root, "data", data);
    b.put(root, "usage", b.usage(r.usage));
    return b.write(root);
}

std::string event_data(const gateway::Event &event) {
    Builder b;
    yyjson_mut_val *d = b.obj();
    if (auto *started = std::get_if<gateway::ResponseStarted>(&event)) {
        b.put(d, "model", b.text(started->model));
    } else if (auto *delta = std::get_if<gateway::ChoiceDelta>(&event)) {
        b.put(d, "choice_index", b.num(delta->choice_index));
        b.put(d, "content_delta", b.text(delta->content_delta));
        b.put(d, "refusal_delta", b.text(delta->refusal_delta));
    } else if (auto *tool = std::get_if<gateway::ToolCallDelta>(&event)) {
        b.put(d, "choice_index", b.num(tool->choice_index));
        b.put(d, "tool_call_index", b.num(tool->tool_call_index));
        b.put(d, "id", b.text(tool->id));
        b.put(d, "name", b.text(tool->name));
        b.put(d, "arguments_delta", b.text(tool->arguments_delta));
    } else if (auto *finished = std::get_if<gateway::ChoiceFinished>(&event)) {
        b.put(d, "choice_index", b.num(finished->choice_index));
        b.put(d, "finish_reason", b.text(finished->finish_reason));
    } else if (auto *safety = std::get_if<gateway::SafetyAnnotation>(&event)) {
        b.put(d, "choice_index", b.num(safety->choice_index));
        b.put(d, "category", b.text(safety->category));
        b.put(d, "blocked", yyjson_mut_bool(b.doc, safety->blocked));
    } else if (auto *report = std::get_if<gateway::UsageReport>(&event)) {
        d = b.usage(report->usage);
    } else if (auto *completed = std::get_if<gateway::ResponseCompleted>(&event)) {
        b.put(d, "model", b.text(completed->model));
        b.put(d, "usage", b.usage(completed->usage));
    }
    return b.write(d);
}

// ------------------------------------------------------------ mock server

struct Request {
    std::string method;
    std::string path;
    std::map<std::string, std::string> headers; // lowercase names
    std::string body;
};

std::string lower(std::string text) {
    std::transform(text.begin(), text.end(), text.begin(), [](unsigned char c) { return std::tolower(c); });
    return text;
}

class Server {
public:
    explicit Server(std::vector<std::string> responses) : responses_(std::move(responses)) {
        listener_ = socket(AF_INET, SOCK_STREAM, 0);
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        socklen_t length = sizeof address;
        if (listener_ < 0 || bind(listener_, reinterpret_cast<sockaddr *>(&address), sizeof address) != 0 ||
            listen(listener_, 8) != 0 ||
            getsockname(listener_, reinterpret_cast<sockaddr *>(&address), &length) != 0) {
            throw std::runtime_error("mock server: cannot listen");
        }
        port_ = ntohs(address.sin_port);
        thread_ = std::thread([this] { run(); });
    }
    Server(const Server &) = delete;
    Server &operator=(const Server &) = delete;
    ~Server() {
        stop_ = true;
        thread_.join();
        close(listener_);
    }

    std::string url() const { return "http://127.0.0.1:" + std::to_string(port_); }

    /** The requests received so far, once the server has answered them. */
    std::vector<Request> requests() {
        std::lock_guard<std::mutex> lock(mutex_);
        return requests_;
    }

private:
    void run() {
        size_t next = 0;
        while (!stop_) {
            pollfd p{listener_, POLLIN, 0};
            if (poll(&p, 1, 20) <= 0) {
                continue;
            }
            int fd = accept(listener_, nullptr, nullptr);
            if (fd < 0) {
                continue;
            }
            Request request;
            if (read_request(fd, request)) {
                std::string reply = next < responses_.size()
                                        ? responses_[next++]
                                        : "HTTP/1.1 599 Unexpected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                {
                    std::lock_guard<std::mutex> lock(mutex_);
                    requests_.push_back(std::move(request));
                }
                send_all(fd, reply);
            }
            close(fd);
        }
    }

    static bool read_request(int fd, Request &request) {
        std::string data;
        char chunk[4096];
        size_t end;
        while ((end = data.find("\r\n\r\n")) == std::string::npos) {
            ssize_t n = recv(fd, chunk, sizeof chunk, 0);
            if (n <= 0) {
                return false;
            }
            data.append(chunk, static_cast<size_t>(n));
        }
        std::istringstream head(data.substr(0, end));
        std::string line;
        std::getline(head, line);
        std::istringstream first(line);
        first >> request.method >> request.path;
        while (std::getline(head, line)) {
            if (!line.empty() && line.back() == '\r') {
                line.pop_back();
            }
            size_t colon = line.find(':');
            if (colon == std::string::npos) {
                continue;
            }
            std::string value = line.substr(colon + 1);
            value.erase(0, value.find_first_not_of(' '));
            request.headers[lower(line.substr(0, colon))] = value;
        }
        size_t length = request.headers.count("content-length") ? std::stoul(request.headers["content-length"]) : 0;
        request.body = data.substr(end + 4);
        while (request.body.size() < length) {
            ssize_t n = recv(fd, chunk, sizeof chunk, 0);
            if (n <= 0) {
                return false;
            }
            request.body.append(chunk, static_cast<size_t>(n));
        }
        return true;
    }

    static void send_all(int fd, const std::string &data) {
        size_t sent = 0;
        while (sent < data.size()) {
            ssize_t n = send(fd, data.data() + sent, data.size() - sent, MSG_NOSIGNAL);
            if (n <= 0) {
                return;
            }
            sent += static_cast<size_t>(n);
        }
    }

    std::vector<std::string> responses_;
    int listener_ = -1;
    int port_ = 0;
    std::atomic<bool> stop_{false};
    std::thread thread_;
    std::mutex mutex_;
    std::vector<Request> requests_;
};

/** One canned response as raw HTTP; `request_id` receives its X-Request-Id, if any. */
std::string render(yyjson_val *response, const std::string &fixtures, std::string &request_id) {
    int status = static_cast<int>(yyjson_get_int(yyjson_obj_get(response, "status")));
    std::map<std::string, std::string> headers;
    std::string body;
    if (yyjson_val *json = yyjson_obj_get(response, "json")) {
        body = dump(json);
        headers["content-type"] = status >= 400 ? "application/problem+json" : "application/json";
    } else if (yyjson_val *sse = yyjson_obj_get(response, "sse")) {
        body = read_file(fixtures + "/" + str(sse));
        headers["content-type"] = "text/event-stream";
    } else {
        body = str(yyjson_obj_get(response, "text"));
    }
    size_t i, n;
    yyjson_val *key, *value;
    yyjson_obj_foreach(yyjson_obj_get(response, "headers"), i, n, key, value) {
        headers[lower(str(key))] = str(value);
    }
    request_id = headers.count("x-request-id") ? headers["x-request-id"] : std::string();
    std::string out = "HTTP/1.1 " + std::to_string(status) + " Status\r\n";
    for (const auto &[name, header] : headers) {
        out += name + ": " + header + "\r\n";
    }
    out += "Content-Length: " + std::to_string(body.size()) + "\r\nConnection: close\r\n\r\n" + body;
    return out;
}

// ----------------------------------------------------------------- cases

struct Failure : std::runtime_error {
    using std::runtime_error::runtime_error;
};

void require(bool condition, const std::string &message) {
    if (!condition) {
        throw Failure(message);
    }
}

void check_subset(yyjson_val *expected, const std::string &actual_json, const std::string &what) {
    Doc actual(actual_json);
    std::string path = what;
    require(subset(expected, actual.root(), false, path),
            path + ": expected " + dump(expected) + " in " + actual_json);
}

gateway::ChatRequest chat_request(yyjson_val *body) {
    gateway::ChatRequest request;
    request.model = str(yyjson_obj_get(body, "model"));
    size_t i, n;
    yyjson_val *message;
    yyjson_arr_foreach(yyjson_obj_get(body, "messages"), i, n, message) {
        gateway::Message m{str(yyjson_obj_get(message, "role")), str(yyjson_obj_get(message, "content"))};
        size_t k, count;
        yyjson_val *call;
        yyjson_arr_foreach(yyjson_obj_get(message, "tool_calls"), k, count, call) {
            yyjson_val *function = yyjson_obj_get(call, "function");
            m.tool_calls.push_back({str(yyjson_obj_get(call, "id")), str(yyjson_obj_get(function, "name")),
                                    str(yyjson_obj_get(function, "arguments"))});
        }
        if (yyjson_val *id = yyjson_obj_get(message, "tool_call_id")) {
            m.tool_call_id = str(id);
        }
        request.messages.push_back(std::move(m));
    }
    size_t ti, tool_count;
    yyjson_val *tool;
    yyjson_arr_foreach(yyjson_obj_get(body, "tools"), ti, tool_count, tool) {
        yyjson_val *function = yyjson_obj_get(tool, "function");
        gateway::Tool definition{str(yyjson_obj_get(function, "name"))};
        if (yyjson_val *description = yyjson_obj_get(function, "description")) {
            definition.description = str(description);
        }
        if (yyjson_val *parameters = yyjson_obj_get(function, "parameters")) {
            char *json = yyjson_val_write(parameters, 0, nullptr);
            definition.parameters_json = json ? json : "";
            std::free(json);
        }
        request.tools.push_back(std::move(definition));
    }
    if (yyjson_val *choice = yyjson_obj_get(body, "tool_choice")) {
        if (yyjson_is_str(choice)) {
            request.tool_choice = str(choice);
        } else {
            request.tool_choice_function = str(yyjson_obj_get(yyjson_obj_get(choice, "function"), "name"));
        }
    }
    if (yyjson_val *parallel = yyjson_obj_get(body, "parallel_tool_calls")) {
        request.parallel_tool_calls = yyjson_get_bool(parallel);
    }
    if (yyjson_val *t = yyjson_obj_get(body, "temperature")) {
        request.temperature = yyjson_get_num(t);
    }
    if (yyjson_val *m = yyjson_obj_get(body, "max_tokens")) {
        request.max_tokens = static_cast<int>(yyjson_get_int(m));
    }
    return request;
}

void check_error(yyjson_val *expected, const gateway::Error &error) {
    std::string kind = str(yyjson_obj_get(expected, "kind"));
    const char *kinds[] = {"problem", "http", "stream", "protocol"};
    gw_status statuses[] = {GW_E_PROBLEM, GW_E_HTTP, GW_E_STREAM, GW_E_PROTOCOL};
    gw_status want = GW_OK;
    for (size_t k = 0; k < 4; k++) {
        if (kind == kinds[k]) {
            want = statuses[k];
        }
    }
    require(error.status() == want, std::string("error kind: expected ") + kind + ", got " +
                                        gw_status_name(error.status()) + ": " + error.what());
    if (yyjson_val *status = yyjson_obj_get(expected, "status")) {
        require(error.http_status() == yyjson_get_int(status),
                "error status: got " + std::to_string(error.http_status()) + " (" + error.what() + ")");
    }
    if (yyjson_val *code = yyjson_obj_get(expected, "code")) {
        require(error.code() == str(code), "error code: got " + error.code());
    }
    if (yyjson_val *id = yyjson_obj_get(expected, "request_id")) {
        require(error.request_id() == str(id), "error request_id: got " + error.request_id());
    }
    if (yyjson_val *retry = yyjson_obj_get(expected, "retry_after")) {
        require(error.retry_after() == yyjson_get_int(retry),
                "error retry_after: got " + std::to_string(error.retry_after()));
    }
}

void check_requests(yyjson_val *c, const std::vector<Request> &requests) {
    yyjson_val *expect = yyjson_obj_get(c, "expect");
    yyjson_val *want = yyjson_obj_get(c, "expect_request");
    require(static_cast<int64_t>(requests.size()) == yyjson_get_int(yyjson_obj_get(expect, "attempts")),
            "attempts: got " + std::to_string(requests.size()));
    for (const auto &request : requests) {
        require(request.method == str(yyjson_obj_get(want, "method")), "method: got " + request.method);
        require(request.path == str(yyjson_obj_get(want, "path")), "path: got " + request.path);
        auto header = [&request](const std::string &name) {
            auto found = request.headers.find(name);
            return found == request.headers.end() ? std::optional<std::string>() : found->second;
        };
        size_t i, n;
        yyjson_val *key, *value;
        yyjson_obj_foreach(yyjson_obj_get(want, "headers"), i, n, key, value) {
            require(header(str(key)) == str(value), "header " + str(key) + ": got " + header(str(key)).value_or("<none>"));
        }
        require(header("content-type") == std::string("application/json"), "content-type");
        yyjson_arr_foreach(yyjson_obj_get(want, "absent_headers"), i, n, value) {
            require(!header(str(value)), "header " + str(value) + " must be absent");
        }
        if (yyjson_get_bool(yyjson_obj_get(want, "generated_idempotency_key"))) {
            auto key_value = header("idempotency-key");
            require(key_value && !key_value->empty() && key_value->size() <= 255, "generated Idempotency-Key");
        }
        Doc body(request.body);
        std::string path = "body";
        require(body.root() && subset(yyjson_obj_get(want, "body"), body.root(), true, path),
                path + ": sent " + request.body);
    }
    if (yyjson_get_bool(yyjson_obj_get(expect, "same_idempotency_key"))) {
        for (const auto &request : requests) {
            require(request.headers.at("idempotency-key") == requests[0].headers.at("idempotency-key"),
                    "attempts must reuse the Idempotency-Key");
        }
    }
}

void run_case(yyjson_val *c, const std::string &fixtures) {
    std::vector<std::string> responses;
    std::vector<std::string> served_ids;
    size_t i, n;
    yyjson_val *response;
    yyjson_arr_foreach(yyjson_obj_get(c, "responses"), i, n, response) {
        std::string id;
        responses.push_back(render(response, fixtures, id));
        served_ids.push_back(id);
    }
    Server server(responses);

    std::vector<std::int64_t> sleeps;
    gateway::ClientOptions options{server.url(), "gw_conformance_key"};
    if (yyjson_val *retries = yyjson_obj_get(yyjson_obj_get(c, "client"), "max_retries")) {
        options.max_retries = static_cast<int>(yyjson_get_int(retries));
    }
    options.timeout_ms = 10000;
    options.sleep = [&sleeps](std::int64_t ms, const gw_cancel *) {
        sleeps.push_back(ms);
        return true;
    };
    gateway::Client client(std::move(options));

    yyjson_val *call = yyjson_obj_get(c, "call");
    yyjson_val *body = yyjson_obj_get(call, "body");
    gateway::CallOptions call_options;
    if (yyjson_val *key = yyjson_obj_get(call, "idempotency_key")) {
        call_options.idempotency_key = str(key);
    }
    if (yyjson_val *id = yyjson_obj_get(call, "request_id")) {
        call_options.request_id = str(id);
    }

    yyjson_val *expect = yyjson_obj_get(c, "expect");
    yyjson_val *expected_error = yyjson_obj_get(expect, "error");
    yyjson_val *expected_events = yyjson_obj_get(expect, "events");
    std::string operation = str(yyjson_obj_get(c, "operation"));
    size_t events_seen = 0;
    try {
        if (operation == "chat") {
            check_subset(yyjson_obj_get(expect, "result"), chat_result(client.chat(chat_request(body), call_options)),
                         "result");
        } else if (operation == "embeddings") {
            gateway::EmbeddingRequest request{str(yyjson_obj_get(body, "model")), {}};
            yyjson_val *input;
            yyjson_arr_foreach(yyjson_obj_get(body, "input"), i, n, input) { request.input.push_back(str(input)); }
            check_subset(yyjson_obj_get(expect, "result"), embedding_result(client.embeddings(request, call_options)),
                         "result");
        } else {
            gateway::Stream stream = client.chat_stream(chat_request(body), call_options);
            require(stream.request_id() == served_ids.at(0), "stream request_id: got " + stream.request_id());
            while (auto event = stream.next()) {
                require(events_seen < yyjson_arr_size(expected_events),
                        std::string("unexpected event ") + gateway::event_name(*event));
                yyjson_val *want = yyjson_arr_get(expected_events, events_seen);
                require(gateway::event_name(*event) == str(yyjson_obj_get(want, "event")),
                        std::string("event ") + std::to_string(events_seen) + ": got " + gateway::event_name(*event));
                check_subset(yyjson_obj_get(want, "data"), event_data(*event), "events[" + std::to_string(events_seen) + "]");
                events_seen++;
            }
        }
        require(!expected_error, "expected an error, the call succeeded");
    } catch (const gateway::Error &error) {
        require(expected_error != nullptr, std::string("unexpected error ") + gw_status_name(error.status()) + ": " +
                                               error.what());
        check_error(expected_error, error);
    }
    if (expected_events) {
        require(events_seen == yyjson_arr_size(expected_events),
                "events: got " + std::to_string(events_seen) + " of " + std::to_string(yyjson_arr_size(expected_events)));
    }

    std::string sleeps_seen;
    yyjson_val *expected_sleeps = yyjson_obj_get(expect, "sleeps");
    bool sleeps_match = sleeps.size() == yyjson_arr_size(expected_sleeps);
    for (size_t s = 0; s < sleeps.size(); s++) {
        sleeps_seen += std::to_string(sleeps[s]) + "ms ";
        sleeps_match = sleeps_match && static_cast<double>(sleeps[s]) ==
                                           1000.0 * yyjson_get_num(yyjson_arr_get(expected_sleeps, s));
    }
    require(sleeps_match, "sleeps: got " + sleeps_seen + "expected " + dump(expected_sleeps));
    check_requests(c, server.requests());
}

/** Text with NUL bytes reaches the wire and comes back whole through the wrapper. */
void wrapper_keeps_nul_bytes() {
    const std::string body = "{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"message\":{\"role\":"
                             "\"assistant\",\"content\":\"a\\u0000b\"},\"finish_reason\":\"stop\"}]}";
    Server server({"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: " +
                   std::to_string(body.size()) + "\r\nConnection: close\r\n\r\n" + body});
    gateway::Client client({server.url(), "gw_conformance_key"});
    auto response = client.chat({"m",
                                 {gateway::Message::user(std::string("x\0y", 3)),
                                  gateway::Message::assistant("", {{"call_1", "lookup", "{}"}}),
                                  gateway::Message::tool("call_1", std::string("p\0q", 3))}});
    require(response.content() == std::string("a\0b", 3), "response content lost its NUL byte");
    auto requests = server.requests();
    require(requests.size() == 1 && requests[0].body.find("\"x\\u0000y\"") != std::string::npos,
            "request content lost its NUL byte");
    require(requests[0].body.find("\"content\":\"p\\u0000q\",\"tool_call_id\":\"call_1\"") != std::string::npos,
            "tool result lost its NUL byte");
}

} // namespace

int main(int argc, char **argv) {
    if (argc != 2) {
        std::cerr << "usage: conformance <clients/conformance>\n";
        return 2;
    }
    std::string root = argv[1];
    std::vector<std::string> names;
    if (DIR *dir = opendir((root + "/cases").c_str())) {
        while (dirent *entry = readdir(dir)) {
            std::string name = entry->d_name;
            if (name.size() > 5 && name.substr(name.size() - 5) == ".json") {
                names.push_back(name);
            }
        }
        closedir(dir);
    }
    std::sort(names.begin(), names.end());
    if (names.empty()) {
        std::cerr << "no cases under " << root << "/cases\n";
        return 2;
    }
    int failed = 0;
    try {
        wrapper_keeps_nul_bytes();
        std::cout << "ok   wrapper keeps NUL bytes\n";
    } catch (const std::exception &error) {
        failed++;
        std::cout << "FAIL wrapper keeps NUL bytes: " << error.what() << "\n";
    }
    for (const auto &name : names) {
        Doc c(read_file(root + "/cases/" + name));
        try {
            if (!c.root()) {
                throw Failure("unreadable case");
            }
            run_case(c.root(), root + "/fixtures");
            std::cout << "ok   " << name << "\n";
        } catch (const std::exception &error) {
            failed++;
            std::cout << "FAIL " << name << ": " << error.what() << "\n";
        }
    }
    std::cout << (failed ? "FAILED: " : "") << names.size() << " cases and the wrapper check, " << failed
              << " failed\n";
    gw_global_cleanup();
    return failed ? 1 : 0;
}
