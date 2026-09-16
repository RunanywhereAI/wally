package translate

import (
	"encoding/json"
	"fmt"
)

// anthropicRequestIn is the top-level Anthropic request. Messages and Tools
// stay as raw elements, decoded one at a time in RequestToOpenAI: a single
// malformed entry should lose that entry, not fail the whole request, and a
// struct-typed slice would fail the whole array on one bad element.
type anthropicRequestIn struct {
	MaxTokens     json.RawMessage   `json:"max_tokens"`
	Messages      []json.RawMessage `json:"messages"`
	System        anthropicContent  `json:"system"`
	Stream        bool              `json:"stream"`
	Temperature   json.RawMessage   `json:"temperature"`
	TopP          json.RawMessage   `json:"top_p"`
	StopSequences json.RawMessage   `json:"stop_sequences"`
	Tools         []json.RawMessage `json:"tools"`
	ToolChoice    json.RawMessage   `json:"tool_choice"`
}

// RequestToOpenAI turns an Anthropic Messages request into an OpenAI chat
// completion request. model replaces whatever model the caller named: Claude
// Code sends its own model ids, and the endpoint behind the daemon has never
// heard of them.
func RequestToOpenAI(anthropic []byte, model string) ([]byte, error) {
	var req anthropicRequestIn
	if err := json.Unmarshal(anthropic, &req); err != nil {
		return nil, fmt.Errorf("translate: decode anthropic request: %w", err)
	}

	openai := openAIRequestOut{Model: model, Stream: req.Stream}
	// Ask for the token counts on the stream. Without this the upstream sends
	// no usage chunk, so prompt/completion tokens never arrive and a wrapped
	// tool's context gauge sits at zero. The counts ride a final `choices: []`
	// chunk after the content.
	if req.Stream {
		openai.StreamOptions = &openAIStreamOptions{IncludeUsage: true}
	}

	// Anthropic carries the system prompt beside the conversation; OpenAI
	// wants it as the first message. Claude Code also puts role:"system"
	// turns inside `messages` (its environment block arrives that way, after
	// the first user turn) — passed through as-is that reads as a system
	// message mid-conversation, and some chat templates refuse the whole
	// request over it. Every system turn is folded into the one leading
	// system message, in order, and none is left in the conversation.
	system := req.System.text()
	var rawMessages []anthropicMessage
	for _, raw := range req.Messages {
		var m anthropicMessage
		if err := json.Unmarshal(raw, &m); err != nil {
			continue
		}
		rawMessages = append(rawMessages, m)
	}
	for _, m := range rawMessages {
		if m.Role != "system" {
			continue
		}
		text := m.Content.text()
		if text == "" {
			continue
		}
		if system == "" {
			system = text
		} else {
			system += "\n\n" + text
		}
	}

	if system != "" {
		openai.Messages = append(openai.Messages, openAIMessageOut{Role: "system", Content: system})
	}
	for _, m := range rawMessages {
		if m.Role == "system" {
			continue
		}
		appendMessage(m, &openai.Messages)
	}

	// max_tokens is required by Anthropic and optional for OpenAI, so it
	// always has a value to carry across.
	if len(req.MaxTokens) > 0 {
		openai.MaxTokens = req.MaxTokens
	}

	var tools []anthropicTool
	for _, raw := range req.Tools {
		var t anthropicTool
		if err := json.Unmarshal(raw, &t); err != nil {
			continue
		}
		tools = append(tools, t)
	}
	openaiTools := toolsToOpenAI(tools)
	// An empty list is not the same as none: OpenAI rejects `tools: []`, and
	// a request whose only tools were server-side ones (web search and the
	// rest, which carry no input_schema this package can forward) has
	// nothing left to send.
	if len(openaiTools) > 0 {
		openai.Tools = openaiTools
		if len(req.ToolChoice) > 0 {
			if choice, has := toolChoiceToOpenAI(req.ToolChoice); has {
				openai.ToolChoice = choice
			}
			var tc anthropicToolChoice
			if json.Unmarshal(req.ToolChoice, &tc) == nil && tc.DisableParallelToolUse {
				f := false
				openai.ParallelToolCalls = &f
			}
		}
	}

	if len(req.Temperature) > 0 {
		openai.Temperature = req.Temperature
	}
	if len(req.TopP) > 0 {
		openai.TopP = req.TopP
	}
	// stop_sequences is OpenAI's `stop`; everything else above keeps its name.
	if len(req.StopSequences) > 0 {
		openai.Stop = req.StopSequences
	}

	return json.Marshal(openai)
}

// EstimateRequestTokens is a rough token count for the system prompt and
// every message in anthropic, for StreamState's input_estimate: ~4 characters
// per token.
func EstimateRequestTokens(anthropic []byte) int {
	var req anthropicRequestIn
	if err := json.Unmarshal(anthropic, &req); err != nil {
		return 0
	}
	chars := req.System.chars()
	for _, raw := range req.Messages {
		var m anthropicMessage
		if err := json.Unmarshal(raw, &m); err != nil {
			continue
		}
		chars += m.Content.chars()
	}
	return estimateTokensFromChars(chars)
}

// appendMessage appends the OpenAI messages one Anthropic message implies.
//
// One Anthropic turn can become several. Anthropic packs the results of a
// round of tool calls into the user turn that follows them, while OpenAI
// wants each result as its own `tool` message sitting directly after the
// assistant turn that asked for it, so the results are written first and
// whatever text shared that turn follows as a message of its own.
func appendMessage(message anthropicMessage, out *[]openAIMessageOut) {
	role := message.Role
	if role == "" {
		role = "user"
	}
	content := message.Content

	for _, block := range content {
		if block.Type != "tool_result" {
			continue
		}
		*out = append(*out, openAIMessageOut{
			Role:       "tool",
			ToolCallID: block.ToolUseID,
			Content:    block.Content.text(),
		})
	}

	var calls []openAICallOut
	for _, block := range content {
		if block.Type != "tool_use" {
			continue
		}
		input := block.Input
		if len(input) == 0 {
			input = json.RawMessage("{}")
		}
		calls = append(calls, openAICallOut{
			ID:   block.ID,
			Type: "function",
			// OpenAI carries a call's arguments as a JSON string; Input is
			// already a valid JSON value, so its bytes are the string body.
			Function: openAIFunctionCallOut{Name: block.Name, Arguments: string(input)},
		})
	}

	text := content.text()
	if len(calls) > 0 {
		// An assistant turn that only called tools has no text to carry, and
		// OpenAI reads a null content there rather than an empty string.
		var msgContent any
		if text != "" {
			msgContent = text
		}
		*out = append(*out, openAIMessageOut{Role: role, Content: msgContent, ToolCalls: calls})
		return
	}
	if text != "" {
		*out = append(*out, openAIMessageOut{Role: role, Content: text})
	}
}

// toolsToOpenAI turns Anthropic tool definitions into OpenAI's shape. Only
// client tools carry an input_schema; Anthropic's server-side tools (web
// search and the rest) name a type nothing here can run and have no schema to
// forward, so they are left out rather than passed on as something the
// endpoint would have to invent a meaning for.
func toolsToOpenAI(tools []anthropicTool) []openAIToolOut {
	var out []openAIToolOut
	for _, t := range tools {
		if len(t.InputSchema) == 0 {
			continue
		}
		out = append(out, openAIToolOut{
			Type: "function",
			Function: openAIFunctionDefOut{
				Name:        t.Name,
				Description: t.Description,
				Parameters:  t.InputSchema,
			},
		})
	}
	return out
}

// toolChoiceToOpenAI turns Anthropic's tool_choice into OpenAI's vocabulary.
// has is false when choice says something OpenAI has no way to express.
func toolChoiceToOpenAI(raw json.RawMessage) (value json.RawMessage, has bool) {
	var s string
	if err := json.Unmarshal(raw, &s); err == nil {
		b, _ := json.Marshal(s)
		return b, true
	}
	var choice anthropicToolChoice
	if err := json.Unmarshal(raw, &choice); err != nil {
		return nil, false
	}
	switch choice.Type {
	case "auto", "none":
		b, _ := json.Marshal(choice.Type)
		return b, true
	case "any":
		// "any" means the model has to call something, without saying what.
		b, _ := json.Marshal("required")
		return b, true
	case "tool":
		b, _ := json.Marshal(map[string]any{
			"type":     "function",
			"function": map[string]any{"name": choice.Name},
		})
		return b, true
	}
	return nil, false
}
