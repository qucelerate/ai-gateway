package gateway

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"mime/multipart"
	"net/http"
	"net/textproto"
	"net/url"
	"strconv"
	"strings"
)

const (
	filesPath   = "/api/inference/v1/files"
	batchesPath = "/api/inference/v1/batches"
)

// The endpoints a batch's lines may call.
const (
	BatchEndpointChat       = "/v1/chat/completions"
	BatchEndpointEmbeddings = "/v1/embeddings"
)

// FileObject is an uploaded batch input file, or a batch's output or error
// file.
type FileObject struct {
	ID        string `json:"id"`
	Bytes     int64  `json:"bytes"`
	Filename  string `json:"filename"`
	Purpose   string `json:"purpose"`
	CreatedAt int64  `json:"created_at"`
}

// BatchRequest creates a batch from an uploaded file. Endpoint is
// BatchEndpointChat or BatchEndpointEmbeddings.
type BatchRequest struct {
	InputFileID string            `json:"input_file_id"`
	Endpoint    string            `json:"endpoint"`
	Metadata    map[string]string `json:"metadata,omitempty"`
}

// BatchCounts are a batch's line counts.
type BatchCounts struct {
	Total     int64 `json:"total"`
	Completed int64 `json:"completed"`
	Failed    int64 `json:"failed"`
}

// Batch is OpenAI's batch object. Status is an open string (validating,
// in_progress, finalizing, completed, failed, expired, cancelling,
// cancelled). Gateway is the gateway's own account, on a single-batch read:
// each job's mode (native or emulated) and price_source, and the cost per
// currency.
type Batch struct {
	ID            string            `json:"id"`
	Status        string            `json:"status"`
	Endpoint      string            `json:"endpoint"`
	InputFileID   string            `json:"input_file_id"`
	OutputFileID  *string           `json:"output_file_id"`
	ErrorFileID   *string           `json:"error_file_id"`
	CreatedAt     int64             `json:"created_at"`
	ExpiresAt     int64             `json:"expires_at"`
	RequestCounts BatchCounts       `json:"request_counts"`
	Metadata      map[string]string `json:"metadata"`
	Gateway       json.RawMessage   `json:"gateway,omitempty"`
	RequestID     string            `json:"request_id,omitempty"`
}

// Done reports whether the batch has ended: its output and error files are
// final.
func (b *Batch) Done() bool {
	switch b.Status {
	case "completed", "failed", "expired", "cancelled":
		return true
	}
	return false
}

// BatchList is a page of batches, newest first.
type BatchList struct {
	Data    []Batch `json:"data"`
	HasMore bool    `json:"has_more"`
}

// UploadBatchFile uploads a batch input file: JSONL, one request per line,
// each {"custom_id", "method": "POST", "url", "body"}.
func (c *Client) UploadBatchFile(ctx context.Context, filename string, content []byte, options ...CallOption) (*FileObject, error) {
	if filename == "" || strings.ContainsAny(filename, "\"\r\n") {
		return nil, fmt.Errorf("%w: not a file name: %q", ErrConfig, filename)
	}
	var body bytes.Buffer
	form := multipart.NewWriter(&body)
	if err := form.WriteField("purpose", "batch"); err != nil {
		return nil, err
	}
	header := textproto.MIMEHeader{}
	header.Set("Content-Disposition", fmt.Sprintf(`form-data; name="file"; filename="%s"`, filename))
	header.Set("Content-Type", "application/jsonl")
	part, err := form.CreatePart(header)
	if err != nil {
		return nil, err
	}
	if _, err := part.Write(content); err != nil {
		return nil, err
	}
	if err := form.Close(); err != nil {
		return nil, err
	}
	var out FileObject
	if _, err := c.plain(ctx, http.MethodPost, filesPath, form.FormDataContentType(), body.Bytes(), false, options, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// CreateBatch creates a batch from an uploaded file. The whole batch is
// checked, routed and reserved when it is created, or refused with nothing
// held. Retry WithIdempotencyKey to get the same batch.
func (c *Client) CreateBatch(ctx context.Context, request BatchRequest, options ...CallOption) (*Batch, error) {
	if request.Endpoint != BatchEndpointChat && request.Endpoint != BatchEndpointEmbeddings {
		return nil, fmt.Errorf("%w: endpoint must be %s or %s", ErrConfig, BatchEndpointChat, BatchEndpointEmbeddings)
	}
	body, err := json.Marshal(struct {
		BatchRequest
		CompletionWindow string `json:"completion_window"`
	}{request, "24h"})
	if err != nil {
		return nil, fmt.Errorf("%w: %v", ErrConfig, err)
	}
	return c.batch(ctx, http.MethodPost, batchesPath, body, true, options)
}

// Batch reads a batch, with the gateway's account of how it runs and its
// cost.
func (c *Client) Batch(ctx context.Context, batchID string, options ...CallOption) (*Batch, error) {
	if err := checkID(batchID); err != nil {
		return nil, err
	}
	return c.batch(ctx, http.MethodGet, batchesPath+"/"+batchID, nil, false, options)
}

// CancelBatch asks a batch to stop: lines not yet run end without a charge.
func (c *Client) CancelBatch(ctx context.Context, batchID string, options ...CallOption) (*Batch, error) {
	if err := checkID(batchID); err != nil {
		return nil, err
	}
	return c.batch(ctx, http.MethodPost, batchesPath+"/"+batchID+"/cancel", nil, false, options)
}

// ListBatches lists the API key's service account's batches, newest first:
// at most limit (0 for the gateway's default), after the batch after ("" for
// the first page).
func (c *Client) ListBatches(ctx context.Context, after string, limit int, options ...CallOption) (*BatchList, error) {
	query := url.Values{}
	if after != "" {
		query.Set("after", after)
	}
	if limit > 0 {
		query.Set("limit", strconv.Itoa(limit))
	}
	path := batchesPath
	if len(query) > 0 {
		path += "?" + query.Encode()
	}
	var out BatchList
	if _, err := c.plain(ctx, http.MethodGet, path, "", nil, false, options, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// FileContent reads a file's content: a batch's output or error file is
// JSONL, one line per request line, matched by custom_id.
func (c *Client) FileContent(ctx context.Context, fileID string, options ...CallOption) ([]byte, error) {
	if err := checkID(fileID); err != nil {
		return nil, err
	}
	var out []byte
	if _, err := c.plain(ctx, http.MethodGet, filesPath+"/"+fileID+"/content", "", nil, false, options, &out); err != nil {
		return nil, err
	}
	return out, nil
}

func (c *Client) batch(ctx context.Context, method, path string, body []byte, write bool, options []CallOption) (*Batch, error) {
	contentType := ""
	if body != nil {
		contentType = "application/json"
	}
	var out Batch
	header, err := c.plain(ctx, method, path, contentType, body, write, options, &out)
	if err != nil {
		return nil, err
	}
	out.RequestID = header.Get("X-Request-Id")
	return &out, nil
}

// checkID refuses an id that could name another path.
func checkID(id string) error {
	if id == "" {
		return fmt.Errorf("%w: an id is required", ErrConfig)
	}
	for _, r := range id {
		if !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '_' || r == '-') {
			return fmt.Errorf("%w: not an id: %q", ErrConfig, id)
		}
	}
	return nil
}

// plain sends a files or batches call, retried only as send retries, and
// decodes its body into out (a *[]byte takes the raw body). A write that
// creates something carries an Idempotency-Key.
func (c *Client) plain(ctx context.Context, method, path, contentType string, body []byte, write bool, options []CallOption, out any) (http.Header, error) {
	var call callOptions
	for _, option := range options {
		option(&call)
	}
	key := call.idempotencyKey
	if write && key == "" {
		key = newKey()
	} else if len([]rune(key)) > maxKeyChars {
		return nil, fmt.Errorf("%w: an Idempotency-Key must be 1 to %d characters", ErrConfig, maxKeyChars)
	}
	if len([]rune(call.requestID)) > maxRequestIDChars {
		return nil, fmt.Errorf("%w: an X-Request-Id must be at most %d characters", ErrConfig, maxRequestIDChars)
	}
	for retries := 0; ; retries++ {
		attemptCtx, cancel := ctx, context.CancelFunc(func() {})
		if c.timeout > 0 {
			attemptCtx, cancel = context.WithTimeout(ctx, c.timeout)
		}
		var reader io.Reader
		if body != nil {
			reader = bytes.NewReader(body)
		}
		request, err := http.NewRequestWithContext(attemptCtx, method, c.baseURL+path, reader)
		if err != nil {
			cancel()
			return nil, fmt.Errorf("%w: %v", ErrConfig, err)
		}
		request.Header.Set("Authorization", "Bearer "+c.apiKey)
		request.Header.Set("User-Agent", "qucelerate-ai-gateway-go/"+Version)
		if contentType != "" {
			request.Header.Set("Content-Type", contentType)
		}
		if write {
			request.Header.Set("Idempotency-Key", key)
		}
		if call.requestID != "" {
			request.Header.Set("X-Request-Id", call.requestID)
		}
		response, err := c.http.Do(request)
		if err != nil {
			cancel()
			return nil, err
		}
		if response.StatusCode >= 200 && response.StatusCode < 300 {
			defer cancel()
			defer response.Body.Close()
			if raw, ok := out.(*[]byte); ok {
				limit := c.maxResponseBytes
				if limit < math.MaxInt64 {
					limit++
				}
				content, err := io.ReadAll(io.LimitReader(response.Body, limit))
				if err != nil {
					return nil, err
				}
				if int64(len(content)) > c.maxResponseBytes {
					return nil, &DecodeError{Err: fmt.Errorf("the response body exceeded %d bytes", c.maxResponseBytes)}
				}
				*raw = content
				return response.Header, nil
			}
			return response.Header, decodeBody(response, out, c.maxResponseBytes)
		}
		failure := errorFrom(response)
		cancel()
		problem, ok := failure.(*ProblemError)
		if !ok || !sameKeyRetryCodes[problem.Code] || retries >= c.maxRetries {
			return nil, failure
		}
		if err := c.sleep(ctx, retryWait(problem.RetryAfter)); err != nil {
			return nil, err
		}
	}
}
