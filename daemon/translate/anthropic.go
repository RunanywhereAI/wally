package translate

import "encoding/json"

// anthropicBlock is one Anthropic content block, keeping only the fields this
// package reads or writes. Cross-checked against ollama/anthropic.ContentBlock
// for the field set; this package parses a narrower slice of it because
// nothing here serves vision, citations or server-side tools.
type anthropicBlock struct {
	Type      string          `json:"type"`
	Text      string          `json:"text,omitempty"`
	ID        string          `json:"id,omitempty"`
	Name      string          `json:"name,omitempty"`
	Input     json.RawMessage `json:"input,omitempty"`
	ToolUseID string          `json:"tool_use_id,omitempty"`
	// Content is a tool_result block's own nested content: a string or
	// another array of blocks, the same polymorphism as the outer message.
	Content anthropicContent `json:"content,omitempty"`
}

// anthropicContent is Anthropic content, a bare string or a list of typed
// blocks. A bare string normalizes to a single text block on unmarshal, so
// every caller only ever walks blocks. An unrecognised shape decodes to no
// blocks rather than failing the message: a client sending something this
// package did not anticipate should lose that field, not its turn.
type anthropicContent []anthropicBlock

func (c *anthropicContent) UnmarshalJSON(data []byte) error {
	var s string
	if err := json.Unmarshal(data, &s); err == nil {
		*c = anthropicContent{{Type: "text", Text: s}}
		return nil
	}
	var blocks []anthropicBlock
	if err := json.Unmarshal(data, &blocks); err == nil {
		*c = blocks
		return nil
	}
	*c = nil
	return nil
}

// text is the concatenation of this content's text blocks, the only part of
// Anthropic content an OpenAI endpoint can take. An image block would need
// to become an OpenAI image_url part, and nothing downstream serves vision,
// so it is dropped rather than given an invented shape.
func (c anthropicContent) text() string {
	out := ""
	for _, block := range c {
		if block.Type == "text" {
			out += block.Text
		}
	}
	return out
}

// chars counts what actually crosses the wire toward EstimateRequestTokens:
// text blocks, a tool_result's nested content, and a tool_use's serialized
// input. text() alone undercounts a coding turn where tool results and call
// arguments carry the bulk (file contents, command output, edit payloads).
func (c anthropicContent) chars() int {
	n := 0
	for _, block := range c {
		switch block.Type {
		case "text":
			n += len(block.Text)
		case "tool_result":
			n += block.Content.chars()
		case "tool_use":
			n += len(block.Input)
		}
	}
	return n
}

// anthropicMessage is one turn in an Anthropic messages array.
type anthropicMessage struct {
	Role    string           `json:"role"`
	Content anthropicContent `json:"content"`
}

// anthropicTool is a client tool definition. Only tools carrying an
// input_schema are client tools; Anthropic's server-side tools (web search
// and the rest) have no schema to forward and are filtered out by the
// caller checking InputSchema for nil.
type anthropicTool struct {
	Name        string          `json:"name"`
	Description string          `json:"description,omitempty"`
	InputSchema json.RawMessage `json:"input_schema,omitempty"`
}

// anthropicToolChoice is Anthropic's tool_choice when it arrives as an
// object. It can also arrive as a bare string ("auto", "none", "any"),
// handled separately in toolChoiceToOpenAI.
type anthropicToolChoice struct {
	Type                   string `json:"type"`
	Name                   string `json:"name"`
	DisableParallelToolUse bool   `json:"disable_parallel_tool_use"`
}

// The Anthropic-shaped output this package writes: a whole message, and the
// streaming events built from it. Field shapes are cross-checked against
// ollama/anthropic.go's MessagesResponse/ContentBlock/streaming event types.

// anthropicMessageOut is a whole Anthropic message, the shape ResponseToAnthropic
// returns and message_start's "message" field carries.
type anthropicMessageOut struct {
	ID           string              `json:"id"`
	Type         string              `json:"type"`
	Role         string              `json:"role"`
	Model        string              `json:"model"`
	Content      []anthropicBlockOut `json:"content"`
	StopReason   *string             `json:"stop_reason"`
	StopSequence *string             `json:"stop_sequence"`
	Usage        anthropicUsageOut   `json:"usage"`
}

// anthropicBlockOut is one block of Anthropic output. Text uses a pointer so
// it serializes as present (even "") only for a block this package actually
// set it on; a tool_use block never sets it, so the key is absent rather
// than an empty string sitting next to id/name/input.
type anthropicBlockOut struct {
	Type  string          `json:"type"`
	Text  *string         `json:"text,omitempty"`
	ID    string          `json:"id,omitempty"`
	Name  string          `json:"name,omitempty"`
	Input json.RawMessage `json:"input,omitempty"`
}

func textBlock(text string) anthropicBlockOut {
	return anthropicBlockOut{Type: "text", Text: &text}
}

type anthropicUsageOut struct {
	InputTokens          int  `json:"input_tokens"`
	CacheReadInputTokens *int `json:"cache_read_input_tokens,omitempty"`
	OutputTokens         int  `json:"output_tokens"`
}

type anthropicDeltaOut struct {
	Type        string `json:"type"`
	Text        string `json:"text,omitempty"`
	PartialJSON string `json:"partial_json,omitempty"`
}

type messageStartEvent struct {
	Type    string              `json:"type"`
	Message anthropicMessageOut `json:"message"`
}

type contentBlockStartEvent struct {
	Type         string            `json:"type"`
	Index        int               `json:"index"`
	ContentBlock anthropicBlockOut `json:"content_block"`
}

type contentBlockDeltaEvent struct {
	Type  string            `json:"type"`
	Index int               `json:"index"`
	Delta anthropicDeltaOut `json:"delta"`
}

type contentBlockStopEvent struct {
	Type  string `json:"type"`
	Index int    `json:"index"`
}

type messageDeltaEvent struct {
	Type  string            `json:"type"`
	Delta messageDeltaBody  `json:"delta"`
	Usage anthropicUsageOut `json:"usage"`
}

type messageDeltaBody struct {
	StopReason   string  `json:"stop_reason"`
	StopSequence *string `json:"stop_sequence"`
}

type messageStopEvent struct {
	Type string `json:"type"`
}

type anthropicErrorBody struct {
	Type  string               `json:"type"`
	Error anthropicErrorDetail `json:"error"`
}

type anthropicErrorDetail struct {
	Type    string `json:"type"`
	Message string `json:"message"`
}
