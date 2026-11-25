package gateway

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

const testBatch = `{"id":"batch_1","object":"batch","endpoint":"/v1/chat/completions","input_file_id":"file_1","completion_window":"24h","status":"completed","output_file_id":"file_2","error_file_id":null,"created_at":1,"expires_at":2,"request_counts":{"total":2,"completed":2,"failed":0},"metadata":{},"gateway":{"jobs":[{"mode":"native"}]},"new_field":1}`

type seenRequest struct {
	method, uri, key, contentType, body string
}

func batchServer(t *testing.T, seen *[]seenRequest) *Client {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		*seen = append(*seen, seenRequest{r.Method, r.URL.RequestURI(), r.Header.Get("Idempotency-Key"), r.Header.Get("Content-Type"), string(body)})
		w.Header().Set("Content-Type", "application/json")
		switch {
		case r.URL.Path == filesPath:
			_, _ = w.Write([]byte(`{"id":"file_1","object":"file","bytes":3,"filename":"b.jsonl","purpose":"batch","created_at":1}`))
		case strings.HasSuffix(r.URL.Path, "/content"):
			_, _ = w.Write([]byte("{\"custom_id\":\"a\"}\n"))
		case r.URL.Path == batchesPath && r.Method == http.MethodGet:
			_, _ = w.Write([]byte(`{"object":"list","data":[` + testBatch + `],"has_more":true}`))
		case r.URL.Path == batchesPath+"/missing":
			w.Header().Set("Content-Type", "application/problem+json")
			w.WriteHeader(http.StatusNotFound)
			_, _ = w.Write([]byte(`{"code":"batch_not_found","status":404}`))
		default:
			_, _ = w.Write([]byte(testBatch))
		}
	}))
	t.Cleanup(server.Close)
	client, err := NewClient(server.URL, "gw_key")
	if err != nil {
		t.Fatal(err)
	}
	return client
}

func TestBatchCalls(t *testing.T) {
	var seen []seenRequest
	client := batchServer(t, &seen)
	ctx := context.Background()

	file, err := client.UploadBatchFile(ctx, "b.jsonl", []byte("{}\n"))
	if err != nil || file.ID != "file_1" {
		t.Fatalf("upload: %v %+v", err, file)
	}
	if !strings.HasPrefix(seen[0].contentType, "multipart/form-data") || !strings.Contains(seen[0].body, `name="purpose"`) || seen[0].key != "" {
		t.Fatalf("upload request: %+v", seen[0])
	}

	batch, err := client.CreateBatch(ctx, BatchRequest{InputFileID: "file_1", Endpoint: BatchEndpointChat, Metadata: map[string]string{"run": "a"}}, WithIdempotencyKey("k-1"))
	if err != nil || !batch.Done() || batch.RequestCounts.Completed != 2 || *batch.OutputFileID != "file_2" {
		t.Fatalf("create: %v %+v", err, batch)
	}
	var sent map[string]any
	_ = json.Unmarshal([]byte(seen[1].body), &sent)
	if seen[1].key != "k-1" || sent["completion_window"] != "24h" || sent["endpoint"] != BatchEndpointChat {
		t.Fatalf("create request: %+v", seen[1])
	}

	list, err := client.ListBatches(ctx, "batch_0", 5)
	if err != nil || !list.HasMore || len(list.Data) != 1 || seen[2].uri != batchesPath+"?after=batch_0&limit=5" {
		t.Fatalf("list: %v %+v %+v", err, list, seen[2])
	}
	if _, err := client.CancelBatch(ctx, "batch_1"); err != nil || seen[3].uri != batchesPath+"/batch_1/cancel" || seen[3].method != http.MethodPost || seen[3].key != "" {
		t.Fatalf("cancel: %v %+v", err, seen[3])
	}
	content, err := client.FileContent(ctx, "file_2")
	if err != nil || string(content) != "{\"custom_id\":\"a\"}\n" {
		t.Fatalf("content: %v %q", err, content)
	}

	var problem *ProblemError
	if _, err := client.Batch(ctx, "missing"); !errors.As(err, &problem) || problem.Code != "batch_not_found" {
		t.Fatalf("missing: %v", err)
	}
	if _, err := client.Batch(ctx, "../files"); !errors.Is(err, ErrConfig) {
		t.Fatalf("bad id: %v", err)
	}
	if _, err := client.CreateBatch(ctx, BatchRequest{InputFileID: "file_1", Endpoint: "/v1/responses"}); !errors.Is(err, ErrConfig) {
		t.Fatalf("bad endpoint: %v", err)
	}
}
