package translate

import "testing"

func runChunks(t *testing.T, s *StreamState, chunks [][]byte) []sseEvent {
	t.Helper()
	var events []sseEvent
	for i, c := range chunks {
		text, err := s.Chunk(c)
		if err != nil {
			t.Fatalf("Chunk(%d): %v", i, err)
		}
		events = append(events, parseSSE(t, text)...)
	}
	return events
}

func rawChunks(t *testing.T, fixture string) [][]byte {
	t.Helper()
	raw := readChunks(t, fixture)
	out := make([][]byte, len(raw))
	for i, r := range raw {
		out[i] = []byte(r)
	}
	return out
}

func TestStreamTextSequence(t *testing.T) {
	s := NewStreamState("qwen", "msg_test", 50)
	events := runChunks(t, s, rawChunks(t, "stream_text.json"))

	names := eventNames(events)
	want := []string{"message_start", "content_block_start", "content_block_delta", "content_block_delta"}
	if len(names) != len(want) {
		t.Fatalf("events before close = %v, want %v", names, want)
	}
	for i := range want {
		if names[i] != want[i] {
			t.Fatalf("events before close = %v, want %v", names, want)
		}
	}

	start := events[0].data["message"].(map[string]any)
	if start["id"] != "msg_test" {
		t.Errorf("message_start id = %v, want msg_test (the constructor's id, not the upstream chunk's)", start["id"])
	}
	if start["model"] != "qwen" {
		t.Errorf("message_start model = %v, want qwen", start["model"])
	}
	usage := start["usage"].(map[string]any)
	if usage["input_tokens"] != float64(50) || usage["output_tokens"] != float64(0) {
		t.Errorf("message_start usage = %v, want the input estimate and 0 output", usage)
	}
	if content, ok := start["content"].([]any); !ok || len(content) != 0 {
		t.Errorf("message_start content = %v, want an empty array", start["content"])
	}

	block := events[1].data["content_block"].(map[string]any)
	if block["type"] != "text" || block["text"] != "" {
		t.Errorf("content_block_start block = %v, want an empty text block", block)
	}

	firstDelta := events[2].data["delta"].(map[string]any)
	if firstDelta["text"] != "Hello" {
		t.Errorf("first content_block_delta text = %v, want Hello", firstDelta["text"])
	}
	secondDelta := events[3].data["delta"].(map[string]any)
	if secondDelta["text"] != ", world" {
		t.Errorf("second content_block_delta text = %v, want \", world\"", secondDelta["text"])
	}

	closeEvents := parseSSE(t, s.Close())
	closeNames := eventNames(closeEvents)
	wantClose := []string{"content_block_stop", "message_delta", "message_stop"}
	if len(closeNames) != len(wantClose) {
		t.Fatalf("close events = %v, want %v", closeNames, wantClose)
	}
	for i := range wantClose {
		if closeNames[i] != wantClose[i] {
			t.Fatalf("close events = %v, want %v", closeNames, wantClose)
		}
	}
	delta := closeEvents[1].data["delta"].(map[string]any)
	if delta["stop_reason"] != "end_turn" {
		t.Errorf("message_delta stop_reason = %v, want end_turn", delta["stop_reason"])
	}
	closeUsage := closeEvents[1].data["usage"].(map[string]any)
	if closeUsage["input_tokens"] != float64(12) || closeUsage["output_tokens"] != float64(3) {
		t.Errorf("message_delta usage = %v, want the real reported counts (12, 3)", closeUsage)
	}
}

func TestStreamToolCallsByIndex(t *testing.T) {
	s := NewStreamState("m", "msg_1", 5)
	runChunks(t, s, rawChunks(t, "stream_tool_calls_by_index.json"))

	events := parseSSE(t, s.Close())
	names := eventNames(events)
	want := []string{
		"content_block_start", "content_block_delta", "content_block_stop",
		"content_block_start", "content_block_delta", "content_block_stop",
		"message_delta", "message_stop",
	}
	if len(names) != len(want) {
		t.Fatalf("close events = %v, want %v", names, want)
	}
	for i := range want {
		if names[i] != want[i] {
			t.Fatalf("close events = %v, want %v", names, want)
		}
	}

	firstBlock := events[0].data["content_block"].(map[string]any)
	if firstBlock["id"] != "call_A" || firstBlock["name"] != "get_weather" {
		t.Errorf("first tool_use block = %v, want call_A/get_weather", firstBlock)
	}
	firstArgs := events[1].data["delta"].(map[string]any)["partial_json"]
	if firstArgs != `{"city":"NYC"}` {
		t.Errorf("first tool arguments assembled across chunks = %v, want {\"city\":\"NYC\"}", firstArgs)
	}

	secondBlock := events[3].data["content_block"].(map[string]any)
	if secondBlock["id"] != "call_B" || secondBlock["name"] != "get_time" {
		t.Errorf("second tool_use block = %v, want call_B/get_time", secondBlock)
	}
	secondArgs := events[4].data["delta"].(map[string]any)["partial_json"]
	if secondArgs != `{"tz":"UTC"}` {
		t.Errorf("second tool arguments = %v, want {\"tz\":\"UTC\"}", secondArgs)
	}

	delta := events[6].data["delta"].(map[string]any)
	if delta["stop_reason"] != "tool_use" {
		t.Errorf("stop_reason = %v, want tool_use", delta["stop_reason"])
	}
	usage := events[6].data["usage"].(map[string]any)
	if usage["input_tokens"] != float64(40) || usage["output_tokens"] != float64(9) {
		t.Errorf("usage = %v, want the reported (40, 9)", usage)
	}
}

func TestStreamToolCallByID(t *testing.T) {
	// Some endpoints send a whole call in one chunk, keyed only by id, with
	// no "index" field at all.
	s := NewStreamState("m", "msg_1", 5)
	runChunks(t, s, rawChunks(t, "stream_tool_call_by_id.json"))

	events := parseSSE(t, s.Close())
	names := eventNames(events)
	want := []string{"content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"}
	if len(names) != len(want) {
		t.Fatalf("close events = %v, want %v", names, want)
	}
	block := events[0].data["content_block"].(map[string]any)
	if block["id"] != "call_X" || block["name"] != "lookup" {
		t.Errorf("tool_use block = %v, want call_X/lookup", block)
	}
	args := events[1].data["delta"].(map[string]any)["partial_json"]
	if args != `{"q":"cats"}` {
		t.Errorf("arguments = %v, want {\"q\":\"cats\"}", args)
	}
}

func TestStreamThinkingRealUsageOverridesFallback(t *testing.T) {
	s := NewStreamState("glm-5.3-flash", "msg_1", 5)
	events := runChunks(t, s, rawChunks(t, "stream_thinking.json"))

	// reasoning_content never produces a content block of its own: only
	// message_start (from the first chunk) and the answer text "4" produce
	// events.
	names := eventNames(events)
	want := []string{"message_start", "content_block_start", "content_block_delta"}
	if len(names) != len(want) {
		t.Fatalf("events = %v, want %v", names, want)
	}
	for i := range want {
		if names[i] != want[i] {
			t.Fatalf("events = %v, want %v", names, want)
		}
	}
	text := events[2].data["delta"].(map[string]any)["text"]
	if text != "4" {
		t.Errorf("content text = %v, want 4", text)
	}

	closeEvents := parseSSE(t, s.Close())
	var deltaEvent sseEvent
	for _, e := range closeEvents {
		if e.name == "message_delta" {
			deltaEvent = e
		}
	}
	usage := deltaEvent.data["usage"].(map[string]any)
	// The endpoint reported completion_tokens itself (47), which includes the
	// thinking tokens; that real count wins over the character fallback.
	if usage["output_tokens"] != float64(47) {
		t.Errorf("output_tokens = %v, want the real reported 47", usage["output_tokens"])
	}
}

func TestStreamThinkingCountsTowardFallbackWhenUsageNeverArrives(t *testing.T) {
	s := NewStreamState("glm-5.3-flash", "msg_1", 5)
	chunks := [][]byte{
		[]byte(`{"choices":[{"delta":{"reasoning_content":"0123456789"}}]}`), // 10 chars
		[]byte(`{"choices":[{"delta":{"content":"42"}}]}`),                   // 2 chars
		[]byte(`{"choices":[{"delta":{},"finish_reason":"stop"}]}`),
	}
	runChunks(t, s, chunks)

	closeEvents := parseSSE(t, s.Close())
	var deltaEvent sseEvent
	for _, e := range closeEvents {
		if e.name == "message_delta" {
			deltaEvent = e
		}
	}
	usage := deltaEvent.data["usage"].(map[string]any)
	// 12 chars total (10 thinking + 2 answer), (12+3)/4 = 3. Leaving the
	// thinking characters out of this count is the bug this guards: a turn
	// that spent its whole budget on thinking would otherwise read as free.
	if usage["output_tokens"] != float64(3) {
		t.Errorf("output_tokens fallback = %v, want 3 (thinking chars counted in)", usage["output_tokens"])
	}
}

func TestStreamMidStreamFailureSuppressesClose(t *testing.T) {
	s := NewStreamState("m", "msg_1", 5)
	chunks := [][]byte{
		[]byte(`{"choices":[{"delta":{"content":"partial"}}]}`),
		[]byte(`{"error":{"message":"You have exceeded your current quota for this month, please check your plan"}}`),
	}

	firstText, err := s.Chunk(chunks[0])
	if err != nil {
		t.Fatalf("Chunk(0): %v", err)
	}
	if len(parseSSE(t, firstText)) == 0 {
		t.Fatal("expected message_start and a text delta before the failure")
	}

	secondText, err := s.Chunk(chunks[1])
	if err != nil {
		t.Fatalf("Chunk(1): %v", err)
	}
	events := parseSSE(t, secondText)
	if len(events) != 1 || events[0].name != "error" {
		t.Fatalf("events after failure = %v, want exactly one error event", eventNames(events))
	}
	errBody := events[0].data["error"].(map[string]any)
	if errBody["type"] != "rate_limit_error" {
		t.Errorf("error type = %v, want rate_limit_error (quota message)", errBody["type"])
	}

	if got := s.Close(); got != "" {
		t.Errorf("Close() after failure = %q, want empty: a failed turn never happened", got)
	}
}

func TestStreamCacheReadTokensSplitFromInputTokens(t *testing.T) {
	s := NewStreamState("m", "msg_1", 5)
	chunks := [][]byte{
		[]byte(`{"choices":[{"delta":{"content":"hi"}}]}`),
		[]byte(`{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":60},"completion_tokens":4}}`),
	}
	runChunks(t, s, chunks)

	closeEvents := parseSSE(t, s.Close())
	var deltaEvent sseEvent
	for _, e := range closeEvents {
		if e.name == "message_delta" {
			deltaEvent = e
		}
	}
	usage := deltaEvent.data["usage"].(map[string]any)
	if usage["input_tokens"] != float64(40) {
		t.Errorf("input_tokens = %v, want 40 (100 total - 60 cached)", usage["input_tokens"])
	}
	if usage["cache_read_input_tokens"] != float64(60) {
		t.Errorf("cache_read_input_tokens = %v, want 60", usage["cache_read_input_tokens"])
	}
	if usage["output_tokens"] != float64(4) {
		t.Errorf("output_tokens = %v, want 4", usage["output_tokens"])
	}
}

func TestStreamNoCacheReadFieldWhenUpstreamNeverReportsOne(t *testing.T) {
	// The common case: an endpoint that never sends prompt_tokens_details
	// must not grow a cache_read_input_tokens field out of nothing.
	s := NewStreamState("m", "msg_1", 5)
	runChunks(t, s, rawChunks(t, "stream_text.json"))

	closeEvents := parseSSE(t, s.Close())
	var deltaEvent sseEvent
	for _, e := range closeEvents {
		if e.name == "message_delta" {
			deltaEvent = e
		}
	}
	usage := deltaEvent.data["usage"].(map[string]any)
	if _, ok := usage["cache_read_input_tokens"]; ok {
		t.Errorf("usage = %v, want no cache_read_input_tokens key", usage)
	}
}

func TestStreamStateFailSuppressesClose(t *testing.T) {
	// Fail is what a caller reaches for when it hits a failure Chunk itself
	// never saw, e.g. a chunk Chunk could not even parse (a decode error, as
	// opposed to the well-formed error body PayloadError recognizes). It
	// must leave Close as inert as PayloadError's own in-band failure does,
	// or the caller's error event would be followed by a misleading
	// message_delta/message_stop for a turn that did not actually finish.
	s := NewStreamState("m", "msg_1", 5)
	if _, err := s.Chunk([]byte(`{"choices":[{"delta":{"content":"partial"}}]}`)); err != nil {
		t.Fatalf("Chunk: %v", err)
	}
	s.Fail()
	if got := s.Close(); got != "" {
		t.Errorf("Close() after Fail() = %q, want empty", got)
	}
}

func TestStreamFailureOnFirstChunkNeverOpens(t *testing.T) {
	s := NewStreamState("m", "msg_1", 5)
	text, err := s.Chunk([]byte(`{"error":{"message":"boom"}}`))
	if err != nil {
		t.Fatalf("Chunk: %v", err)
	}
	events := parseSSE(t, text)
	if len(events) != 1 || events[0].name != "error" {
		t.Fatalf("events = %v, want exactly one error event and no message_start", eventNames(events))
	}
	if s.opened {
		t.Error("opened = true, want false: a failure on the first frame should never open the message")
	}
	if got := s.Close(); got != "" {
		t.Errorf("Close() = %q, want empty", got)
	}
}

func TestNewStreamStateMessageID(t *testing.T) {
	t.Run("empty id falls back to msg_wally", func(t *testing.T) {
		s := NewStreamState("m", "", 0)
		text, err := s.Chunk([]byte(`{"id":"chatcmpl-upstream","choices":[{"delta":{}}]}`))
		if err != nil {
			t.Fatalf("Chunk: %v", err)
		}
		events := parseSSE(t, text)
		id := events[0].data["message"].(map[string]any)["id"]
		if id != "msg_wally" {
			t.Errorf("message id = %v, want msg_wally", id)
		}
	})

	t.Run("caller's id wins over the upstream chunk's own id", func(t *testing.T) {
		s := NewStreamState("m", "custom-id", 0)
		text, err := s.Chunk([]byte(`{"id":"chatcmpl-upstream","choices":[{"delta":{}}]}`))
		if err != nil {
			t.Fatalf("Chunk: %v", err)
		}
		events := parseSSE(t, text)
		id := events[0].data["message"].(map[string]any)["id"]
		if id != "custom-id" {
			t.Errorf("message id = %v, want custom-id", id)
		}
	})
}
