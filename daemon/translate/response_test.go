package translate

import "testing"

func TestResponseToAnthropic(t *testing.T) {
	cases := []struct {
		name  string
		input string
		model string
		want  string
	}{
		{
			name: "plain text response with usage",
			input: `{"id":"chatcmpl-1","choices":[{"message":{"role":"assistant","content":"hello there"},"finish_reason":"stop"}],
				"usage":{"prompt_tokens":10,"completion_tokens":3}}`,
			model: "m",
			want: `{"id":"chatcmpl-1","type":"message","role":"assistant","model":"m",
				"content":[{"type":"text","text":"hello there"}],
				"stop_reason":"end_turn","stop_sequence":null,
				"usage":{"input_tokens":10,"output_tokens":3}}`,
		},
		{
			name: "tool-call-only turn: null content, finish_reason tool_calls maps to tool_use",
			input: `{"id":"chatcmpl-2","choices":[{"message":{"role":"assistant","content":null,
				"tool_calls":[{"id":"call_1","function":{"name":"get_weather","arguments":"{\"city\":\"NYC\"}"}}]},
				"finish_reason":"tool_calls"}]}`,
			model: "m",
			want: `{"id":"chatcmpl-2","type":"message","role":"assistant","model":"m",
				"content":[{"type":"tool_use","id":"call_1","name":"get_weather","input":{"city":"NYC"}}],
				"stop_reason":"tool_use","stop_sequence":null,
				"usage":{"input_tokens":0,"output_tokens":0}}`,
		},
		{
			name: "finish_reason stop with a tool call still reads as tool_use, not end_turn",
			input: `{"id":"chatcmpl-3","choices":[{"message":{"role":"assistant","content":null,
				"tool_calls":[{"id":"call_1","function":{"name":"f","arguments":"{}"}}]},
				"finish_reason":"stop"}]}`,
			model: "m",
			want: `{"id":"chatcmpl-3","type":"message","role":"assistant","model":"m",
				"content":[{"type":"tool_use","id":"call_1","name":"f","input":{}}],
				"stop_reason":"tool_use","stop_sequence":null,
				"usage":{"input_tokens":0,"output_tokens":0}}`,
		},
		{
			name: "finish_reason length with a tool call stays max_tokens: truncation outranks the call",
			input: `{"id":"chatcmpl-4","choices":[{"message":{"role":"assistant","content":null,
				"tool_calls":[{"id":"call_1","function":{"name":"f","arguments":"{}"}}]},
				"finish_reason":"length"}]}`,
			model: "m",
			want: `{"id":"chatcmpl-4","type":"message","role":"assistant","model":"m",
				"content":[{"type":"tool_use","id":"call_1","name":"f","input":{}}],
				"stop_reason":"max_tokens","stop_sequence":null,
				"usage":{"input_tokens":0,"output_tokens":0}}`,
		},
		{
			name:  "empty response gets a single empty text block and a null stop_reason",
			input: `{"choices":[{"message":{"role":"assistant","content":""},"finish_reason":""}]}`,
			model: "m",
			want: `{"id":"msg_wally","type":"message","role":"assistant","model":"m",
				"content":[{"type":"text","text":""}],
				"stop_reason":null,"stop_sequence":null,
				"usage":{"input_tokens":0,"output_tokens":0}}`,
		},
		{
			name:  "missing id falls back to msg_wally",
			input: `{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}`,
			model: "m",
			want: `{"id":"msg_wally","type":"message","role":"assistant","model":"m",
				"content":[{"type":"text","text":"hi"}],
				"stop_reason":"end_turn","stop_sequence":null,
				"usage":{"input_tokens":0,"output_tokens":0}}`,
		},
		{
			name: "a tool call with no name is dropped; a message with only such calls falls back to empty text",
			input: `{"choices":[{"message":{"role":"assistant","content":null,
				"tool_calls":[{"id":"call_1","function":{"name":"","arguments":"{}"}}]},
				"finish_reason":"tool_calls"}]}`,
			model: "m",
			// finish_reason itself already says tool_calls, which stopReason
			// maps to tool_use before stopWithTools even runs; with no named
			// block written, stopWithTools has calls=false and returns that
			// mapped reason unchanged rather than overriding it.
			want: `{"id":"msg_wally","type":"message","role":"assistant","model":"m",
				"content":[{"type":"text","text":""}],
				"stop_reason":"tool_use","stop_sequence":null,
				"usage":{"input_tokens":0,"output_tokens":0}}`,
		},
		{
			name: "prompt_tokens_details.cached_tokens splits into input_tokens and cache_read_input_tokens",
			input: `{"id":"chatcmpl-5","choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
				"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":60},"completion_tokens":4}}`,
			model: "m",
			want: `{"id":"chatcmpl-5","type":"message","role":"assistant","model":"m",
				"content":[{"type":"text","text":"hi"}],
				"stop_reason":"end_turn","stop_sequence":null,
				"usage":{"input_tokens":40,"cache_read_input_tokens":60,"output_tokens":4}}`,
		},
		{
			name: "a cached_tokens count larger than prompt_tokens is clamped rather than going negative",
			input: `{"id":"chatcmpl-6","choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
				"usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":999},"completion_tokens":1}}`,
			model: "m",
			want: `{"id":"chatcmpl-6","type":"message","role":"assistant","model":"m",
				"content":[{"type":"text","text":"hi"}],
				"stop_reason":"end_turn","stop_sequence":null,
				"usage":{"input_tokens":0,"cache_read_input_tokens":10,"output_tokens":1}}`,
		},
		{
			name: "unparsable tool call arguments become an empty object rather than dropping the turn",
			input: `{"choices":[{"message":{"role":"assistant","content":null,
				"tool_calls":[{"id":"call_1","function":{"name":"f","arguments":"not json"}}]},
				"finish_reason":"tool_calls"}]}`,
			model: "m",
			want: `{"id":"msg_wally","type":"message","role":"assistant","model":"m",
				"content":[{"type":"tool_use","id":"call_1","name":"f","input":{}}],
				"stop_reason":"tool_use","stop_sequence":null,
				"usage":{"input_tokens":0,"output_tokens":0}}`,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ResponseToAnthropic([]byte(tc.input), tc.model)
			if err != nil {
				t.Fatalf("ResponseToAnthropic: %v", err)
			}
			requireJSONEqual(t, got, []byte(tc.want))
		})
	}
}

func TestResponseToAnthropicInvalidJSON(t *testing.T) {
	if _, err := ResponseToAnthropic([]byte("not json"), "m"); err == nil {
		t.Fatal("expected an error decoding malformed openai response, got nil")
	}
}

func TestStopReason(t *testing.T) {
	cases := map[string]string{
		"length":     "max_tokens",
		"tool_calls": "tool_use",
		"":           "",
		"stop":       "end_turn",
		"anything":   "end_turn",
	}
	for finish, want := range cases {
		if got := stopReason(finish); got != want {
			t.Errorf("stopReason(%q) = %q, want %q", finish, got, want)
		}
	}
}

func TestStopWithTools(t *testing.T) {
	cases := []struct {
		finish string
		calls  bool
		want   string
	}{
		{"end_turn", false, "end_turn"},
		{"end_turn", true, "tool_use"},
		{"max_tokens", true, "max_tokens"},
		{"max_tokens", false, "max_tokens"},
	}
	for _, tc := range cases {
		if got := stopWithTools(tc.finish, tc.calls); got != tc.want {
			t.Errorf("stopWithTools(%q, %v) = %q, want %q", tc.finish, tc.calls, got, tc.want)
		}
	}
}
