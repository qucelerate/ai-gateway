package gateway

import "encoding/json"

// Message roles of a chat request.
const (
	RoleSystem    = "system"
	RoleDeveloper = "developer"
	RoleUser      = "user"
	RoleAssistant = "assistant"
	RoleTool      = "tool"
)

// Message is one message of a chat request.
type Message struct {
	Role    string `json:"role"`
	Content string `json:"content"`
	// ToolCalls are an assistant message's calls, sent back unchanged
	// before their results; an empty Content is then sent as null.
	ToolCalls []ToolCall `json:"tool_calls,omitempty"`
	// ToolCallID is a tool message's: the id of the call it answers.
	ToolCallID string `json:"tool_call_id,omitempty"`
}

// MarshalJSON sends an empty content as null on a message that only calls
// tools, as OpenAI documents.
func (m Message) MarshalJSON() ([]byte, error) {
	type plain Message
	if m.Content == "" && len(m.ToolCalls) > 0 {
		return json.Marshal(struct {
			Content *string `json:"content"`
			plain
		}{nil, plain(m)})
	}
	return json.Marshal(plain(m))
}

// ToolCall is a call the model made to one of the request's tools,
// OpenAI-shaped. Function.Arguments is a JSON object, serialized, as the
// model wrote it: validate it before acting on it.
type ToolCall struct {
	ID       string       `json:"id"`
	Type     string       `json:"type"`
	Function FunctionCall `json:"function"`
}

// FunctionCall is the function a ToolCall calls and its arguments.
type FunctionCall struct {
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

// Tool is a function the model may call. Parameters is a JSON Schema
// whose type is object; nil sends none (the gateway takes an empty object).
type Tool struct {
	Type     string             `json:"type"`
	Function FunctionDefinition `json:"function"`
}

// FunctionDefinition names and describes a Tool's function.
type FunctionDefinition struct {
	Name        string          `json:"name"`
	Description string          `json:"description,omitempty"`
	Parameters  json.RawMessage `json:"parameters,omitempty"`
}

// FunctionTool is a Tool calling the function name.
func FunctionTool(name, description string, parameters json.RawMessage) Tool {
	return Tool{Type: "function", Function: FunctionDefinition{name, description, parameters}}
}

// ToolChoice says whether the model must call tools: ToolChoiceAuto,
// ToolChoiceNone, ToolChoiceRequired or ToolChoiceFunction(name).
type ToolChoice struct{ value any }

var (
	ToolChoiceAuto     = ToolChoice{"auto"}
	ToolChoiceNone     = ToolChoice{"none"}
	ToolChoiceRequired = ToolChoice{"required"}
)

// ToolChoiceFunction makes the model call the function name.
func ToolChoiceFunction(name string) ToolChoice {
	return ToolChoice{map[string]any{"type": "function", "function": map[string]string{"name": name}}}
}

func (c ToolChoice) MarshalJSON() ([]byte, error) { return json.Marshal(c.value) }

func (c *ToolChoice) UnmarshalJSON(data []byte) error { return json.Unmarshal(data, &c.value) }

// ChatRequest is the body of a chat call. Temperature (0 to 2) and
// MaxTokens (1 to 4096) are optional; use Float and Int to set them.
type ChatRequest struct {
	Model       string    `json:"model"`
	Messages    []Message `json:"messages"`
	Temperature *float64  `json:"temperature,omitempty"`
	MaxTokens   *int      `json:"max_tokens,omitempty"`
	// Tools the model may call. A model alias that cannot take tools
	// refuses them (tools_unsupported), never drops them.
	Tools      []Tool      `json:"tools,omitempty"`
	ToolChoice *ToolChoice `json:"tool_choice,omitempty"`
	// ParallelToolCalls false asks for at most one call per response.
	ParallelToolCalls *bool `json:"parallel_tool_calls,omitempty"`
}

// EmbeddingRequest is the body of an embeddings call: 1 to 128 non-empty
// strings.
type EmbeddingRequest struct {
	Model string   `json:"model"`
	Input []string `json:"input"`
}

// Float returns a pointer to v, for optional request fields.
func Float(v float64) *float64 { return &v }

// Int returns a pointer to v, for optional request fields.
func Int(v int) *int { return &v }

// Bool returns a pointer to v, for optional request fields.
func Bool(v bool) *bool { return &v }

// Usage reports tokens. A counter the provider did not report is nil, never
// 0. ReasoningTokens is counted apart from OutputTokens; nil means the
// provider does not report reasoning, which is then inside OutputTokens.
// Quality is exact, estimated or unknown; Completeness is complete, partial
// or unavailable.
type Usage struct {
	InputTokens       *int64 `json:"input_tokens"`
	OutputTokens      *int64 `json:"output_tokens"`
	CachedInputTokens *int64 `json:"cached_input_tokens"`
	ReasoningTokens   *int64 `json:"reasoning_tokens"`
	OtherTokens       *int64 `json:"other_tokens"`
	Quality           string `json:"quality"`
	Completeness      string `json:"completeness"`
}

// MessageResponse is the assistant message of a choice. Content is empty
// when the model returned only tool calls or a refusal.
type MessageResponse struct {
	Role       string  `json:"role"`
	Content    string  `json:"content"`
	Name       *string `json:"name"`
	ToolCallID *string `json:"tool_call_id"`
	// ToolCalls are the calls the model made; empty when it made none.
	ToolCalls []ToolCall `json:"tool_calls,omitempty"`
}

// ToMessage is this response as the assistant message that continues the
// conversation, its tool calls included.
func (m MessageResponse) ToMessage() Message {
	return Message{Role: "assistant", Content: m.Content, ToolCalls: m.ToolCalls}
}

// Choice is one choice of a chat response. FinishReason is an open string
// (stop, length, tool_calls, ...): handle values you do not know.
type Choice struct {
	Index        int               `json:"index"`
	Message      MessageResponse   `json:"message"`
	FinishReason *string           `json:"finish_reason"`
	ToolCalls    []json.RawMessage `json:"tool_calls"`
	Refusal      *string           `json:"refusal"`
	Safety       []json.RawMessage `json:"safety"`
	Annotations  []json.RawMessage `json:"annotations"`
}

// ChatResponse is a chat completion. RequestID is the X-Request-Id response
// header: quote it when reporting a problem.
type ChatResponse struct {
	ID        string   `json:"id"`
	Model     string   `json:"model"`
	Choices   []Choice `json:"choices"`
	Usage     *Usage   `json:"usage"`
	RequestID string   `json:"request_id,omitempty"`
}

// Content is the first choice's message content, the usual answer.
func (r *ChatResponse) Content() string {
	if len(r.Choices) == 0 {
		return ""
	}
	return r.Choices[0].Message.Content
}

// Embedding is one embedding vector.
type Embedding struct {
	Object    string    `json:"object"`
	Index     int       `json:"index"`
	Embedding []float32 `json:"embedding"`
}

// EmbeddingResponse holds one vector per input, in input order.
type EmbeddingResponse struct {
	Object    string      `json:"object"`
	Model     string      `json:"model"`
	Data      []Embedding `json:"data"`
	Usage     *Usage      `json:"usage"`
	RequestID string      `json:"request_id,omitempty"`
}

// Event is an event of a chat stream. Events this version does not know are
// skipped; a terminal error event ends the stream with a *StreamError.
type Event interface {
	// EventName is the SSE event name.
	EventName() string
}

// ResponseStarted is the first event of a stream.
type ResponseStarted struct {
	Model string `json:"model"`
}

// ChoiceDelta carries more content or refusal text for a choice.
type ChoiceDelta struct {
	ChoiceIndex  int               `json:"choice_index"`
	ContentDelta *string           `json:"content_delta"`
	RefusalDelta *string           `json:"refusal_delta"`
	Annotations  []json.RawMessage `json:"annotations"`
}

// ToolCallDelta is part of a tool call; ID and Name arrive in its first
// delta only.
type ToolCallDelta struct {
	ChoiceIndex    int     `json:"choice_index"`
	ToolCallIndex  int     `json:"tool_call_index"`
	ID             *string `json:"id"`
	Name           *string `json:"name"`
	ArgumentsDelta string  `json:"arguments_delta"`
}

// ChoiceFinished says a choice is complete.
type ChoiceFinished struct {
	ChoiceIndex  int    `json:"choice_index"`
	FinishReason string `json:"finish_reason"`
}

// SafetyAnnotation is a provider safety signal for a choice.
type SafetyAnnotation struct {
	ChoiceIndex int    `json:"choice_index"`
	Category    string `json:"category"`
	Blocked     bool   `json:"blocked"`
}

// UsageEvent is the usage event.
type UsageEvent struct{ Usage }

// ResponseCompleted is the last event of a successful stream.
type ResponseCompleted struct {
	Model string `json:"model"`
	Usage *Usage `json:"usage"`
}

func (ResponseStarted) EventName() string   { return "response.started" }
func (ChoiceDelta) EventName() string       { return "choice.delta" }
func (ToolCallDelta) EventName() string     { return "tool_call.delta" }
func (ChoiceFinished) EventName() string    { return "choice.finished" }
func (SafetyAnnotation) EventName() string  { return "safety.annotation" }
func (UsageEvent) EventName() string        { return "usage" }
func (ResponseCompleted) EventName() string { return "response.completed" }

// decodeEvent returns the typed event, or nil for an event this version does
// not know.
func decodeEvent(name string, data []byte) (Event, error) {
	var event Event
	var err error
	switch name {
	case "response.started":
		var value ResponseStarted
		err = json.Unmarshal(data, &value)
		event = value
	case "choice.delta":
		var value ChoiceDelta
		err = json.Unmarshal(data, &value)
		event = value
	case "tool_call.delta":
		var value ToolCallDelta
		err = json.Unmarshal(data, &value)
		event = value
	case "choice.finished":
		var value ChoiceFinished
		err = json.Unmarshal(data, &value)
		event = value
	case "safety.annotation":
		var value SafetyAnnotation
		err = json.Unmarshal(data, &value)
		event = value
	case "usage":
		var value UsageEvent
		err = json.Unmarshal(data, &value.Usage)
		event = value
	case "response.completed":
		var value ResponseCompleted
		err = json.Unmarshal(data, &value)
		event = value
	default:
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return event, nil
}
