package translate

import "encoding/json"

// The OpenAI-shaped request this package writes. Field shapes are
// cross-checked against ollama/openai.ChatCompletionRequest/Message/ToolCall;
// this package only ever sets the subset below.

type openAIRequestOut struct {
	Model             string               `json:"model"`
	Stream            bool                 `json:"stream"`
	StreamOptions     *openAIStreamOptions `json:"stream_options,omitempty"`
	Messages          []openAIMessageOut   `json:"messages"`
	MaxTokens         json.RawMessage      `json:"max_tokens,omitempty"`
	Tools             []openAIToolOut      `json:"tools,omitempty"`
	ToolChoice        json.RawMessage      `json:"tool_choice,omitempty"`
	ParallelToolCalls *bool                `json:"parallel_tool_calls,omitempty"`
	Temperature       json.RawMessage      `json:"temperature,omitempty"`
	TopP              json.RawMessage      `json:"top_p,omitempty"`
	Stop              json.RawMessage      `json:"stop,omitempty"`
}

type openAIStreamOptions struct {
	IncludeUsage bool `json:"include_usage"`
}

// openAIMessageOut is one OpenAI chat message. Content has no omitempty: it
// is always present, as a string or (for an assistant turn that only called
// tools) an explicit null. ToolCalls and ToolCallID are only ever set on the
// branch that needs them, so omitempty is what keeps them off every other
// message.
type openAIMessageOut struct {
	Role       string          `json:"role"`
	Content    any             `json:"content"`
	ToolCalls  []openAICallOut `json:"tool_calls,omitempty"`
	ToolCallID string          `json:"tool_call_id,omitempty"`
}

type openAICallOut struct {
	ID       string                `json:"id"`
	Type     string                `json:"type"`
	Function openAIFunctionCallOut `json:"function"`
}

type openAIFunctionCallOut struct {
	Name string `json:"name"`
	// Arguments carries the call's input serialized to a JSON string:
	// OpenAI wants it that way, where Anthropic carries the object itself.
	Arguments string `json:"arguments"`
}

type openAIToolOut struct {
	Type     string               `json:"type"`
	Function openAIFunctionDefOut `json:"function"`
}

type openAIFunctionDefOut struct {
	Name        string          `json:"name"`
	Description string          `json:"description,omitempty"`
	Parameters  json.RawMessage `json:"parameters"`
}

// The OpenAI-shaped input this package reads: a whole chat completion, and
// one stream chunk. Cross-checked against ollama/openai.ChatCompletion and
// ChatCompletionChunk for the field set.

type openAIResponseIn struct {
	ID      string           `json:"id"`
	Choices []openAIChoiceIn `json:"choices"`
	Usage   *openAIUsageIn   `json:"usage"`
}

type openAIChoiceIn struct {
	Message      openAIMessageIn `json:"message"`
	FinishReason string          `json:"finish_reason"`
}

// openAIMessageIn is the message on a buffered (non-streaming) response.
// Content is a plain string: OpenAI sends null for the content of a turn
// that only called tools, and unmarshaling null into a string leaves it "",
// which is exactly the empty-text reading this package wants.
type openAIMessageIn struct {
	Content   string         `json:"content"`
	ToolCalls []openAICallIn `json:"tool_calls"`
}

type openAICallIn struct {
	ID       string               `json:"id"`
	Function openAIFunctionCallIn `json:"function"`
}

type openAIFunctionCallIn struct {
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

type openAIUsageIn struct {
	PromptTokens        int                    `json:"prompt_tokens"`
	PromptTokensDetails *openAIPromptDetailsIn `json:"prompt_tokens_details,omitempty"`
	CompletionTokens    int                    `json:"completion_tokens"`
}

// openAIPromptDetailsIn carries the cached-prefix count an endpoint may
// report as part of prompt_tokens_details. PromptTokens above is the total
// (matching ollama/openai.Usage), so CachedTokens is a subset of it, not an
// addition.
type openAIPromptDetailsIn struct {
	CachedTokens int `json:"cached_tokens"`
}

type openAIChunkIn struct {
	ID      string                `json:"id"`
	Choices []openAIChunkChoiceIn `json:"choices"`
	Usage   *openAIUsageIn        `json:"usage"`
}

type openAIChunkChoiceIn struct {
	Delta        openAIDeltaIn `json:"delta"`
	FinishReason string        `json:"finish_reason"`
}

// openAIDeltaIn is one chunk's incremental update. A reasoning model's
// thinking, separate from and usually well before Content, arrives under
// either field depending on the endpoint: GLM and DeepSeek-style upstreams
// use reasoning_content, Ollama's own /v1 endpoint and some others use
// reasoning. Both are read; reasoningText picks whichever is set.
type openAIDeltaIn struct {
	Content          string                  `json:"content"`
	ReasoningContent string                  `json:"reasoning_content"`
	Reasoning        string                  `json:"reasoning"`
	ToolCalls        []openAIToolCallDeltaIn `json:"tool_calls"`
}

// reasoningText returns whichever reasoning field the endpoint set.
func (d openAIDeltaIn) reasoningText() string {
	if d.ReasoningContent != "" {
		return d.ReasoningContent
	}
	return d.Reasoning
}

// openAIToolCallDeltaIn is one fragment of one tool call. Index is a pointer
// because its presence, not its value, is what tells apart an endpoint that
// numbers its calls from one that sends a call whole keyed only by id: a
// zero-value int would read as "index 0" instead of "no index sent".
type openAIToolCallDeltaIn struct {
	Index    *int                 `json:"index"`
	ID       string               `json:"id"`
	Function openAIFunctionCallIn `json:"function"`
}
