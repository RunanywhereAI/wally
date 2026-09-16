package translate

import "testing"

func TestRequestToOpenAI(t *testing.T) {
	cases := []struct {
		name  string
		input string
		model string
		want  string
	}{
		{
			name:  "model override and minimal fields",
			input: `{"model":"claude-3-5-sonnet","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}`,
			model: "qwen2.5-coder",
			want:  `{"model":"qwen2.5-coder","stream":false,"max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}`,
		},
		{
			name: "streaming request asks for usage on the stream",
			input: `{"model":"m","max_tokens":10,"stream":true,
				"messages":[{"role":"user","content":"hi"}]}`,
			model: "m",
			want: `{"model":"m","stream":true,"stream_options":{"include_usage":true},
				"max_tokens":10,"messages":[{"role":"user","content":"hi"}]}`,
		},
		{
			name: "top-level system and mid-conversation system turns fold into one leading message",
			input: `{"model":"m","max_tokens":10,"system":"top-level",
				"messages":[
					{"role":"user","content":"hi"},
					{"role":"system","content":"env block"},
					{"role":"assistant","content":"ok"}
				]}`,
			model: "m",
			want: `{"model":"m","stream":false,"max_tokens":10,"messages":[
				{"role":"system","content":"top-level\n\nenv block"},
				{"role":"user","content":"hi"},
				{"role":"assistant","content":"ok"}
			]}`,
		},
		{
			name:  "no system prompt at all omits the leading system message",
			input: `{"model":"m","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}`,
			model: "m",
			want:  `{"model":"m","stream":false,"max_tokens":10,"messages":[{"role":"user","content":"hi"}]}`,
		},
		{
			name: "tool definitions: schema converts, server-side tool without input_schema is filtered, tool_choice tool maps to function",
			input: `{"model":"m","max_tokens":10,
				"messages":[{"role":"user","content":"hi"}],
				"tools":[
					{"name":"get_weather","description":"gets weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}},
					{"type":"web_search_20250305","name":"web_search"}
				],
				"tool_choice":{"type":"tool","name":"get_weather"}}`,
			model: "m",
			want: `{"model":"m","stream":false,"max_tokens":10,
				"messages":[{"role":"user","content":"hi"}],
				"tools":[{"type":"function","function":{"name":"get_weather","description":"gets weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],
				"tool_choice":{"type":"function","function":{"name":"get_weather"}}}`,
		},
		{
			name: "tool_choice any maps to required and disables parallel calls",
			input: `{"model":"m","max_tokens":10,
				"messages":[{"role":"user","content":"hi"}],
				"tools":[{"name":"t","input_schema":{"type":"object"}}],
				"tool_choice":{"type":"any","disable_parallel_tool_use":true}}`,
			model: "m",
			want: `{"model":"m","stream":false,"max_tokens":10,
				"messages":[{"role":"user","content":"hi"}],
				"tools":[{"type":"function","function":{"name":"t","parameters":{"type":"object"}}}],
				"tool_choice":"required","parallel_tool_calls":false}`,
		},
		{
			name: "tool_choice as a bare string passes through",
			input: `{"model":"m","max_tokens":10,
				"messages":[{"role":"user","content":"hi"}],
				"tools":[{"name":"t","input_schema":{"type":"object"}}],
				"tool_choice":"auto"}`,
			model: "m",
			want: `{"model":"m","stream":false,"max_tokens":10,
				"messages":[{"role":"user","content":"hi"}],
				"tools":[{"type":"function","function":{"name":"t","parameters":{"type":"object"}}}],
				"tool_choice":"auto"}`,
		},
		{
			name: "only server-side tools present omits tools and tool_choice entirely",
			input: `{"model":"m","max_tokens":10,
				"messages":[{"role":"user","content":"hi"}],
				"tools":[{"type":"web_search_20250305","name":"web_search"}],
				"tool_choice":"auto"}`,
			model: "m",
			want:  `{"model":"m","stream":false,"max_tokens":10,"messages":[{"role":"user","content":"hi"}]}`,
		},
		{
			name: "multi-turn: tool_use becomes tool_calls with serialized arguments, tool_result becomes a role:tool message before the next turn",
			input: `{"model":"m","max_tokens":10,"messages":[
				{"role":"user","content":"read file"},
				{"role":"assistant","content":[
					{"type":"text","text":"let me check"},
					{"type":"tool_use","id":"call_1","name":"read_file","input":{"path":"a.txt"}}
				]},
				{"role":"user","content":[
					{"type":"tool_result","tool_use_id":"call_1","content":"file contents"}
				]}
			]}`,
			model: "m",
			want: `{"model":"m","stream":false,"max_tokens":10,"messages":[
				{"role":"user","content":"read file"},
				{"role":"assistant","content":"let me check","tool_calls":[
					{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}
				]},
				{"role":"tool","tool_call_id":"call_1","content":"file contents"}
			]}`,
		},
		{
			name: "assistant turn that only calls a tool carries null content, not an empty string",
			input: `{"model":"m","max_tokens":10,"messages":[
				{"role":"user","content":"do it"},
				{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"foo","input":{}}]}
			]}`,
			model: "m",
			want: `{"model":"m","stream":false,"max_tokens":10,"messages":[
				{"role":"user","content":"do it"},
				{"role":"assistant","content":null,"tool_calls":[
					{"id":"c1","type":"function","function":{"name":"foo","arguments":"{}"}}
				]}
			]}`,
		},
		{
			name:  "temperature, top_p and stop_sequences pass through, stop_sequences renamed to stop",
			input: `{"model":"m","max_tokens":10,"temperature":0.5,"top_p":0.9,"stop_sequences":["END"],"messages":[{"role":"user","content":"hi"}]}`,
			model: "m",
			want:  `{"model":"m","stream":false,"max_tokens":10,"temperature":0.5,"top_p":0.9,"stop":["END"],"messages":[{"role":"user","content":"hi"}]}`,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := RequestToOpenAI([]byte(tc.input), tc.model)
			if err != nil {
				t.Fatalf("RequestToOpenAI: %v", err)
			}
			requireJSONEqual(t, got, []byte(tc.want))
		})
	}
}

func TestRequestToOpenAIInvalidJSON(t *testing.T) {
	if _, err := RequestToOpenAI([]byte("not json"), "m"); err == nil {
		t.Fatal("expected an error decoding malformed anthropic request, got nil")
	}
}

func TestEstimateRequestTokens(t *testing.T) {
	cases := []struct {
		name  string
		input string
		want  int
	}{
		{
			name:  "empty request",
			input: `{}`,
			want:  0,
		},
		{
			name:  "system text only, four characters rounds up to one token",
			input: `{"system":"abcd"}`,
			want:  1,
		},
		{
			name:  "system plus a user message combine",
			input: `{"system":"abcd","messages":[{"role":"user","content":"efgh"}]}`,
			want:  2,
		},
		{
			name: "tool_result nested content and tool_use serialized input both count",
			input: `{"messages":[
				{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"f","input":{"path":"a.txt"}}]},
				{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"0123456789012345"}]}
			]}`,
			// tool_use input {"path":"a.txt"} is 16 raw chars; tool_result
			// content is 16 chars. (16+16+3)/4 = 8.
			want: 8,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := EstimateRequestTokens([]byte(tc.input))
			if got != tc.want {
				t.Fatalf("EstimateRequestTokens() = %d, want %d", got, tc.want)
			}
		})
	}
}
