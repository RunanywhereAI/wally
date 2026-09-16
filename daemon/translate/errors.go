package translate

import (
	"encoding/json"
	"strconv"
	"strings"
)

// ErrorBody is an Anthropic-shaped error body, so a failure reads as one to
// the client rather than as a malformed message.
func ErrorBody(typ, message string) string {
	b, _ := json.Marshal(errorPayload(typ, message))
	return string(b)
}

// ErrorEvent is ErrorBody framed as the SSE "error" event StreamState.Chunk
// itself emits for a mid-stream upstream failure, so a caller outside this
// package can report a stream-level failure of its own (a chunk it could not
// even hand to Chunk) in the same shape a client already knows how to read.
func ErrorEvent(typ, message string) string {
	return event("error", errorPayload(typ, message))
}

func errorPayload(typ, message string) anthropicErrorBody {
	return anthropicErrorBody{
		Type:  "error",
		Error: anthropicErrorDetail{Type: typ, Message: message},
	}
}

// PayloadError reads the failure out of a body an endpoint sent with a
// success status. ok is false when payload carries no error, the ordinary
// case. The payload's shape is not ours to constrain — it is whatever the
// endpoint decided to send on a 200 it is using to carry a failure — so this
// reads it dynamically rather than against a fixed struct.
func PayloadError(payload []byte) (typ, message string, ok bool) {
	var v struct {
		Error json.RawMessage `json:"error"`
	}
	if err := json.Unmarshal(payload, &v); err != nil {
		return "", "", false
	}
	if len(v.Error) == 0 || string(v.Error) == "null" {
		return "", "", false
	}

	var text string
	var asObject struct {
		Message string `json:"message"`
	}
	if json.Unmarshal(v.Error, &asObject) == nil && asObject.Message != "" {
		text = asObject.Message
	} else {
		var asOther any
		if json.Unmarshal(v.Error, &asOther) == nil {
			if _, isObject := asOther.(map[string]any); !isObject {
				text = string(v.Error)
			}
		}
	}
	if text == "" {
		text = "the model endpoint reported an error it did not describe"
	}
	// Worth telling apart: a client that knows it was rate limited can back
	// off and try again, where a plain api_error reads as a dead endpoint.
	if strings.Contains(text, "RESOURCE_EXHAUSTED") || strings.Contains(text, "exceeded your current quota") {
		typ = "rate_limit_error"
	} else {
		typ = "api_error"
	}
	return typ, text, true
}

// UpstreamFailure maps a failed upstream reply to the (type, message) an
// Anthropic-shaped error should carry. status 0 means the endpoint never
// answered.
//
// The HTTP status picks the error type, so a 403 reads as permission_error
// and a 429 as rate_limit_error rather than the generic api_error a tool
// will retry forever; the message is pulled from an OpenAI-style error body
// when there is one, otherwise the raw body, otherwise a plain status line.
func UpstreamFailure(status int, body []byte) (typ, message string) {
	_, extracted, _ := PayloadError(body)
	if extracted == "" {
		// No structured error message: fall back to the raw body (trimmed
		// and capped so a huge HTML error page does not become the
		// message), then a status line, then a plain no-answer note.
		trimmed := strings.TrimSpace(string(body))
		switch {
		case trimmed != "":
			if len(trimmed) > 1000 {
				trimmed = trimmed[:1000]
			}
			extracted = trimmed
		case status != 0:
			extracted = "the model endpoint returned status " + strconv.Itoa(status)
		default:
			extracted = "the model endpoint did not answer"
		}
	}
	if status == 0 {
		typ = "api_error"
	} else {
		typ = errorTypeForStatus(status)
	}
	return typ, extracted
}

// errorTypeForStatus is the Anthropic error type that matches an HTTP
// status. Anything without a closer match is api_error, a retryable
// dead-endpoint signal, which is why the mapping is deliberate: a 403 that
// read as api_error would have the tool retry a refusal it can never
// satisfy.
func errorTypeForStatus(status int) string {
	switch status {
	case 401:
		return "authentication_error"
	case 403:
		return "permission_error"
	case 400, 413, 422:
		return "invalid_request_error"
	case 404:
		return "not_found_error"
	case 429:
		return "rate_limit_error"
	case 503, 529:
		return "overloaded_error"
	default:
		return "api_error"
	}
}
