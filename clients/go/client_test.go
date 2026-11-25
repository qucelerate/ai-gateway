package gateway

import (
	"context"
	"encoding/json"
	"errors"
	"math"
	"net"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
	"time"
)

var hello = ChatRequest{Model: "m", Messages: []Message{{Role: RoleUser, Content: "hi"}}}

func TestAStreamThatNeverStartsTimesOut(t *testing.T) {
	release := make(chan struct{})
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-release:
		case <-r.Context().Done():
		}
	}))
	defer server.Close()
	defer close(release)
	client, _ := NewClient(server.URL, "gw_key", WithTimeout(200*time.Millisecond))
	started := time.Now()
	_, err := client.ChatStream(context.Background(), hello)
	if !errors.Is(err, ErrTimeout) || time.Since(started) > 5*time.Second {
		t.Fatalf("%v after %v", err, time.Since(started))
	}
}

func TestAStreamOutlivesTheTimeoutOnceStarted(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.Write([]byte(started))
		w.(http.Flusher).Flush()
		time.Sleep(400 * time.Millisecond)
		w.Write([]byte("event: response.completed\ndata: {\"model\":\"m\",\"usage\":null}\n\n"))
	}))
	defer server.Close()
	client, _ := NewClient(server.URL, "gw_key", WithTimeout(200*time.Millisecond))
	stream, err := client.ChatStream(context.Background(), hello)
	if err != nil {
		t.Fatal(err)
	}
	defer stream.Close()
	count := 0
	for stream.Next() {
		count++
	}
	if count != 2 || stream.Err() != nil {
		t.Fatalf("%d %v", count, stream.Err())
	}
}

func TestAnErrorBodyCutShortIsATransportError(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	go func() {
		connection, err := listener.Accept()
		if err != nil {
			return
		}
		defer connection.Close()
		buffer := make([]byte, 65536)
		connection.Read(buffer)
		connection.Write([]byte("HTTP/1.1 503 X\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{\"code\":"))
	}()
	client, _ := NewClient("http://"+listener.Addr().String(), "gw_key")
	_, err = client.Chat(context.Background(), hello)
	var problem *ProblemError
	var httpError *HTTPError
	if err == nil || errors.As(err, &problem) || errors.As(err, &httpError) || !strings.Contains(err.Error(), "503") {
		t.Fatalf("expected a read error, got %v", err)
	}
}

func TestTheAPIKeyIsRedacted(t *testing.T) {
	client, _ := NewClient("https://gateway.example.com", "gw_secret")
	for _, text := range []string{client.String(), client.GoString()} {
		if strings.Contains(text, "gw_secret") {
			t.Fatal(text)
		}
	}
}

func TestRetryWaitIsCappedWithoutOverflow(t *testing.T) {
	cases := map[int]time.Duration{-1: time.Second, 0: 0, 3: 3 * time.Second, 60: maxWait, 61: maxWait, int(^uint(0) >> 1): maxWait}
	if strconv.IntSize == 64 {
		// The smallest seconds whose Duration overflows int64; converted at
		// run time so the test still compiles on 32-bit targets.
		overflow := int64(9223372037)
		cases[int(overflow)] = maxWait
	}
	for seconds, want := range cases {
		if got := retryWait(seconds); got != want {
			t.Errorf("retryWait(%d) = %v, want %v", seconds, got, want)
		}
	}
}

func TestABodyOverTheResponseLimitIsADecodeError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"id":"` + strings.Repeat("x", 4096) + `"}`))
	}))
	defer server.Close()
	client, _ := NewClient(server.URL, "gw_key", WithMaxResponseBytes(1024))
	_, err := client.Chat(context.Background(), hello)
	var decodeError *DecodeError
	if !errors.As(err, &decodeError) || !strings.Contains(err.Error(), "1024") {
		t.Fatalf("expected a decode error, got %v", err)
	}
	if _, err := NewClient(server.URL, "gw_key", WithMaxResponseBytes(0)); !errors.Is(err, ErrConfig) {
		t.Fatalf("expected a config error, got %v", err)
	}
}

func TestTheLargestLimitStillReadsResponses(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"id":"c","model":"m","choices":[],"usage":null}`))
	}))
	defer server.Close()
	client, _ := NewClient(server.URL, "gw_key", WithMaxResponseBytes(math.MaxInt64))
	if response, err := client.Chat(context.Background(), hello); err != nil || response.ID != "c" {
		t.Fatalf("%v %v", response, err)
	}
}

func TestABodyAtTheResponseLimitIsRead(t *testing.T) {
	body := `{"id":"c","model":"m","choices":[],"usage":null}`
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(body))
	}))
	defer server.Close()
	client, _ := NewClient(server.URL, "gw_key", WithMaxResponseBytes(int64(len(body))))
	if response, err := client.Chat(context.Background(), hello); err != nil || response.ID != "c" {
		t.Fatalf("%v %v", response, err)
	}
}

func TestToolsAndCallsGoOnTheWire(t *testing.T) {
	calls := []ToolCall{{ID: "call_1", Type: "function", Function: FunctionCall{"get_weather", `{"city":"Paris"}`}}}
	request := ChatRequest{
		Model: "chat-default",
		Messages: []Message{
			{Role: RoleUser, Content: "Weather?"},
			MessageResponse{Role: "assistant", ToolCalls: calls}.ToMessage(),
			{Role: RoleTool, Content: "18C", ToolCallID: "call_1"},
		},
		Tools:             []Tool{FunctionTool("get_weather", "Current weather", json.RawMessage(`{"type":"object"}`))},
		ToolChoice:        &ToolChoiceRequired,
		ParallelToolCalls: Bool(false),
	}
	body, err := json.Marshal(request)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"model":"chat-default","messages":[{"role":"user","content":"Weather?"},` +
		`{"content":null,"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},` +
		`{"role":"tool","content":"18C","tool_call_id":"call_1"}],` +
		`"tools":[{"type":"function","function":{"name":"get_weather","description":"Current weather","parameters":{"type":"object"}}}],` +
		`"tool_choice":"required","parallel_tool_calls":false}`
	if string(body) != want {
		t.Fatalf("body:\n%s\nwant:\n%s", body, want)
	}
	named, _ := json.Marshal(ToolChoiceFunction("get_weather"))
	if string(named) != `{"function":{"name":"get_weather"},"type":"function"}` {
		t.Fatalf("named choice: %s", named)
	}
}
