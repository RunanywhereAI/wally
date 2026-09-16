package translate

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// requireJSONEqual fails the test unless got and want encode equal JSON
// values, independent of key order or formatting.
func requireJSONEqual(t *testing.T, got, want []byte) {
	t.Helper()
	var g, w any
	if err := json.Unmarshal(got, &g); err != nil {
		t.Fatalf("unmarshal got: %v\ngot: %s", err, got)
	}
	if err := json.Unmarshal(want, &w); err != nil {
		t.Fatalf("unmarshal want: %v\nwant: %s", err, want)
	}
	if !reflect.DeepEqual(g, w) {
		t.Fatalf("json mismatch\n got: %s\nwant: %s", got, want)
	}
}

type sseEvent struct {
	name string
	data map[string]any
}

// parseSSE splits Anthropic SSE text into its individual events.
func parseSSE(t *testing.T, text string) []sseEvent {
	t.Helper()
	if text == "" {
		return nil
	}
	var events []sseEvent
	for _, frame := range strings.Split(strings.TrimSuffix(text, "\n\n"), "\n\n") {
		if frame == "" {
			continue
		}
		lines := strings.SplitN(frame, "\n", 2)
		if len(lines) != 2 {
			t.Fatalf("malformed SSE frame: %q", frame)
		}
		name := strings.TrimPrefix(lines[0], "event: ")
		dataText := strings.TrimPrefix(lines[1], "data: ")
		var data map[string]any
		if err := json.Unmarshal([]byte(dataText), &data); err != nil {
			t.Fatalf("unmarshal SSE data: %v\ndata: %s", err, dataText)
		}
		events = append(events, sseEvent{name: name, data: data})
	}
	return events
}

func eventNames(events []sseEvent) []string {
	names := make([]string, len(events))
	for i, e := range events {
		names[i] = e.name
	}
	return names
}

// readChunks loads a testdata fixture holding a JSON array of OpenAI stream
// chunks, and returns each chunk's raw bytes in order.
func readChunks(t *testing.T, name string) []json.RawMessage {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatalf("read fixture %s: %v", name, err)
	}
	var chunks []json.RawMessage
	if err := json.Unmarshal(data, &chunks); err != nil {
		t.Fatalf("decode fixture %s: %v", name, err)
	}
	return chunks
}
