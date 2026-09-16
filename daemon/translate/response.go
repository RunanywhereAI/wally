package translate

import (
	"encoding/json"
	"fmt"
)

// ResponseToAnthropic turns a whole OpenAI chat completion into a whole
// Anthropic message.
func ResponseToAnthropic(openai []byte, model string) ([]byte, error) {
	var resp openAIResponseIn
	if err := json.Unmarshal(openai, &resp); err != nil {
		return nil, fmt.Errorf("translate: decode openai response: %w", err)
	}

	var choice openAIChoiceIn
	if len(resp.Choices) > 0 {
		choice = resp.Choices[0]
	}

	var content []anthropicBlockOut
	if choice.Message.Content != "" {
		content = append(content, textBlock(choice.Message.Content))
	}
	toolCalled := false
	for _, call := range choice.Message.ToolCalls {
		if call.Function.Name == "" {
			continue
		}
		content = append(content, anthropicBlockOut{
			Type:  "tool_use",
			ID:    call.ID,
			Name:  call.Function.Name,
			Input: parseArguments(call.Function.Arguments),
		})
		toolCalled = true
	}
	// A turn that said nothing still needs a block: an empty content array
	// reads to some clients as a malformed message rather than an empty one.
	if len(content) == 0 {
		content = append(content, textBlock(""))
	}

	replyID := resp.ID
	if replyID == "" {
		replyID = "msg_wally"
	}

	stop := stopWithTools(stopReason(choice.FinishReason), toolCalled)
	var stopReasonPtr *string
	if stop != "" {
		stopReasonPtr = &stop
	}

	var usage openAIUsageIn
	if resp.Usage != nil {
		usage = *resp.Usage
	}

	out := anthropicMessageOut{
		ID:           replyID,
		Type:         "message",
		Role:         "assistant",
		Model:        model,
		Content:      content,
		StopReason:   stopReasonPtr,
		StopSequence: nil,
		Usage:        usageToAnthropic(usage),
	}
	return json.Marshal(out)
}

// usageToAnthropic splits OpenAI's usage into Anthropic's shape. OpenAI's
// prompt_tokens is a total that already includes any cached prefix
// (prompt_tokens_details.cached_tokens); Anthropic wants input_tokens to
// exclude the cached portion and carry it separately as
// cache_read_input_tokens, the same split ollama/anthropic.UsageFromMetrics
// makes from Ollama's own metrics.
func usageToAnthropic(usage openAIUsageIn) anthropicUsageOut {
	total := usage.PromptTokens
	var cached *int
	if usage.PromptTokensDetails != nil {
		c := clampCached(usage.PromptTokensDetails.CachedTokens, total)
		cached = &c
	}
	return anthropicUsageOut{
		InputTokens:          total - intValue(cached),
		CacheReadInputTokens: cached,
		OutputTokens:         usage.CompletionTokens,
	}
}

// clampCached bounds a reported cached-token count to [0, total]: it is a
// subset of total, and an endpoint's cached_tokens has been seen to arrive
// negative or larger than the count it is supposedly a part of.
func clampCached(cached, total int) int {
	return min(max(0, cached), total)
}

func intValue(v *int) int {
	if v == nil {
		return 0
	}
	return *v
}

// parseArguments reads OpenAI's string-carried call arguments as the object
// Anthropic wants. A model that emits something unparseable here is common
// enough that dropping the whole turn over it would be worse than a call with
// no arguments, which the tool can at least reject on its own terms.
func parseArguments(arguments string) json.RawMessage {
	if arguments == "" {
		return json.RawMessage("{}")
	}
	var v map[string]any
	if err := json.Unmarshal([]byte(arguments), &v); err != nil {
		return json.RawMessage("{}")
	}
	return json.RawMessage(arguments)
}

// stopReason maps an OpenAI finish reason into Anthropic's vocabulary.
func stopReason(finish string) string {
	switch finish {
	case "length":
		return "max_tokens"
	case "tool_calls":
		return "tool_use"
	case "":
		return ""
	default:
		return "end_turn"
	}
}

// stopWithTools decides the reason a turn carrying tool calls ended.
//
// Endpoints disagree here: one closes a turn holding a tool call with
// "tool_calls", another with a plain "stop". The second reads as end_turn,
// which tells the client to show the answer and wait for the reader rather
// than run the tool, so the call is written, ignored, and the agent narrates
// what it was about to do instead of doing it. Truncation still outranks it,
// because a call cut off mid-argument cannot be run.
func stopWithTools(finish string, calls bool) string {
	if !calls || finish == "max_tokens" {
		return finish
	}
	return "tool_use"
}
