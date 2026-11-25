package gateway

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"sync"
	"testing"
	"time"
)

// The suite in clients/conformance: every case runs against a mock gateway
// that serves the case's responses in order and records what it received.

type conformanceCase struct {
	Name      string `json:"name"`
	Operation string `json:"operation"`
	Call      struct {
		Body           json.RawMessage `json:"body"`
		IdempotencyKey *string         `json:"idempotency_key"`
		RequestID      *string         `json:"request_id"`
	} `json:"call"`
	Client *struct {
		MaxRetries *int `json:"max_retries"`
	} `json:"client"`
	ExpectRequest struct {
		Method                  string            `json:"method"`
		Path                    string            `json:"path"`
		Headers                 map[string]string `json:"headers"`
		AbsentHeaders           []string          `json:"absent_headers"`
		Body                    any               `json:"body"`
		GeneratedIdempotencyKey bool              `json:"generated_idempotency_key"`
	} `json:"expect_request"`
	Responses []struct {
		Status  int               `json:"status"`
		Headers map[string]string `json:"headers"`
		JSON    json.RawMessage   `json:"json"`
		SSE     string            `json:"sse"`
		Text    *string           `json:"text"`
	} `json:"responses"`
	Expect struct {
		Attempts int            `json:"attempts"`
		Sleeps   []float64      `json:"sleeps"`
		Result   any            `json:"result"`
		Events   []any          `json:"events"`
		Error    map[string]any `json:"error"`
	} `json:"expect"`
}

type recorded struct {
	method, path string
	header       http.Header
	body         []byte
}

func suiteDir() string { return filepath.Join("..", "conformance") }

func TestConformance(t *testing.T) {
	paths, err := filepath.Glob(filepath.Join(suiteDir(), "cases", "*.json"))
	if err != nil || len(paths) == 0 {
		t.Fatalf("no conformance cases: %v", err)
	}
	sort.Strings(paths)
	for _, path := range paths {
		raw, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		var c conformanceCase
		if err := json.Unmarshal(raw, &c); err != nil {
			t.Fatalf("%s: %v", path, err)
		}
		t.Run(c.Name, func(t *testing.T) { runCase(t, &c) })
	}
}

func runCase(t *testing.T, c *conformanceCase) {
	var mu sync.Mutex
	var requests []recorded
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		mu.Lock()
		index := len(requests)
		requests = append(requests, recorded{r.Method, r.URL.Path, r.Header.Clone(), body})
		mu.Unlock()
		if index >= len(c.Responses) {
			http.Error(w, "no more responses", http.StatusInternalServerError)
			return
		}
		response := c.Responses[index]
		var payload []byte
		contentType := "text/plain"
		switch {
		case response.JSON != nil:
			payload, contentType = response.JSON, "application/json"
		case response.SSE != "":
			payload, err := os.ReadFile(filepath.Join(suiteDir(), "fixtures", response.SSE))
			if err != nil {
				t.Errorf("fixture: %v", err)
			}
			writeResponse(w, response.Status, response.Headers, "text/event-stream", payload)
			return
		case response.Text != nil:
			payload = []byte(*response.Text)
		}
		writeResponse(w, response.Status, response.Headers, contentType, payload)
	}))
	defer server.Close()

	var options []Option
	if c.Client != nil && c.Client.MaxRetries != nil {
		options = append(options, WithMaxRetries(*c.Client.MaxRetries))
	}
	client, err := NewClient(server.URL, "gw_conformance_key", options...)
	if err != nil {
		t.Fatal(err)
	}
	var sleeps []float64
	client.sleep = func(_ context.Context, d time.Duration) error {
		sleeps = append(sleeps, d.Seconds())
		return nil
	}
	var calls []CallOption
	if c.Call.IdempotencyKey != nil {
		calls = append(calls, WithIdempotencyKey(*c.Call.IdempotencyKey))
	}
	if c.Call.RequestID != nil {
		calls = append(calls, WithRequestID(*c.Call.RequestID))
	}

	ctx := context.Background()
	var result any
	var events []any
	var failure error
	switch c.Operation {
	case "chat":
		var request ChatRequest
		mustDecode(t, c.Call.Body, &request)
		var response *ChatResponse
		response, failure = client.Chat(ctx, request, calls...)
		if failure == nil {
			result = asJSON(t, response)
		}
	case "embeddings":
		var request EmbeddingRequest
		mustDecode(t, c.Call.Body, &request)
		var response *EmbeddingResponse
		response, failure = client.Embeddings(ctx, request, calls...)
		if failure == nil {
			result = asJSON(t, response)
		}
	case "chat_stream":
		var request ChatRequest
		mustDecode(t, c.Call.Body, &request)
		var stream *Stream
		stream, failure = client.ChatStream(ctx, request, calls...)
		if failure == nil {
			served := c.Responses[len(c.Responses)-1].Headers["x-request-id"]
			if stream.RequestID != served {
				t.Errorf("stream RequestID %q, served %q", stream.RequestID, served)
			}
			for stream.Next() {
				event := stream.Event()
				events = append(events, map[string]any{"event": event.EventName(), "data": asJSON(t, event)})
			}
			failure = stream.Err()
			stream.Close()
		}
	default:
		t.Fatalf("unknown operation %q", c.Operation)
	}

	if c.Operation == "chat_stream" {
		subset(t, c.Expect.Events, orEmpty(events), "events")
	}
	if c.Expect.Error != nil {
		checkError(t, c.Expect.Error, failure)
	} else if failure != nil {
		t.Fatalf("unexpected error: %v", failure)
	}
	if c.Expect.Result != nil {
		subset(t, c.Expect.Result, result, "result")
	}

	if len(requests) != c.Expect.Attempts {
		t.Errorf("attempts: got %d, want %d", len(requests), c.Expect.Attempts)
	}
	keys := map[string]bool{}
	for _, request := range requests {
		if request.method != c.ExpectRequest.Method || request.path != c.ExpectRequest.Path {
			t.Errorf("request %s %s", request.method, request.path)
		}
		if got := request.header.Get("Content-Type"); got != "application/json" {
			t.Errorf("content-type %q", got)
		}
		for name, value := range c.ExpectRequest.Headers {
			if got := request.header.Get(name); got != value {
				t.Errorf("header %s: got %q, want %q", name, got, value)
			}
		}
		for _, name := range c.ExpectRequest.AbsentHeaders {
			if _, ok := request.header[http.CanonicalHeaderKey(name)]; ok {
				t.Errorf("header %s sent", name)
			}
		}
		key := request.header.Get("Idempotency-Key")
		if c.ExpectRequest.GeneratedIdempotencyKey && (len(key) < 1 || len(key) > 255) {
			t.Errorf("generated key %q", key)
		}
		keys[key] = true
		var body any
		if err := json.Unmarshal(request.body, &body); err != nil {
			t.Errorf("body: %v", err)
		} else if !reflect.DeepEqual(body, c.ExpectRequest.Body) {
			t.Errorf("body: got %v, want %v", body, c.ExpectRequest.Body)
		}
	}
	if len(keys) != 1 {
		t.Errorf("idempotency keys: %v", keys)
	}
	if !reflect.DeepEqual(orEmptyFloats(sleeps), orEmptyFloats(c.Expect.Sleeps)) {
		t.Errorf("sleeps: got %v, want %v", sleeps, c.Expect.Sleeps)
	}
}

func writeResponse(w http.ResponseWriter, status int, headers map[string]string, contentType string, payload []byte) {
	for name, value := range headers {
		w.Header().Set(name, value)
	}
	if w.Header().Get("Content-Type") == "" {
		w.Header().Set("Content-Type", contentType)
	}
	w.WriteHeader(status)
	_, _ = w.Write(payload)
}

func checkError(t *testing.T, expected map[string]any, failure error) {
	t.Helper()
	if failure == nil {
		t.Fatalf("expected a %v error, got none", expected["kind"])
	}
	number := func(key string) int { return int(expected[key].(float64)) }
	switch expected["kind"] {
	case "problem":
		var problem *ProblemError
		if !errors.As(failure, &problem) {
			t.Fatalf("expected a problem, got %v", failure)
		}
		if problem.Status != number("status") || problem.Code != expected["code"] {
			t.Errorf("problem %d %s", problem.Status, problem.Code)
		}
		if id, ok := expected["request_id"]; ok && problem.RequestID != id {
			t.Errorf("request_id %q", problem.RequestID)
		}
		if _, ok := expected["retry_after"]; ok && problem.RetryAfter != number("retry_after") {
			t.Errorf("retry_after %d", problem.RetryAfter)
		}
	case "http":
		var httpError *HTTPError
		if !errors.As(failure, &httpError) || httpError.Status != number("status") {
			t.Errorf("expected HTTP %v, got %v", expected["status"], failure)
		}
	case "stream":
		var streamError *StreamError
		if !errors.As(failure, &streamError) || streamError.Code != expected["code"] || streamError.Status != number("status") {
			t.Errorf("expected stream error %v, got %v", expected, failure)
		}
	case "protocol":
		if !errors.Is(failure, ErrProtocol) {
			t.Errorf("expected a protocol error, got %v", failure)
		}
	default:
		t.Fatalf("unknown error kind %v", expected["kind"])
	}
}

// subset is the suite's match: listed keys must be present and equal;
// arrays must have the listed length.
func subset(t *testing.T, expected, actual any, path string) {
	t.Helper()
	switch want := expected.(type) {
	case map[string]any:
		got, ok := actual.(map[string]any)
		if !ok {
			t.Errorf("%s: expected an object, got %v", path, actual)
			return
		}
		for key, value := range want {
			inner, ok := got[key]
			if !ok {
				t.Errorf("%s.%s: missing", path, key)
				continue
			}
			subset(t, value, inner, path+"."+key)
		}
	case []any:
		got, ok := actual.([]any)
		if !ok || len(got) != len(want) {
			t.Errorf("%s: expected %d items, got %v", path, len(want), actual)
			return
		}
		for index := range want {
			subset(t, want[index], got[index], fmt.Sprintf("%s[%d]", path, index))
		}
	default:
		if !reflect.DeepEqual(expected, actual) {
			t.Errorf("%s: got %v, want %v", path, actual, expected)
		}
	}
}

func mustDecode(t *testing.T, raw json.RawMessage, out any) {
	t.Helper()
	if err := json.Unmarshal(raw, out); err != nil {
		t.Fatal(err)
	}
}

func asJSON(t *testing.T, value any) any {
	t.Helper()
	raw, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var out any
	mustDecode(t, raw, &out)
	return out
}

func orEmpty(values []any) []any {
	if values == nil {
		return []any{}
	}
	return values
}

func orEmptyFloats(values []float64) []float64 {
	if values == nil {
		return []float64{}
	}
	return values
}
