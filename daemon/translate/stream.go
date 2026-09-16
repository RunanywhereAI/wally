package translate

import (
	"encoding/json"
	"fmt"
	"sort"
	"strconv"
)

// toolCall is a call being assembled from the stream. OpenAI spreads one
// across as many chunks as it likes, naming it once and then sending its
// arguments a few characters at a time.
type toolCall struct {
	id        string
	name      string
	arguments string
}

// StreamState is one OpenAI stream turned into Anthropic's stream, which is a
// state machine (message_start, content_block_start, deltas,
// content_block_stop, message_delta, message_stop) where OpenAI's is a flat
// run of deltas. StreamState carries what has already been emitted so the
// opening events fire exactly once.
type StreamState struct {
	model     string
	messageID string

	opened bool
	// A text content block is currently open (assigned textIndex).
	blockOpen bool
	// The text block claims index 0 the first time content arrives; a
	// tool_use block, decided only once the stream ends, takes whatever
	// follows. -1 until assigned.
	textIndex int
	// The next index Close hands to a tool_use block.
	nextIndex int
	// Set once the endpoint has reported a failure, after which the closing
	// events would be describing a turn that never happened.
	failed bool

	stopReason   string
	inputTokens  int
	outputTokens int
	// The raw cached-prefix count off the last chunk that reported one, or
	// nil when no chunk ever did. Clamped and subtracted from inputTokens in
	// Close, not here: inputTokens can still be overwritten by a later
	// chunk, and clamping early against a total that is about to change
	// would clamp against the wrong number.
	cacheReadTokens *int
	// A rough stand-in for inputTokens, set by the caller before the first
	// chunk arrives (see EstimateRequestTokens). message_start goes out
	// before the upstream has said anything about usage — an OpenAI-shaped
	// endpoint only attaches it to a later chunk, sometimes the very last one
	// — so 0 there was never "no input", only "not yet known". A wrapped tool
	// that reads usage once at message_start and never again would otherwise
	// see a hard zero for the whole turn.
	inputEstimate int
	// Characters of assistant text, thinking, and tool-call arguments
	// written so far: the fallback basis for outputTokens if the endpoint's
	// stream ends without ever reporting completion_tokens. A reasoning
	// model can spend its whole budget on thinking before any answer text
	// exists (glm-5.3-flash measured: 199 of 200 tokens on a throwaway
	// max_tokens), so leaving thinking out of this count is what made the
	// fallback read 0 on a turn that plainly cost something.
	outputChars int

	// The calls so far, keyed by slot in the order they were first seen,
	// which is the order Close writes them in.
	//
	// Endpoints identify a call in two different ways and the slot is what
	// reconciles them: OpenAI numbers its calls and dribbles the arguments of
	// each across chunks, while others send a call whole and number nothing,
	// leaning on the id instead. Keying on either alone merges calls that are
	// separate or splits one that is not.
	toolCalls       map[int]*toolCall
	toolSlotByIndex map[int]int
	toolSlotByID    map[string]int
	nextToolSlot    int
}

// NewStreamState starts a translator for one turn. messageID is the id this
// state writes into message_start; unlike the upstream chunk's own "id"
// field (which some endpoints omit or randomize per chunk), the caller
// supplies it up front so the id in the Anthropic stream matches whatever the
// daemon is already tracking the request under. An empty messageID falls
// back to "msg_wally" once the stream actually opens.
func NewStreamState(model, messageID string, inputEstimate int) *StreamState {
	return &StreamState{
		model:           model,
		messageID:       messageID,
		textIndex:       -1,
		inputEstimate:   inputEstimate,
		toolCalls:       map[int]*toolCall{},
		toolSlotByIndex: map[int]int{},
		toolSlotByID:    map[string]int{},
	}
}

// Chunk turns one OpenAI stream chunk into the Anthropic SSE text it implies,
// or "" when it implies nothing.
func (s *StreamState) Chunk(openaiChunk []byte) (string, error) {
	// An endpoint is free to answer 200 and then report the failure in the
	// stream, which is how a console reports a request over quota. Skipping
	// the frame as unrecognised ends the stream with no content, and a
	// client handed an empty turn sits there waiting rather than saying it
	// was refused.
	if typ, msg, ok := PayloadError(openaiChunk); ok {
		s.failed = true
		return event("error", errorPayload(typ, msg)), nil
	}

	var chunk openAIChunkIn
	if err := json.Unmarshal(openaiChunk, &chunk); err != nil {
		return "", fmt.Errorf("translate: decode stream chunk: %w", err)
	}

	out := ""

	if !s.opened {
		s.opened = true
		if s.messageID == "" {
			s.messageID = "msg_wally"
		}
		out += event("message_start", messageStartEvent{
			Type: "message_start",
			Message: anthropicMessageOut{
				ID:      s.messageID,
				Type:    "message",
				Role:    "assistant",
				Model:   s.model,
				Content: []anthropicBlockOut{},
				// input_tokens is an estimate, not a placeholder zero: the
				// real count only arrives on a later chunk (sometimes the
				// very last one). output_tokens is genuinely 0 — nothing
				// has been generated yet.
				Usage: anthropicUsageOut{InputTokens: s.inputEstimate, OutputTokens: 0},
			},
		})
	}

	var choice openAIChunkChoiceIn
	if len(chunk.Choices) > 0 {
		choice = chunk.Choices[0]
	}

	if choice.FinishReason != "" {
		s.stopReason = stopReason(choice.FinishReason)
	}
	if chunk.Usage != nil {
		s.inputTokens = chunk.Usage.PromptTokens
		s.outputTokens = chunk.Usage.CompletionTokens
		s.cacheReadTokens = nil
		if chunk.Usage.PromptTokensDetails != nil {
			c := chunk.Usage.PromptTokensDetails.CachedTokens
			s.cacheReadTokens = &c
		}
	}

	delta := choice.Delta

	// Gathered rather than written straight through. A call arrives as
	// fragments scattered across the stream, and a provider is free to
	// advance two of them at once; writing as they land would interleave two
	// half-built blocks, which Anthropic's stream cannot express. They go
	// out whole in Close instead.
	for _, call := range delta.ToolCalls {
		s.mergeToolCallDelta(call)
	}

	// Reasoning models (glm-5.3-flash among them) stream their thinking
	// separate from and usually well before content — against a throwaway
	// max_tokens, a captured run spent 199 of 200 completion_tokens here
	// before ever reaching answer text. Those characters count toward the
	// fallback output estimate so a thinking-heavy turn is not mistaken for
	// a free one, but they are NOT forwarded as a content block: wally does
	// not surface model thinking to the client. That is a product choice,
	// not a technical constraint — Ollama's own Anthropic-shaped stream
	// (ollama/anthropic.go StreamConverter.Process) emits a thinking block's
	// content_block_stop with no signature_delta ahead of it, so a signature
	// was never required to close one. The real completion_tokens the
	// endpoint reports already includes these tokens, so the count stays
	// correct without surfacing a block wally has decided not to show.
	s.outputChars += len(delta.reasoningText())

	// content is null on the chunk that only carries a finish reason.
	if delta.Content == "" {
		return out, nil
	}
	s.outputChars += len(delta.Content)

	// The block opens on the first token rather than up front: a stream that
	// only ever carries a finish reason should not announce a text block
	// that never gets one.
	if !s.blockOpen {
		s.blockOpen = true
		s.textIndex = s.nextIndex
		s.nextIndex++
		out += event("content_block_start", contentBlockStartEvent{
			Type:         "content_block_start",
			Index:        s.textIndex,
			ContentBlock: textBlock(""),
		})
	}
	out += event("content_block_delta", contentBlockDeltaEvent{
		Type:  "content_block_delta",
		Index: s.textIndex,
		Delta: anthropicDeltaOut{Type: "text_delta", Text: delta.Content},
	})
	return out, nil
}

// mergeToolCallDelta reconciles one OpenAI tool-call fragment into the slot
// it belongs to, by index when the endpoint numbers calls and by id when it
// does not.
func (s *StreamState) mergeToolCallDelta(call openAIToolCallDeltaIn) {
	numbered := call.Index != nil
	index := 0
	if numbered {
		index = *call.Index
	}

	slot := -1
	switch {
	case numbered:
		if found, ok := s.toolSlotByIndex[index]; ok {
			slot = found
		}
	case call.ID != "":
		if found, ok := s.toolSlotByID[call.ID]; ok {
			slot = found
		}
	case s.nextToolSlot > 0:
		// Nothing to identify it by, so the only reading left is that it
		// carries on the call already being assembled.
		slot = s.nextToolSlot - 1
	}
	if slot < 0 {
		slot = s.nextToolSlot
		s.nextToolSlot++
	}
	if numbered {
		s.toolSlotByIndex[index] = slot
	}
	if call.ID != "" {
		s.toolSlotByID[call.ID] = slot
	}

	pending, ok := s.toolCalls[slot]
	if !ok {
		pending = &toolCall{}
		s.toolCalls[slot] = pending
	}
	if call.ID != "" {
		pending.id = call.ID
	}
	if call.Function.Name != "" {
		pending.name = call.Function.Name
	}
	if call.Function.Arguments != "" {
		pending.arguments += call.Function.Arguments
		s.outputChars += len(call.Function.Arguments)
	}
}

// Fail marks the turn as ended in error without writing anything itself, so
// a caller that reports a stream-level failure of its own (see ErrorEvent)
// can stop Close from following it with the closing events for a turn that
// did not actually finish.
func (s *StreamState) Fail() {
	s.failed = true
}

// Close returns the closing events once the upstream stream ends, or "" when
// nothing ever opened or the turn already failed.
func (s *StreamState) Close() string {
	if !s.opened || s.failed {
		return ""
	}
	out := ""

	// The text block opened first (index 0); tool_use blocks take whatever
	// indices are left.
	if s.blockOpen {
		s.blockOpen = false
		out += event("content_block_stop", contentBlockStopEvent{
			Type:  "content_block_stop",
			Index: s.textIndex,
		})
	}

	slots := make([]int, 0, len(s.toolCalls))
	for slot := range s.toolCalls {
		slots = append(slots, slot)
	}
	sort.Ints(slots)

	index := s.nextIndex
	emittedToolBlock := false
	for _, slot := range slots {
		call := s.toolCalls[slot]
		// A call nobody ever named cannot be run, and a block naming
		// nothing is worse for the client than a call it never hears about.
		if call.name == "" {
			continue
		}
		emittedToolBlock = true
		// The client matches a result back to its call by this id, so a
		// call the endpoint never named still needs one it can quote.
		id := call.id
		if id == "" {
			id = "tool_" + strconv.Itoa(slot)
		}
		out += event("content_block_start", contentBlockStartEvent{
			Type:  "content_block_start",
			Index: index,
			ContentBlock: anthropicBlockOut{
				Type:  "tool_use",
				ID:    id,
				Name:  call.name,
				Input: json.RawMessage("{}"),
			},
		})
		partialJSON := call.arguments
		if partialJSON == "" {
			partialJSON = "{}"
		}
		out += event("content_block_delta", contentBlockDeltaEvent{
			Type:  "content_block_delta",
			Index: index,
			Delta: anthropicDeltaOut{Type: "input_json_delta", PartialJSON: partialJSON},
		})
		out += event("content_block_stop", contentBlockStopEvent{
			Type:  "content_block_stop",
			Index: index,
		})
		index++
	}

	// What was actually written, not what was pending. Every call being
	// unnamed leaves toolCalls non-empty with no tool_use block in the
	// content, and "tool_use" there parks the client waiting for a call that
	// never arrives.
	finish := s.stopReason
	if finish == "" {
		finish = "end_turn"
	}
	stop := stopWithTools(finish, emittedToolBlock)

	// inputTokens too, not just output. message_start could only carry an
	// estimate (the upstream reports prompt_tokens on a later chunk, often
	// the very last one), so this final usage is the one place the real
	// prompt count reaches the client. The endpoint's own count wins
	// whenever it sent one; a stream that finished without ever reporting
	// usage is not the same claim as "this turn cost nothing", so it falls
	// back to the same character estimate rather than assert a number
	// nobody actually measured.
	inputFinal := s.inputEstimate
	if s.inputTokens > 0 {
		inputFinal = s.inputTokens
	}
	outputFinal := estimateTokensFromChars(s.outputChars)
	if s.outputTokens > 0 {
		outputFinal = s.outputTokens
	}

	// inputFinal is the reported total, same as OpenAI's prompt_tokens; the
	// cached portion is split out here, against that resolved total, rather
	// than at the point each chunk arrived, since a later chunk can still
	// replace inputFinal before Close runs.
	var cacheRead *int
	if s.cacheReadTokens != nil {
		c := clampCached(*s.cacheReadTokens, inputFinal)
		cacheRead = &c
	}

	out += event("message_delta", messageDeltaEvent{
		Type:  "message_delta",
		Delta: messageDeltaBody{StopReason: stop, StopSequence: nil},
		Usage: anthropicUsageOut{
			InputTokens:          inputFinal - intValue(cacheRead),
			CacheReadInputTokens: cacheRead,
			OutputTokens:         outputFinal,
		},
	})
	out += event("message_stop", messageStopEvent{Type: "message_stop"})
	return out
}
