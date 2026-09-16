package daemon

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestMessagesNonStreamTranslates(t *testing.T) {
	var gotBody string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		gotBody = string(b)
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"id":"chatcmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"Hello there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}`)
	}))
	defer upstream.Close()

	d := newTestDaemon(NewRouter(stubSession{base: upstream.URL, token: "sk-test"}))
	defer d.Close()

	req := `{"model":"cloud-x","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`
	resp, err := http.Post(d.URL+"/v1/messages", "application/json", strings.NewReader(req))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)

	if !strings.Contains(gotBody, `"messages"`) || !strings.Contains(gotBody, `"cloud-x"`) {
		t.Errorf("upstream did not receive a translated OpenAI request: %s", gotBody)
	}
	if !strings.Contains(string(body), "Hello there") || !strings.Contains(string(body), `"type":"message"`) {
		t.Errorf("response is not an Anthropic message: %s", body)
	}
}

func TestMessagesStreamTranslates(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"}}]}\n\n")
		io.WriteString(w, "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"}}]}\n\n")
		io.WriteString(w, "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
		io.WriteString(w, "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n")
		io.WriteString(w, "data: [DONE]\n\n")
	}))
	defer upstream.Close()

	d := newTestDaemon(NewRouter(stubSession{base: upstream.URL, token: "sk-test"}))
	defer d.Close()

	req := `{"model":"cloud-x","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`
	resp, err := http.Post(d.URL+"/v1/messages", "application/json", strings.NewReader(req))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	out := string(body)

	for _, want := range []string{"message_start", "Hi", "there", "message_stop"} {
		if !strings.Contains(out, want) {
			t.Errorf("stream output missing %q:\n%s", want, out)
		}
	}
}

func TestMessagesStreamMidStreamMalformedChunkSurfacesError(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"}}]}\n\n")
		io.WriteString(w, "data: not json at all\n\n")
		io.WriteString(w, "data: [DONE]\n\n")
	}))
	defer upstream.Close()

	d := newTestDaemon(NewRouter(stubSession{base: upstream.URL, token: "sk-test"}))
	defer d.Close()

	req := `{"model":"cloud-x","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`
	resp, err := http.Post(d.URL+"/v1/messages", "application/json", strings.NewReader(req))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	out := string(body)

	if !strings.Contains(out, `"type":"error"`) {
		t.Errorf("malformed mid-stream chunk was dropped instead of surfaced as an error:\n%s", out)
	}
	if strings.Contains(out, "message_stop") {
		t.Errorf("a turn that failed mid-stream should not also carry a closing message_stop:\n%s", out)
	}
}

func TestMessagesNotSignedIn(t *testing.T) {
	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Post(d.URL+"/v1/messages", "application/json", strings.NewReader(`{"model":"cloud-x","messages":[]}`))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusUnauthorized {
		t.Errorf("status = %d, want 401", resp.StatusCode)
	}
	if !strings.Contains(string(body), "wally login") {
		t.Errorf("body = %q", body)
	}
}

func TestMessagesGatewayPathModelOverridesBody(t *testing.T) {
	var upstreamBody string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		upstreamBody = string(b)
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"id":"c1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}`)
	}))
	defer upstream.Close()

	d := newTestDaemon(NewRouter(stubSession{base: upstream.URL, token: "sk"}))
	defer d.Close()

	// Body carries the forced desktop model name; the path pins the real one.
	req := `{"model":"claude-sonnet-4-5","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}`
	resp, err := http.Post(d.URL+"/gw/glm-5.3-flash/v1/messages", "application/json", strings.NewReader(req))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	io.ReadAll(resp.Body)

	if !strings.Contains(upstreamBody, `"glm-5.3-flash"`) || strings.Contains(upstreamBody, "claude-sonnet-4-5") {
		t.Errorf("path model did not override the body model; upstream got: %s", upstreamBody)
	}
}
