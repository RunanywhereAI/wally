package daemon

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net/http"

	"github.com/RunanywhereAI/wally/daemon/translate"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func (rt *Router) messages(w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		writeAnthropicError(w, http.StatusBadRequest, "invalid_request_error", "could not read the request body")
		return
	}

	var head struct {
		Model  string `json:"model"`
		Stream bool   `json:"stream"`
	}
	_ = json.Unmarshal(body, &head)

	// A gateway that pins the model in the path (Claude Desktop, which forces
	// its own model name into the body) overrides whatever the body carried.
	model := head.Model
	if forced := r.PathValue("model"); forced != "" {
		model = forced
	}
	if model == "" {
		writeAnthropicError(w, http.StatusBadRequest, "invalid_request_error", "the request is missing a model")
		return
	}

	ep, err := rt.endpoint(model)
	if err != nil {
		writeResolveErrorAnthropic(w, err)
		return
	}

	openaiBody, err := translate.RequestToOpenAI(body, model)
	if err != nil {
		writeAnthropicError(w, http.StatusBadRequest, "invalid_request_error", "the request could not be translated")
		return
	}

	upstream, err := http.NewRequestWithContext(r.Context(), http.MethodPost, ep.BaseURL+"/chat/completions", bytes.NewReader(openaiBody))
	if err != nil {
		writeAnthropicError(w, http.StatusInternalServerError, "api_error", "could not build the upstream request")
		return
	}
	upstream.Header.Set("Content-Type", "application/json")
	if ep.APIKey != "" {
		upstream.Header.Set("Authorization", "Bearer "+ep.APIKey)
	}
	if head.Stream {
		upstream.Header.Set("Accept", "text/event-stream")
	}

	resp, err := rt.client.Do(upstream)
	if err != nil {
		typ, msg := translate.UpstreamFailure(0, nil)
		writeAnthropicError(w, http.StatusBadGateway, typ, msg)
		return
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		errBody, _ := io.ReadAll(resp.Body)
		typ, msg := translate.UpstreamFailure(resp.StatusCode, errBody)
		// A rate-limited or overloaded upstream says how long to wait; dropping
		// it makes the client guess and back off wrong.
		if ra := resp.Header.Get("Retry-After"); ra != "" {
			w.Header().Set("Retry-After", ra)
		}
		writeAnthropicError(w, resp.StatusCode, typ, msg)
		return
	}

	if head.Stream {
		rt.streamMessages(w, resp.Body, model, body)
		return
	}

	openaiResp, _ := io.ReadAll(resp.Body)
	if typ, msg, ok := translate.PayloadError(openaiResp); ok {
		writeAnthropicError(w, http.StatusBadGateway, typ, msg)
		return
	}
	anthropicResp, err := translate.ResponseToAnthropic(openaiResp, model)
	if err != nil {
		writeAnthropicError(w, http.StatusBadGateway, "api_error", "the model response could not be translated")
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	w.Write(anthropicResp)
}

func (rt *Router) streamMessages(w http.ResponseWriter, body io.Reader, model string, anthropicReq []byte) {
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.WriteHeader(http.StatusOK)
	rc := http.NewResponseController(w)

	state := translate.NewStreamState(model, newMessageID(), translate.EstimateRequestTokens(anthropicReq))

	sawDone := false
	reader := bufio.NewReaderSize(body, 1<<20)
	for {
		line, readErr := reader.ReadBytes('\n')
		if trimmed := bytes.TrimSpace(line); bytes.HasPrefix(trimmed, []byte("data:")) {
			payload := bytes.TrimSpace(trimmed[len("data:"):])
			if string(payload) == "[DONE]" {
				sawDone = true
				break
			}
			sse, err := state.Chunk(payload)
			if err != nil {
				// A chunk Chunk could not even parse is still a mid-stream
				// failure the client is owed, the same as one the upstream
				// reported through a body translate.PayloadError recognizes
				// (which Chunk already turns into its own "error" event
				// below, no err returned). Left unreported, the client would
				// see the turn simply end, indistinguishable from a normal
				// close, once state.Close writes the closing events.
				state.Fail()
				io.WriteString(w, translate.ErrorEvent("api_error", "the model stream sent a chunk that could not be translated"))
				rc.Flush()
				break
			}
			if sse != "" {
				io.WriteString(w, sse)
				rc.Flush()
			}
		}
		if readErr != nil {
			break
		}
	}
	// A finished OpenAI stream ends with a finish_reason and then [DONE].
	// Reaching here without both means the upstream was cut off mid-turn:
	// closing cleanly would sign a truncated answer off as a successful
	// end_turn, and would flush any half-built tool call as an executable
	// tool_use. Report the truncation instead and let Close stay silent.
	if !state.Failed() && !state.Complete(sawDone) {
		state.Fail()
		io.WriteString(w, translate.ErrorEvent("api_error", "the model stream ended before it finished"))
		rc.Flush()
	}
	io.WriteString(w, state.Close())
	rc.Flush()
}

func writeAnthropicError(w http.ResponseWriter, status int, typ, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	io.WriteString(w, translate.ErrorBody(typ, message))
}

func writeResolveErrorAnthropic(w http.ResponseWriter, err error) {
	switch {
	case errors.Is(err, runanywhere.ErrNotSignedIn):
		writeAnthropicError(w, http.StatusUnauthorized, "authentication_error", errmap.NewNotSignedIn().Error())
	case errors.Is(err, runanywhere.ErrOnDeviceNotEnabled):
		writeAnthropicError(w, http.StatusNotImplemented, "api_error", errmap.NewOnDeviceNotEnabled().Error())
	default:
		writeAnthropicError(w, http.StatusBadGateway, "api_error", err.Error())
	}
}

func newMessageID() string {
	b := make([]byte, 12)
	rand.Read(b)
	return "msg_" + hex.EncodeToString(b)
}
