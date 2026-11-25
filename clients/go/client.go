// Package gateway is a client for the Gateway inference API, contract 1.0.0:
// chat, streaming chat, and embeddings.
//
//	client, err := gateway.NewClient("https://gateway.example.com", os.Getenv("GATEWAY_API_KEY"))
//	response, err := client.Chat(ctx, gateway.ChatRequest{
//		Model:    "chat-default",
//		Messages: []gateway.Message{{Role: gateway.RoleUser, Content: "Say hello."}},
//	})
//	fmt.Println(response.Content())
//
// The client is a transport wrapper: authorization, budgets, routing and
// settlement stay on the gateway. Every call sends an Idempotency-Key
// (generated unless you pass WithIdempotencyKey) and retries automatically
// only when the gateway refused the request before reserving anything; see
// WithMaxRetries. Cancel the context to cancel a call or close a stream.
package gateway

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"mime"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// Version is this client's version.
const Version = "2026.11.0-rc.2"

const (
	chatPath          = "/api/inference/v1/chat/completions"
	streamPath        = "/api/inference/v1/chat/completions/stream"
	embeddingsPath    = "/api/inference/v1/embeddings"
	maxWait           = 60 * time.Second
	maxKeyChars       = 255
	maxRequestIDChars = 128
	// maxErrorBodyBytes is how far an error body is read: enough for its problem.
	maxErrorBodyBytes = 1 << 20
	// defaultMaxResponseBytes is the default largest successful body read.
	defaultMaxResponseBytes = 64 << 20
)

// sameKeyRetryCodes are the problem codes for which the gateway refused the
// request before reserving anything, so the same Idempotency-Key is still
// unused and a retry is safe (retry: same_key in
// clients/conformance/errors.json).
var sameKeyRetryCodes = map[string]bool{
	"rate_limit_exceeded":        true,
	"concurrency_limit_exceeded": true,
	"provider_circuit_open":      true,
}

// Client calls the Gateway inference API. It is safe for concurrent use.
type Client struct {
	baseURL    string
	apiKey     string
	maxRetries int
	timeout    time.Duration
	http       *http.Client
	sleep      func(context.Context, time.Duration) error
	// maxResponseBytes caps a successful non-streaming body.
	maxResponseBytes int64
}

// Option configures a Client.
type Option func(*Client)

// WithMaxRetries sets how many times a request the gateway refused before
// reserving anything is retried (default 2). Other failures are never
// retried automatically.
func WithMaxRetries(n int) Option { return func(c *Client) { c.maxRetries = n } }

// WithTimeout limits each attempt of a non-streaming call (default 120
// seconds). A stream's response must start within it too, or the call fails
// with ErrTimeout; after that there is no limit between events, so cancel
// the context to stop one.
func WithTimeout(d time.Duration) Option { return func(c *Client) { c.timeout = d } }

// WithMaxResponseBytes sets the largest successful non-streaming response
// body read (default 64 MiB); a larger one fails the call with a *DecodeError.
func WithMaxResponseBytes(n int64) Option { return func(c *Client) { c.maxResponseBytes = n } }

// WithHTTPClient uses your own http.Client, for example for a proxy or TLS
// roots. Its Timeout should be zero, or streams are cut off.
func WithHTTPClient(h *http.Client) Option { return func(c *Client) { c.http = h } }

// NewClient returns a client for the gateway at baseURL, such as
// https://gateway.example.com, authenticating with a gw_ API key.
func NewClient(baseURL, apiKey string, options ...Option) (*Client, error) {
	if strings.TrimSpace(apiKey) == "" {
		return nil, fmt.Errorf("%w: an API key is required", ErrConfig)
	}
	parsed, err := url.Parse(baseURL)
	if err != nil || (parsed.Scheme != "http" && parsed.Scheme != "https") || parsed.Host == "" {
		return nil, fmt.Errorf("%w: base URL must be http or https: %q", ErrConfig, baseURL)
	}
	client := &Client{
		baseURL:    strings.TrimRight(baseURL, "/"),
		apiKey:     apiKey,
		maxRetries: 2,
		timeout:    120 * time.Second,
		http:       &http.Client{},
		sleep:      sleepContext,

		maxResponseBytes: defaultMaxResponseBytes,
	}
	for _, option := range options {
		option(client)
	}
	if client.maxRetries < 0 {
		return nil, fmt.Errorf("%w: max retries must be 0 or more", ErrConfig)
	}
	if client.maxResponseBytes < 1 {
		return nil, fmt.Errorf("%w: max response bytes must be positive", ErrConfig)
	}
	return client, nil
}

// String redacts the API key.
func (c *Client) String() string {
	return fmt.Sprintf("gateway.Client{baseURL: %q, apiKey: <redacted>, maxRetries: %d}", c.baseURL, c.maxRetries)
}

// GoString redacts the API key from %#v.
func (c *Client) GoString() string { return c.String() }

func sleepContext(ctx context.Context, d time.Duration) error {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

type callOptions struct {
	idempotencyKey string
	requestID      string
}

// CallOption configures one call.
type CallOption func(*callOptions)

// WithIdempotencyKey sets the call's Idempotency-Key (1 to 255 characters).
// Left unset, the client generates one per call and reuses it for that
// call's retries. Set it to make your own retries of an operation safe
// across process restarts.
func WithIdempotencyKey(key string) CallOption {
	return func(o *callOptions) { o.idempotencyKey = key }
}

// WithRequestID sends an X-Request-Id to correlate the call with your logs.
func WithRequestID(id string) CallOption { return func(o *callOptions) { o.requestID = id } }

// Chat creates a chat completion.
func (c *Client) Chat(ctx context.Context, request ChatRequest, options ...CallOption) (*ChatResponse, error) {
	response, err := c.send(ctx, chatPath, request, options, false)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	var out ChatResponse
	if err := decodeBody(response, &out, c.maxResponseBytes); err != nil {
		return nil, err
	}
	out.RequestID = response.Header.Get("X-Request-Id")
	return &out, nil
}

// Embeddings creates one embedding per input.
func (c *Client) Embeddings(ctx context.Context, request EmbeddingRequest, options ...CallOption) (*EmbeddingResponse, error) {
	response, err := c.send(ctx, embeddingsPath, request, options, false)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	var out EmbeddingResponse
	if err := decodeBody(response, &out, c.maxResponseBytes); err != nil {
		return nil, err
	}
	out.RequestID = response.Header.Get("X-Request-Id")
	return &out, nil
}

// ChatStream streams a chat completion. Read it with Next and Event until
// Next returns false, then check Err; always Close it.
func (c *Client) ChatStream(ctx context.Context, request ChatRequest, options ...CallOption) (*Stream, error) {
	response, err := c.send(ctx, streamPath, request, options, true)
	if err != nil {
		return nil, err
	}
	return &Stream{body: response.Body, RequestID: response.Header.Get("X-Request-Id")}, nil
}

func decodeBody(response *http.Response, out any, limit int64) error {
	// One byte past the limit shows the body is longer, without overflowing.
	readLimit := limit
	if readLimit < math.MaxInt64 {
		readLimit++
	}
	body, err := io.ReadAll(io.LimitReader(response.Body, readLimit))
	if err != nil {
		return err
	}
	if int64(len(body)) > limit {
		return &DecodeError{Err: fmt.Errorf("the response body exceeded %d bytes", limit)}
	}
	if err := json.Unmarshal(body, out); err != nil {
		return &DecodeError{Err: err}
	}
	return nil
}

func newKey() string {
	var raw [16]byte
	_, _ = rand.Read(raw[:])
	raw[6] = raw[6]&0x0f | 0x40
	raw[8] = raw[8]&0x3f | 0x80
	text := hex.EncodeToString(raw[:])
	return text[0:8] + "-" + text[8:12] + "-" + text[12:16] + "-" + text[16:20] + "-" + text[20:]
}

// send posts the request, retrying with the same key only when nothing was
// reserved, and returns the first successful response with its body open.
func (c *Client) send(ctx context.Context, path string, request any, options []CallOption, stream bool) (*http.Response, error) {
	var call callOptions
	for _, option := range options {
		option(&call)
	}
	key := call.idempotencyKey
	if key == "" {
		key = newKey()
	} else if len([]rune(key)) > maxKeyChars {
		return nil, fmt.Errorf("%w: an Idempotency-Key must be 1 to %d characters", ErrConfig, maxKeyChars)
	}
	if len([]rune(call.requestID)) > maxRequestIDChars {
		return nil, fmt.Errorf("%w: an X-Request-Id must be at most %d characters", ErrConfig, maxRequestIDChars)
	}
	body, err := json.Marshal(request)
	if err != nil {
		return nil, fmt.Errorf("%w: %v", ErrConfig, err)
	}
	for retries := 0; ; retries++ {
		attemptCtx, cancel := context.WithCancel(ctx)
		// A stream's timer stops once its response started; a non-streaming
		// call's runs until its body is read.
		var timer *time.Timer
		if c.timeout > 0 {
			if stream {
				timer = time.AfterFunc(c.timeout, cancel)
			} else {
				var cancelTimeout context.CancelFunc
				attemptCtx, cancelTimeout = context.WithTimeout(attemptCtx, c.timeout)
				cancelOuter := cancel
				cancel = func() { cancelTimeout(); cancelOuter() }
			}
		}
		// started stops a stream's timer and reports whether it had fired.
		started := func() bool { return timer != nil && !timer.Stop() }
		httpRequest, err := http.NewRequestWithContext(attemptCtx, http.MethodPost, c.baseURL+path, bytes.NewReader(body))
		if err != nil {
			cancel()
			return nil, fmt.Errorf("%w: %v", ErrConfig, err)
		}
		httpRequest.Header.Set("Authorization", "Bearer "+c.apiKey)
		httpRequest.Header.Set("Content-Type", "application/json")
		httpRequest.Header.Set("Idempotency-Key", key)
		httpRequest.Header.Set("User-Agent", "qucelerate-ai-gateway-go/"+Version)
		if call.requestID != "" {
			httpRequest.Header.Set("X-Request-Id", call.requestID)
		}
		response, err := c.http.Do(httpRequest)
		if err != nil {
			fired := started()
			cancel()
			if fired {
				return nil, fmt.Errorf("%w (%v)", ErrTimeout, c.timeout)
			}
			return nil, err
		}
		if response.StatusCode >= 200 && response.StatusCode < 300 {
			if started() {
				response.Body.Close()
				cancel()
				return nil, fmt.Errorf("%w (%v)", ErrTimeout, c.timeout)
			}
			response.Body = &cancelOnClose{ReadCloser: response.Body, cancel: cancel}
			return response, nil
		}
		failure := errorFrom(response)
		fired := started()
		cancel()
		if fired {
			return nil, fmt.Errorf("%w (%v)", ErrTimeout, c.timeout)
		}
		problem, ok := failure.(*ProblemError)
		if !ok || !sameKeyRetryCodes[problem.Code] || retries >= c.maxRetries {
			return nil, failure
		}
		wait := retryWait(problem.RetryAfter)
		if err := c.sleep(ctx, wait); err != nil {
			return nil, err
		}
	}
}

// retryWait is Retry-After capped at maxWait, or 1 second without one. The
// cap applies to the seconds, so a huge header cannot overflow the duration.
func retryWait(seconds int) time.Duration {
	switch {
	case seconds < 0:
		return time.Second
	case seconds > int(maxWait/time.Second):
		return maxWait
	default:
		return time.Duration(seconds) * time.Second
	}
}

type cancelOnClose struct {
	io.ReadCloser
	cancel context.CancelFunc
}

func (c *cancelOnClose) Close() error {
	err := c.ReadCloser.Close()
	c.cancel()
	return err
}

func errorFrom(response *http.Response) error {
	defer response.Body.Close()
	body, err := io.ReadAll(io.LimitReader(response.Body, maxErrorBodyBytes))
	if err != nil {
		return fmt.Errorf("reading the %d response: %w", response.StatusCode, err)
	}
	retryAfter := -1
	if value, err := strconv.Atoi(strings.TrimSpace(response.Header.Get("Retry-After"))); err == nil && value >= 0 {
		retryAfter = value
	}
	mediaType, _, _ := mime.ParseMediaType(response.Header.Get("Content-Type"))
	if strings.HasSuffix(mediaType, "json") {
		var problem ProblemError
		if json.Unmarshal(body, &problem) == nil && problem.Code != "" {
			problem.Status = response.StatusCode
			problem.RetryAfter = retryAfter
			return &problem
		}
	}
	return &HTTPError{Status: response.StatusCode, RequestID: response.Header.Get("X-Request-Id")}
}

// Stream is a streamed chat completion.
type Stream struct {
	// RequestID is the X-Request-Id response header.
	RequestID string
	body      io.ReadCloser
	parser    sseParser
	pending   []Event
	current   Event
	failure   error
	done      bool
	buffer    [4096]byte
}

// Next advances to the next event and reports whether there is one. After
// it returns false, Err reports why: nil after response.completed.
func (s *Stream) Next() bool {
	for {
		if len(s.pending) > 0 {
			s.current, s.pending = s.pending[0], s.pending[1:]
			return true
		}
		if s.done {
			s.current = nil
			s.Close()
			return false
		}
		n, err := s.body.Read(s.buffer[:])
		if n > 0 {
			s.accept(s.buffer[:n])
		}
		if err == io.EOF && !s.done {
			s.done = true
			s.failure = fmt.Errorf("%w", ErrProtocol)
		} else if err != nil && err != io.EOF && !s.done {
			s.done = true
			s.failure = err
		}
	}
}

func (s *Stream) accept(chunk []byte) {
	raws, oversized := s.parser.feed(chunk)
	defer func() {
		if oversized && !s.done {
			s.done = true
			s.failure = fmt.Errorf("%w: an event exceeded %d bytes", ErrProtocol, maxEventBytes)
		}
	}()
	for _, raw := range raws {
		if s.done {
			return
		}
		if raw.name == "error" {
			s.done = true
			s.failure = streamError(raw.data)
			return
		}
		event, err := decodeEvent(raw.name, []byte(raw.data))
		if err != nil {
			s.done = true
			s.failure = &DecodeError{Err: fmt.Errorf("%s: %w", raw.name, err)}
			return
		}
		if event == nil {
			continue
		}
		s.pending = append(s.pending, event)
		if _, ok := event.(ResponseCompleted); ok {
			s.done = true
		}
	}
}

// streamError is the terminal error event's *StreamError, or a *DecodeError
// when it is malformed.
func streamError(data string) error {
	var payload struct {
		Code   *string `json:"code"`
		Status *int    `json:"status"`
	}
	if err := json.Unmarshal([]byte(data), &payload); err != nil {
		return &DecodeError{Err: fmt.Errorf("error event: %w", err)}
	}
	if payload.Code == nil || payload.Status == nil {
		return &DecodeError{Err: errors.New("error event: code and status are required")}
	}
	return &StreamError{Code: *payload.Code, Status: *payload.Status}
}

// Event is the event Next advanced to.
func (s *Stream) Event() Event { return s.current }

// Err is why the stream ended: nil after response.completed, a *StreamError
// for a terminal error event, ErrProtocol (wrapped) for an early close or an
// event over 16 MiB.
func (s *Stream) Err() error { return s.failure }

// Close closes the connection. It is safe to call more than once.
func (s *Stream) Close() error { return s.body.Close() }
