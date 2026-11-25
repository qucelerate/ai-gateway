// gateway.hpp: a header-only C++17 wrapper over the C client (gateway.h):
// chat, streams, embeddings, and batch files and batches.
//
// It copies everything into standard types (std::string, std::vector,
// std::optional, std::variant), so no C++ type crosses the library boundary
// and the wrapper works with any compiler that can call the C ABI. Failures
// throw gateway::Error or a subclass; Client is safe to share across threads.
//
//   gateway::Client client({"https://gateway.example.com", std::getenv("GATEWAY_API_KEY")});
//   auto response = client.chat({"chat-default", {gateway::Message::user("Say hello.")}});
//   std::cout << response.content() << "\n";
#ifndef GATEWAY_GATEWAY_HPP
#define GATEWAY_GATEWAY_HPP

#include "gateway.h"

#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <stdexcept>
#include <string>
#include <utility>
#include <variant>
#include <vector>

namespace gateway {

// ------------------------------------------------------------------ errors

/** Every failure. status() is the C status; the subclasses name the common ones. */
class Error : public std::runtime_error {
public:
    Error(gw_status status, std::string message, int http_status = 0, std::string code = {},
          std::string request_id = {}, std::int64_t retry_after = -1)
        : std::runtime_error(std::move(message)), status_(status), http_status_(http_status),
          code_(std::move(code)), request_id_(std::move(request_id)), retry_after_(retry_after) {}

    gw_status status() const noexcept { return status_; }
    /** The HTTP status of a problem, error response or stream error; 0 if none. */
    int http_status() const noexcept { return http_status_; }
    /** A problem's or stream error's code (an open string), or empty. */
    const std::string &code() const noexcept { return code_; }
    const std::string &request_id() const noexcept { return request_id_; }
    /** Retry-After in whole seconds, or -1 when absent. */
    std::int64_t retry_after() const noexcept { return retry_after_; }

private:
    gw_status status_;
    int http_status_;
    std::string code_;
    std::string request_id_;
    std::int64_t retry_after_;
};

#define GATEWAY_ERROR_CLASS(Name)                                                                  \
    class Name : public Error {                                                                    \
    public:                                                                                        \
        using Error::Error;                                                                        \
    };
GATEWAY_ERROR_CLASS(ProblemError)   // GW_E_PROBLEM: an RFC 9457 problem
GATEWAY_ERROR_CLASS(HttpError)      // GW_E_HTTP: an error response without a problem
GATEWAY_ERROR_CLASS(StreamError)    // GW_E_STREAM: a stream's terminal error event
GATEWAY_ERROR_CLASS(ProtocolError)  // GW_E_PROTOCOL: early close or an event over 16 MiB
GATEWAY_ERROR_CLASS(DecodeError)    // GW_E_DECODE: a body that does not match the contract
GATEWAY_ERROR_CLASS(TransportError) // GW_E_TRANSPORT
GATEWAY_ERROR_CLASS(TimeoutError)   // GW_E_TIMEOUT
GATEWAY_ERROR_CLASS(CancelledError) // GW_E_CANCELLED
GATEWAY_ERROR_CLASS(ConfigError)    // GW_E_CONFIG
#undef GATEWAY_ERROR_CLASS

namespace detail {

inline std::string str(const char *text) { return text ? std::string(text) : std::string(); }

inline std::optional<std::string> opt(const char *text) {
    return text ? std::optional<std::string>(text) : std::nullopt;
}

/** Model text, which may contain NUL bytes: copy all `length` bytes. */
inline std::optional<std::string> opt(const char *text, std::size_t length) {
    return text ? std::optional<std::string>(std::string(text, length)) : std::nullopt;
}

/** Event text with its length, such as gw_event_content_delta. */
inline std::optional<std::string> text(const char *(*get)(const gw_event *, std::size_t *), const gw_event *event) {
    std::size_t length = 0;
    const char *value = get(event, &length);
    return opt(value, length);
}

/** A choice's text with its length, such as gw_chat_response_choice_content. */
inline std::optional<std::string> text(const char *(*get)(const gw_chat_response *, std::size_t, std::size_t *),
                                       const gw_chat_response *response, std::size_t choice) {
    std::size_t length = 0;
    const char *value = get(response, choice, &length);
    return opt(value, length);
}

template <class E> [[noreturn]] void throw_as(gw_status status, std::string message, const gw_error *error) {
    throw E(status, std::move(message), gw_error_http_status(error), str(gw_error_code(error)),
            str(gw_error_request_id(error)), gw_error_retry_after(error));
}

/** Throw the exception for a failed status, freeing the C error. */
[[noreturn]] inline void raise(gw_status status, gw_error *error) {
    std::unique_ptr<gw_error, decltype(&gw_error_free)> owned(error, gw_error_free);
    std::string message = error ? str(gw_error_message(error)) : std::string(gw_status_name(status));
    switch (status) {
    case GW_E_PROBLEM: throw_as<ProblemError>(status, message, error);
    case GW_E_HTTP: throw_as<HttpError>(status, message, error);
    case GW_E_STREAM: throw_as<StreamError>(status, message, error);
    case GW_E_PROTOCOL: throw_as<ProtocolError>(status, message, error);
    case GW_E_DECODE: throw_as<DecodeError>(status, message, error);
    case GW_E_TRANSPORT: throw_as<TransportError>(status, message, error);
    case GW_E_TIMEOUT: throw_as<TimeoutError>(status, message, error);
    case GW_E_CANCELLED: throw_as<CancelledError>(status, message, error);
    case GW_E_CONFIG: throw_as<ConfigError>(status, message, error);
    case GW_E_NOMEM: throw std::bad_alloc();
    default: throw_as<Error>(status, message, error);
    }
}

inline void check(gw_status status, gw_error *error = nullptr) {
    if (status != GW_OK && status != GW_DONE) {
        raise(status, error);
    }
    gw_error_free(error);
}

template <class T, void (*Free)(T *)> struct Deleter {
    void operator()(T *value) const noexcept { Free(value); }
};
template <class T, void (*Free)(T *)> using Owned = std::unique_ptr<T, Deleter<T, Free>>;

template <class T> T *require(T *value) {
    if (!value) {
        throw std::bad_alloc();
    }
    return value;
}

/** gw_global_init once per process, with the default allocator. */
inline void init() {
    static std::once_flag once;
    static gw_status status = GW_OK;
    std::call_once(once, [] { status = gw_global_init(nullptr); });
    if (status != GW_OK) {
        raise(status, nullptr);
    }
}

} // namespace detail

// --------------------------------------------------------------- requests

/** A call the model made; send it back unchanged before its result. */
struct ToolCall {
    std::string id;
    std::string name;
    /** A JSON object, serialized, as the model wrote it: validate it. */
    std::string arguments;

    bool operator==(const ToolCall &other) const {
        return id == other.id && name == other.name && arguments == other.arguments;
    }
};

/** A function the model may call; parameters_json is a JSON Schema object. */
struct Tool {
    std::string name;
    std::optional<std::string> description = std::nullopt;
    std::optional<std::string> parameters_json = std::nullopt;
};

struct Message {
    std::string role;
    std::string content;
    /** An assistant message's calls; an empty content is then sent as null. */
    std::vector<ToolCall> tool_calls = {};
    /** A tool message's: the call it answers. */
    std::optional<std::string> tool_call_id = std::nullopt;

    static Message system(std::string content) { return {"system", std::move(content)}; }
    static Message developer(std::string content) { return {"developer", std::move(content)}; }
    static Message user(std::string content) { return {"user", std::move(content)}; }
    static Message assistant(std::string content) { return {"assistant", std::move(content)}; }
    static Message assistant(std::string content, std::vector<ToolCall> calls) {
        return {"assistant", std::move(content), std::move(calls)};
    }
    static Message tool(std::string tool_call_id, std::string content) {
        return {"tool", std::move(content), {}, std::move(tool_call_id)};
    }
};

struct ChatRequest {
    std::string model;
    std::vector<Message> messages;
    std::optional<double> temperature = std::nullopt;
    std::optional<int> max_tokens = std::nullopt;
    /** Functions the model may call; an alias that cannot take tools refuses them. */
    std::vector<Tool> tools = {};
    /** "auto", "none" or "required". */
    std::optional<std::string> tool_choice = std::nullopt;
    /** Make the model call this function (instead of tool_choice). */
    std::optional<std::string> tool_choice_function = std::nullopt;
    std::optional<bool> parallel_tool_calls = std::nullopt;
};

struct EmbeddingRequest {
    std::string model;
    std::vector<std::string> input;
};

/** A cancellation flag; cancel() may be called from any thread. */
class Cancel {
public:
    Cancel() : cancel_(detail::require(gw_cancel_new())) {}
    void cancel() noexcept { gw_cancel_trigger(cancel_.get()); }
    bool cancelled() const noexcept { return gw_cancel_triggered(cancel_.get()); }
    gw_cancel *get() const noexcept { return cancel_.get(); }

private:
    detail::Owned<gw_cancel, gw_cancel_free> cancel_;
};

struct CallOptions {
    std::optional<std::string> idempotency_key = std::nullopt;
    std::optional<std::string> request_id = std::nullopt;
    /** Borrowed; it must outlive the call (and the stream). */
    const Cancel *cancel = nullptr;
};

// -------------------------------------------------------------- responses

/** Token usage; an unreported counter is std::nullopt, never 0. */
struct Usage {
    std::optional<std::int64_t> input_tokens;
    std::optional<std::int64_t> output_tokens;
    std::optional<std::int64_t> cached_input_tokens;
    std::optional<std::int64_t> reasoning_tokens;
    std::optional<std::int64_t> other_tokens;
    std::string quality;
    std::string completeness;

    static std::optional<Usage> from(const gw_usage *usage) {
        if (!usage) {
            return std::nullopt;
        }
        auto counter = [usage](gw_usage_counter which) {
            std::int64_t value = 0;
            return gw_usage_get(usage, which, &value) ? std::optional<std::int64_t>(value) : std::nullopt;
        };
        return Usage{counter(GW_USAGE_INPUT_TOKENS),     counter(GW_USAGE_OUTPUT_TOKENS),
                     counter(GW_USAGE_CACHED_INPUT_TOKENS), counter(GW_USAGE_REASONING_TOKENS),
                     counter(GW_USAGE_OTHER_TOKENS),     detail::str(gw_usage_quality(usage)),
                     detail::str(gw_usage_completeness(usage))};
    }
};

struct Choice {
    std::int64_t index = 0;
    std::string role;
    std::optional<std::string> content;
    /** An open string (stop, length, tool_calls, ...). */
    std::optional<std::string> finish_reason;
    std::optional<std::string> refusal;
    /** The calls the model made; empty when it made none. */
    std::vector<ToolCall> tool_calls = {};
};

struct ChatResponse {
    std::string id;
    std::string model;
    std::vector<Choice> choices;
    std::optional<Usage> usage;
    /** The X-Request-Id header: quote it when reporting a problem. */
    std::string request_id;
    /** The raw body, for fields not modeled here (tool calls, safety). */
    std::string json;

    /** The first choice's content, the usual answer, or "". */
    std::string content() const {
        return choices.empty() || !choices[0].content ? std::string() : *choices[0].content;
    }
};

struct Embedding {
    std::int64_t index = 0;
    std::vector<float> embedding;
};

struct EmbeddingResponse {
    std::string model;
    std::vector<Embedding> data;
    std::optional<Usage> usage;
    std::string request_id;
};

// ----------------------------------------------------------------- events

struct ResponseStarted {
    std::string model;
};
struct ChoiceDelta {
    std::int64_t choice_index = 0;
    std::optional<std::string> content_delta;
    std::optional<std::string> refusal_delta;
};
struct ToolCallDelta {
    std::int64_t choice_index = 0;
    std::int64_t tool_call_index = 0;
    std::optional<std::string> id;
    std::optional<std::string> name;
    std::string arguments_delta;
};
struct ChoiceFinished {
    std::int64_t choice_index = 0;
    std::string finish_reason;
};
struct SafetyAnnotation {
    std::int64_t choice_index = 0;
    std::string category;
    bool blocked = false;
};
struct UsageReport {
    Usage usage;
};
struct ResponseCompleted {
    std::string model;
    std::optional<Usage> usage;
};

/** One stream event; use std::get_if or std::visit. */
using Event = std::variant<ResponseStarted, ChoiceDelta, ToolCallDelta, ChoiceFinished, SafetyAnnotation,
                           UsageReport, ResponseCompleted>;

/** The SSE event name of an event, such as "choice.delta". */
inline const char *event_name(const Event &event) {
    static const char *const names[] = {"response.started",  "choice.delta", "tool_call.delta",
                                        "choice.finished",   "safety.annotation", "usage",
                                        "response.completed"};
    return names[event.index()];
}

namespace detail {

inline std::optional<Event> to_event(const gw_event *e) {
    switch (gw_event_get_type(e)) {
    case GW_EVENT_RESPONSE_STARTED: return Event{ResponseStarted{str(gw_event_model(e))}};
    case GW_EVENT_CHOICE_DELTA:
        return Event{ChoiceDelta{gw_event_choice_index(e), text(gw_event_content_delta, e),
                                 text(gw_event_refusal_delta, e)}};
    case GW_EVENT_TOOL_CALL_DELTA:
        return Event{ToolCallDelta{gw_event_choice_index(e), gw_event_tool_call_index(e),
                                   opt(gw_event_tool_call_id(e)), opt(gw_event_tool_name(e)),
                                   text(gw_event_arguments_delta, e).value_or(std::string())}};
    case GW_EVENT_CHOICE_FINISHED:
        return Event{ChoiceFinished{gw_event_choice_index(e), str(gw_event_finish_reason(e))}};
    case GW_EVENT_SAFETY_ANNOTATION:
        return Event{SafetyAnnotation{gw_event_choice_index(e), str(gw_event_safety_category(e)),
                                      gw_event_safety_blocked(e)}};
    case GW_EVENT_USAGE: return Event{UsageReport{Usage::from(gw_event_usage(e)).value_or(Usage{})}};
    case GW_EVENT_RESPONSE_COMPLETED:
        return Event{ResponseCompleted{str(gw_event_model(e)), Usage::from(gw_event_usage(e))}};
    }
    return std::nullopt; // a type added after this header: skipped
}

} // namespace detail

/** A streamed chat completion: call next() until it returns std::nullopt. */
class Stream {
public:
    explicit Stream(gw_stream *stream) : stream_(stream) {}

    /** The next event, or std::nullopt after response.completed; throws on failure. */
    std::optional<Event> next() {
        for (;;) {
            const gw_event *event = nullptr;
            gw_error *error = nullptr;
            gw_status status = gw_stream_next(stream_.get(), &event, &error);
            if (status == GW_DONE) {
                return std::nullopt;
            }
            detail::check(status, error);
            if (auto converted = detail::to_event(event)) {
                return converted;
            }
        }
    }

    /** The X-Request-Id response header, or "". */
    std::string request_id() const { return detail::str(gw_stream_request_id(stream_.get())); }

private:
    detail::Owned<gw_stream, gw_stream_free> stream_;
};

// ------------------------------------------------------- files and batches

/** An uploaded file. */
struct File {
    std::string id;
    std::string filename;
    std::string purpose;
    std::int64_t bytes = 0;
    /** Unix seconds. */
    std::int64_t created_at = 0;
    std::string request_id;
};

/** What create_batch sends; the completion window is "24h". */
struct BatchRequest {
    std::string input_file_id;
    /** "/v1/chat/completions" or "/v1/embeddings". */
    std::string endpoint;
    std::vector<std::pair<std::string, std::string>> metadata = {};
};

struct BatchCounts {
    std::int64_t total = 0;
    std::int64_t completed = 0;
    std::int64_t failed = 0;
};

struct Batch {
    std::string id;
    /** An open string: validating, in_progress, completed, cancelled, ... */
    std::string status;
    std::string endpoint;
    std::string completion_window;
    std::string input_file_id;
    std::optional<std::string> output_file_id;
    std::optional<std::string> error_file_id;
    /** Unix seconds. */
    std::int64_t created_at = 0;
    std::int64_t expires_at = 0;
    std::optional<std::int64_t> in_progress_at;
    std::optional<std::int64_t> finalizing_at;
    std::optional<std::int64_t> completed_at;
    std::optional<std::int64_t> failed_at;
    std::optional<std::int64_t> expired_at;
    std::optional<std::int64_t> cancelling_at;
    std::optional<std::int64_t> cancelled_at;
    BatchCounts request_counts;
    /** The raw batch, for fields not modeled here (metadata, errors, the gateway account). */
    std::string json;
    /** The X-Request-Id header; "" for a page's batches. */
    std::string request_id;

    static Batch from(const gw_batch *batch) {
        auto moment = [batch](gw_batch_time which) {
            std::int64_t value = 0;
            return gw_batch_time_get(batch, which, &value) ? std::optional<std::int64_t>(value) : std::nullopt;
        };
        Batch out;
        out.id = detail::str(gw_batch_id(batch));
        out.status = detail::str(gw_batch_status(batch));
        out.endpoint = detail::str(gw_batch_endpoint(batch));
        out.completion_window = detail::str(gw_batch_completion_window(batch));
        out.input_file_id = detail::str(gw_batch_input_file_id(batch));
        out.output_file_id = detail::opt(gw_batch_output_file_id(batch));
        out.error_file_id = detail::opt(gw_batch_error_file_id(batch));
        out.created_at = moment(GW_BATCH_CREATED_AT).value_or(0);
        out.expires_at = moment(GW_BATCH_EXPIRES_AT).value_or(0);
        out.in_progress_at = moment(GW_BATCH_IN_PROGRESS_AT);
        out.finalizing_at = moment(GW_BATCH_FINALIZING_AT);
        out.completed_at = moment(GW_BATCH_COMPLETED_AT);
        out.failed_at = moment(GW_BATCH_FAILED_AT);
        out.expired_at = moment(GW_BATCH_EXPIRED_AT);
        out.cancelling_at = moment(GW_BATCH_CANCELLING_AT);
        out.cancelled_at = moment(GW_BATCH_CANCELLED_AT);
        out.request_counts = {gw_batch_count_get(batch, GW_BATCH_LINES_TOTAL),
                              gw_batch_count_get(batch, GW_BATCH_LINES_COMPLETED),
                              gw_batch_count_get(batch, GW_BATCH_LINES_FAILED)};
        std::size_t length = 0;
        const char *json = gw_batch_json(batch, &length);
        out.json.assign(json ? json : "", length);
        out.request_id = detail::str(gw_batch_request_id(batch));
        return out;
    }
};

/** A page of batches, newest first. */
struct BatchPage {
    std::vector<Batch> data;
    bool has_more = false;
    /** Pass as `after` for the next page. */
    std::optional<std::string> last_id;
    std::string request_id;
};

// ------------------------------------------------------------------ client

struct ClientOptions {
    std::string base_url;
    std::string api_key;
    int max_retries = 2;
    std::int64_t timeout_ms = 120000;
    std::optional<std::string> ca_file = std::nullopt;
    std::optional<std::string> ca_path = std::nullopt;
    /** How to wait before a retry; return false to stop (cancelled). Unset: a cancellable sleep. */
    std::function<bool(std::int64_t milliseconds, const gw_cancel *cancel)> sleep = nullptr;
};

class Client {
public:
    explicit Client(ClientOptions options) : sleep_(new Sleep{std::move(options.sleep)}) {
        detail::init();
        detail::Owned<gw_client_options, gw_client_options_free> o(detail::require(gw_client_options_new()));
        detail::check(gw_client_options_set_base_url(o.get(), options.base_url.c_str()));
        detail::check(gw_client_options_set_api_key(o.get(), options.api_key.c_str()));
        detail::check(gw_client_options_set_max_retries(o.get(), options.max_retries));
        detail::check(gw_client_options_set_timeout_ms(o.get(), options.timeout_ms));
        if (options.ca_file) {
            detail::check(gw_client_options_set_ca_file(o.get(), options.ca_file->c_str()));
        }
        if (options.ca_path) {
            detail::check(gw_client_options_set_ca_path(o.get(), options.ca_path->c_str()));
        }
        if (sleep_->function) {
            detail::check(gw_client_options_set_sleep(o.get(), &Sleep::call, sleep_.get()));
        }
        gw_client *client = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_client_new(o.get(), &client, &error);
        detail::check(status, error); // apart: argument order is unspecified
        client_.reset(client);
    }

    ChatResponse chat(const ChatRequest &request, const CallOptions &options = {}) {
        auto c_request = chat_request(request);
        auto c_options = call_options(options);
        gw_chat_response *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_chat(client_.get(), c_request.get(), c_options.get(), &raw, &error);
        detail::check(status, error);
        detail::Owned<gw_chat_response, gw_chat_response_free> response(raw);
        ChatResponse out;
        out.id = detail::str(gw_chat_response_id(raw));
        out.model = detail::str(gw_chat_response_model(raw));
        out.request_id = detail::str(gw_chat_response_request_id(raw));
        for (size_t i = 0; i < gw_chat_response_choice_count(raw); i++) {
            Choice choice{gw_chat_response_choice_index(raw, i),
                          detail::str(gw_chat_response_choice_role(raw, i)),
                          detail::text(gw_chat_response_choice_content, raw, i),
                          detail::opt(gw_chat_response_choice_finish_reason(raw, i)),
                          detail::text(gw_chat_response_choice_refusal, raw, i)};
            for (size_t k = 0; k < gw_chat_response_choice_tool_call_count(raw, i); k++) {
                const char *id = nullptr, *name = nullptr, *arguments = nullptr;
                gw_chat_response_choice_tool_call(raw, i, k, &id, &name, &arguments);
                choice.tool_calls.push_back({detail::str(id), detail::str(name), detail::str(arguments)});
            }
            out.choices.push_back(std::move(choice));
        }
        out.usage = Usage::from(gw_chat_response_usage(raw));
        size_t length = 0;
        const char *json = gw_chat_response_json(raw, &length);
        out.json.assign(json ? json : "", length);
        return out;
    }

    EmbeddingResponse embeddings(const EmbeddingRequest &request, const CallOptions &options = {}) {
        detail::Owned<gw_embedding_request, gw_embedding_request_free> c_request(
            detail::require(gw_embedding_request_new(request.model.c_str())));
        for (const auto &input : request.input) {
            detail::check(gw_embedding_request_add_input_n(c_request.get(), input.data(), input.size()));
        }
        auto c_options = call_options(options);
        gw_embedding_response *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_embeddings(client_.get(), c_request.get(), c_options.get(), &raw, &error);
        detail::check(status, error);
        detail::Owned<gw_embedding_response, gw_embedding_response_free> response(raw);
        EmbeddingResponse out;
        out.model = detail::str(gw_embedding_response_model(raw));
        out.request_id = detail::str(gw_embedding_response_request_id(raw));
        for (size_t i = 0; i < gw_embedding_response_count(raw); i++) {
            size_t dimensions = 0;
            const float *vector = gw_embedding_response_vector(raw, i, &dimensions);
            out.data.push_back(Embedding{gw_embedding_response_index(raw, i),
                                         std::vector<float>(vector, vector + dimensions)});
        }
        out.usage = Usage::from(gw_embedding_response_usage(raw));
        return out;
    }

    /** Open a stream; its response must start within the client's timeout. */
    Stream chat_stream(const ChatRequest &request, const CallOptions &options = {}) {
        auto c_request = chat_request(request);
        auto c_options = call_options(options);
        gw_stream *stream = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_chat_stream_open(client_.get(), c_request.get(), c_options.get(), &stream, &error);
        detail::check(status, error);
        return Stream(stream);
    }

    /** Upload a batch input file (JSONL, at most 64 MiB). */
    File upload_file(const std::string &filename, const std::string &data, const CallOptions &options = {}) {
        auto c_options = call_options(options);
        gw_file *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status =
            gw_file_upload(client_.get(), filename.c_str(), data.data(), data.size(), c_options.get(), &raw, &error);
        detail::check(status, error);
        detail::Owned<gw_file, gw_file_free> file(raw);
        return File{detail::str(gw_file_id(raw)),  detail::str(gw_file_filename(raw)),
                    detail::str(gw_file_purpose(raw)), gw_file_bytes(raw),
                    gw_file_created_at(raw),        detail::str(gw_file_request_id(raw))};
    }

    /** A file's content, such as a batch's output or error file (at most 256 MiB). */
    std::string file_content(const std::string &file_id, const CallOptions &options = {}) {
        auto c_options = call_options(options);
        gw_file_content *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_file_download(client_.get(), file_id.c_str(), c_options.get(), &raw, &error);
        detail::check(status, error);
        detail::Owned<gw_file_content, gw_file_content_free> content(raw);
        std::size_t length = 0;
        const char *data = gw_file_content_data(raw, &length);
        return std::string(data ? data : "", length);
    }

    /** Create a batch; sends an Idempotency-Key, so a retry with the same key returns the same batch. */
    Batch create_batch(const BatchRequest &request, const CallOptions &options = {}) {
        detail::Owned<gw_batch_request, gw_batch_request_free> c_request(detail::require(
            gw_batch_request_new(request.input_file_id.c_str(), request.endpoint.c_str())));
        for (const auto &[key, value] : request.metadata) {
            detail::check(gw_batch_request_set_metadata(c_request.get(), key.c_str(), value.c_str()));
        }
        auto c_options = call_options(options);
        gw_batch *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_batch_create(client_.get(), c_request.get(), c_options.get(), &raw, &error);
        return batch_from(status, raw, error);
    }

    Batch batch(const std::string &batch_id, const CallOptions &options = {}) {
        auto c_options = call_options(options);
        gw_batch *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_batch_retrieve(client_.get(), batch_id.c_str(), c_options.get(), &raw, &error);
        return batch_from(status, raw, error);
    }

    Batch cancel_batch(const std::string &batch_id, const CallOptions &options = {}) {
        auto c_options = call_options(options);
        gw_batch *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_batch_cancel(client_.get(), batch_id.c_str(), c_options.get(), &raw, &error);
        return batch_from(status, raw, error);
    }

    /** A page of batches (`limit` 1 to 100, 0 for the gateway's 20) after `after`. */
    BatchPage list_batches(const std::optional<std::string> &after = std::nullopt, int limit = 0,
                           const CallOptions &options = {}) {
        auto c_options = call_options(options);
        gw_batch_page *raw = nullptr;
        gw_error *error = nullptr;
        gw_status status = gw_batches_list(client_.get(), after ? after->c_str() : nullptr, limit, c_options.get(),
                                           &raw, &error);
        detail::check(status, error);
        detail::Owned<gw_batch_page, gw_batch_page_free> page(raw);
        BatchPage out;
        for (std::size_t i = 0; i < gw_batch_page_count(raw); i++) {
            out.data.push_back(Batch::from(gw_batch_page_item(raw, i)));
        }
        out.has_more = gw_batch_page_has_more(raw);
        out.last_id = detail::opt(gw_batch_page_last_id(raw));
        out.request_id = detail::str(gw_batch_page_request_id(raw));
        return out;
    }

private:
    static Batch batch_from(gw_status status, gw_batch *raw, gw_error *error) {
        detail::check(status, error);
        detail::Owned<gw_batch, gw_batch_free> batch(raw);
        return Batch::from(raw);
    }

    struct Sleep {
        std::function<bool(std::int64_t, const gw_cancel *)> function;
        static bool call(void *ctx, int64_t milliseconds, const gw_cancel *cancel) {
            return static_cast<Sleep *>(ctx)->function(milliseconds, cancel);
        }
    };

    static detail::Owned<gw_chat_request, gw_chat_request_free> chat_request(const ChatRequest &request) {
        detail::Owned<gw_chat_request, gw_chat_request_free> out(
            detail::require(gw_chat_request_new(request.model.c_str())));
        for (const auto &message : request.messages) {
            if (message.tool_call_id) {
                detail::check(gw_chat_request_add_tool_result_n(out.get(), message.tool_call_id->c_str(),
                                                                message.content.data(), message.content.size()));
                continue;
            }
            detail::check(gw_chat_request_add_message_n(out.get(), message.role.c_str(), message.content.data(),
                                                        message.content.size()));
            for (const auto &call : message.tool_calls) {
                detail::check(gw_chat_request_add_tool_call(out.get(), call.id.c_str(), call.name.c_str(),
                                                            call.arguments.c_str()));
            }
        }
        for (const auto &tool : request.tools) {
            detail::check(gw_chat_request_add_tool(out.get(), tool.name.c_str(),
                                                   tool.description ? tool.description->c_str() : nullptr,
                                                   tool.parameters_json ? tool.parameters_json->c_str() : nullptr));
        }
        if (request.tool_choice) {
            detail::check(gw_chat_request_set_tool_choice(out.get(), request.tool_choice->c_str()));
        }
        if (request.tool_choice_function) {
            detail::check(gw_chat_request_set_tool_choice_function(out.get(), request.tool_choice_function->c_str()));
        }
        if (request.parallel_tool_calls) {
            detail::check(gw_chat_request_set_parallel_tool_calls(out.get(), *request.parallel_tool_calls));
        }
        if (request.temperature) {
            detail::check(gw_chat_request_set_temperature(out.get(), *request.temperature));
        }
        if (request.max_tokens) {
            detail::check(gw_chat_request_set_max_tokens(out.get(), *request.max_tokens));
        }
        return out;
    }

    static detail::Owned<gw_call_options, gw_call_options_free> call_options(const CallOptions &options) {
        detail::Owned<gw_call_options, gw_call_options_free> out(detail::require(gw_call_options_new()));
        if (options.idempotency_key) {
            detail::check(gw_call_options_set_idempotency_key(out.get(), options.idempotency_key->c_str()));
        }
        if (options.request_id) {
            detail::check(gw_call_options_set_request_id(out.get(), options.request_id->c_str()));
        }
        if (options.cancel) {
            detail::check(gw_call_options_set_cancel(out.get(), options.cancel->get()));
        }
        return out;
    }

    std::unique_ptr<Sleep> sleep_;
    detail::Owned<gw_client, gw_client_free> client_;
};

} // namespace gateway

#endif // GATEWAY_GATEWAY_HPP
