package gateway

import (
	"errors"
	"fmt"
)

// ProblemError is an RFC 9457 problem the gateway returned. Code is an open
// string: match the codes you handle and fall back to Status for the rest.
// clients/conformance/errors.json lists the codes of contract 1.0.0 and what
// each means for a retry.
type ProblemError struct {
	Status    int    `json:"-"`
	Type      string `json:"type"`
	Code      string `json:"code"`
	Title     string `json:"title"`
	Detail    string `json:"detail"`
	Instance  string `json:"instance"`
	RequestID string `json:"request_id"`
	// RetryAfter is the Retry-After header in whole seconds, or -1 when the
	// response had none.
	RetryAfter int `json:"-"`
}

func (e *ProblemError) Error() string {
	return fmt.Sprintf("gateway problem %d: %s (%s; request %s)", e.Status, e.Code, e.Title, e.RequestID)
}

// HTTPError is an error response without a problem body, for example from a
// proxy.
type HTTPError struct {
	Status    int
	RequestID string
}

func (e *HTTPError) Error() string { return fmt.Sprintf("HTTP %d without a problem body", e.Status) }

// StreamError is a stream's terminal error event. The request reached the
// provider, so it is never retried automatically.
type StreamError struct {
	Code   string
	Status int
}

func (e *StreamError) Error() string {
	return fmt.Sprintf("stream failed: %s (status %d)", e.Code, e.Status)
}

// ErrProtocol is returned (wrapped) when a stream closes before its terminal
// event. Streams are not resumable.
var ErrProtocol = errors.New("stream closed before a terminal event")

// ErrTimeout is returned (wrapped) when a stream's response did not start
// within the client's timeout.
var ErrTimeout = errors.New("the stream did not start within the timeout")

// ErrConfig is returned (wrapped) for an invalid client or call option.
var ErrConfig = errors.New("invalid configuration")

// DecodeError says a response body did not match the contract.
type DecodeError struct{ Err error }

func (e *DecodeError) Error() string { return "could not decode the response: " + e.Err.Error() }
func (e *DecodeError) Unwrap() error { return e.Err }
