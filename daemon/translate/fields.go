// Package translate converts between the Anthropic and OpenAI wire formats.
package translate

import "encoding/json"

// estimateTokensFromChars applies the ~4-characters-per-token rule of thumb
// the provider docs use for back-of-envelope sizing. It is never a substitute
// for a real count; it is only what message_start has to show before one
// exists.
func estimateTokensFromChars(chars int) int {
	if chars == 0 {
		return 0
	}
	return (chars + 3) / 4
}

// event builds one SSE frame. Marshal cannot fail here: every value this
// package passes in is a request- or state-derived struct or a plain map
// this package built itself, never a channel, a function, or a value that
// carries a NaN float.
func event(name string, data any) string {
	b, err := json.Marshal(data)
	if err != nil {
		return ""
	}
	return "event: " + name + "\ndata: " + string(b) + "\n\n"
}
