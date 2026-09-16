package translate

import (
	"strings"
	"testing"
)

func TestUpstreamFailure(t *testing.T) {
	cases := []struct {
		name     string
		status   int
		body     string
		wantType string
		wantMsg  string
	}{
		{
			name:     "403 with a structured error body",
			status:   403,
			body:     `{"error":{"message":"insufficient permissions"}}`,
			wantType: "permission_error",
			wantMsg:  "insufficient permissions",
		},
		{
			name:     "429 maps by status regardless of the message text",
			status:   429,
			body:     `{"error":{"message":"slow down"}}`,
			wantType: "rate_limit_error",
			wantMsg:  "slow down",
		},
		{
			name:     "500 with a non-object error value dumps it raw, quotes and all",
			status:   500,
			body:     `{"error":"internal server meltdown"}`,
			wantType: "api_error",
			wantMsg:  `"internal server meltdown"`,
		},
		{
			name:     "500 with a plain-text body falls back to the trimmed body",
			status:   500,
			body:     "  internal error, try later  \n",
			wantType: "api_error",
			wantMsg:  "internal error, try later",
		},
		{
			name:     "0 with an empty body: the endpoint never answered",
			status:   0,
			body:     "",
			wantType: "api_error",
			wantMsg:  "the model endpoint did not answer",
		},
		{
			name:     "500 with an empty body falls back to a status line",
			status:   500,
			body:     "",
			wantType: "api_error",
			wantMsg:  "the model endpoint returned status 500",
		},
		{
			name:     "401 maps to authentication_error",
			status:   401,
			body:     "",
			wantType: "authentication_error",
			wantMsg:  "the model endpoint returned status 401",
		},
		{
			name:     "404 maps to not_found_error",
			status:   404,
			body:     "",
			wantType: "not_found_error",
			wantMsg:  "the model endpoint returned status 404",
		},
		{
			name:     "503 maps to overloaded_error",
			status:   503,
			body:     "",
			wantType: "overloaded_error",
			wantMsg:  "the model endpoint returned status 503",
		},
		{
			name:     "529 maps to overloaded_error",
			status:   529,
			body:     "",
			wantType: "overloaded_error",
			wantMsg:  "the model endpoint returned status 529",
		},
		{
			name:     "an unmapped status falls back to api_error",
			status:   502,
			body:     "",
			wantType: "api_error",
			wantMsg:  "the model endpoint returned status 502",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			typ, msg := UpstreamFailure(tc.status, []byte(tc.body))
			if typ != tc.wantType {
				t.Errorf("type = %q, want %q", typ, tc.wantType)
			}
			if msg != tc.wantMsg {
				t.Errorf("message = %q, want %q", msg, tc.wantMsg)
			}
		})
	}
}

func TestUpstreamFailureTruncatesAHugeBody(t *testing.T) {
	body := strings.Repeat("x", 5000)
	_, msg := UpstreamFailure(500, []byte(body))
	if len(msg) != 1000 {
		t.Fatalf("message length = %d, want 1000", len(msg))
	}
	if msg != strings.Repeat("x", 1000) {
		t.Fatal("truncated message is not the first 1000 characters of the body")
	}
}

func TestPayloadError(t *testing.T) {
	cases := []struct {
		name     string
		payload  string
		wantOK   bool
		wantType string
		wantMsg  string
	}{
		{
			name:    "no error key at all",
			payload: `{"choices":[{"message":{"content":"hi"}}]}`,
			wantOK:  false,
		},
		{
			name:    "an explicit null error is not an error",
			payload: `{"error":null}`,
			wantOK:  false,
		},
		{
			name:     "an error object with a message",
			payload:  `{"error":{"message":"model overloaded"}}`,
			wantOK:   true,
			wantType: "api_error",
			wantMsg:  "model overloaded",
		},
		{
			name:     "a quota message maps to rate_limit_error",
			payload:  `{"error":{"message":"You have exceeded your current quota for this month, check your plan"}}`,
			wantOK:   true,
			wantType: "rate_limit_error",
		},
		{
			name:     "a RESOURCE_EXHAUSTED message maps to rate_limit_error",
			payload:  `{"error":{"message":"RESOURCE_EXHAUSTED: too many requests"}}`,
			wantOK:   true,
			wantType: "rate_limit_error",
		},
		{
			name:     "an error object without a message field falls back to the default description",
			payload:  `{"error":{}}`,
			wantOK:   true,
			wantType: "api_error",
			wantMsg:  "the model endpoint reported an error it did not describe",
		},
		{
			name:    "malformed JSON is not an error payload",
			payload: "not json",
			wantOK:  false,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			typ, msg, ok := PayloadError([]byte(tc.payload))
			if ok != tc.wantOK {
				t.Fatalf("ok = %v, want %v", ok, tc.wantOK)
			}
			if !ok {
				return
			}
			if typ != tc.wantType {
				t.Errorf("type = %q, want %q", typ, tc.wantType)
			}
			if tc.wantMsg != "" && msg != tc.wantMsg {
				t.Errorf("message = %q, want %q", msg, tc.wantMsg)
			}
		})
	}
}

func TestErrorBody(t *testing.T) {
	got := ErrorBody("rate_limit_error", "slow down")
	requireJSONEqual(t, []byte(got), []byte(`{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}`))
}

func TestErrorEvent(t *testing.T) {
	got := ErrorEvent("api_error", "the model stream sent a chunk that could not be translated")
	want := "event: error\ndata: " +
		`{"type":"error","error":{"type":"api_error","message":"the model stream sent a chunk that could not be translated"}}` +
		"\n\n"
	if got != want {
		t.Errorf("ErrorEvent = %q, want %q", got, want)
	}
}
