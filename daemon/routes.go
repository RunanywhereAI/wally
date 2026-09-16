package daemon

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"net/http"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
	"github.com/RunanywhereAI/wally/version"
)

func (rt *Router) register(mux *http.ServeMux) {
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
		io.WriteString(w, "ok")
	})
	mux.HandleFunc("GET /whoami", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(Identity{
			Version:       version.Version,
			StartedAtUnix: processStart.Unix(),
			Exe:           selfExe(),
		})
	})
	mux.HandleFunc("GET /v1/models", rt.models)
	mux.HandleFunc("POST /v1/chat/completions", func(w http.ResponseWriter, r *http.Request) {
		rt.proxyOpenAI(w, r, "/chat/completions")
	})
	mux.HandleFunc("POST /v1/completions", func(w http.ResponseWriter, r *http.Request) {
		rt.proxyOpenAI(w, r, "/completions")
	})
	mux.HandleFunc("POST /v1/embeddings", func(w http.ResponseWriter, r *http.Request) {
		rt.proxyOpenAI(w, r, "/embeddings")
	})
	mux.HandleFunc("POST /v1/messages", rt.messages)
	// A gateway can pin the model in the path when its client forces its own
	// model name into the body (Claude Desktop).
	mux.HandleFunc("POST /gw/{model}/v1/messages", rt.messages)
	mux.HandleFunc("GET /gw/{model}/v1/models", rt.models)
	mux.HandleFunc("GET /{$}", rt.dashboard)
	mux.HandleFunc("GET /status", rt.status)
}

// openAIModel and openAIModelList mirror the shape Ollama and the OpenAI API
// serve from GET /v1/models, which is what points a harness's model picker at
// this daemon instead of a hardcoded list.
type openAIModel struct {
	ID      string `json:"id"`
	Object  string `json:"object"`
	OwnedBy string `json:"owned_by"`
}

type openAIModelList struct {
	Object string        `json:"object"`
	Data   []openAIModel `json:"data"`
}

// models lists every model this daemon can resolve a request against: the
// cached cloud catalog plus whatever is installed on disk. It never calls
// out, so it carries no endpoint or credential.
func (rt *Router) models(w http.ResponseWriter, _ *http.Request) {
	cloud, _ := catalog.Load()
	installed := runanywhere.InstalledModels()

	data := make([]openAIModel, 0, len(cloud)+len(installed))
	for _, m := range cloud {
		data = append(data, openAIModel{ID: m.ID, Object: "model", OwnedBy: "wally"})
	}
	for _, m := range installed {
		data = append(data, openAIModel{ID: m.ID, Object: "model", OwnedBy: "wally"})
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(openAIModelList{Object: "list", Data: data})
}

// proxyOpenAI resolves the model named in the request body to an Endpoint and
// forwards the request body to that endpoint's OpenAI-compatible path,
// streaming the upstream response back unchanged. /v1/chat/completions,
// /v1/completions, and /v1/embeddings share this because they differ only in
// path and in what the resolved endpoint does with the body.
func (rt *Router) proxyOpenAI(w http.ResponseWriter, r *http.Request, path string) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		writeError(w, http.StatusBadRequest, "could not read the request body")
		return
	}

	var head struct {
		Model string `json:"model"`
	}
	if err := json.Unmarshal(body, &head); err != nil || head.Model == "" {
		writeError(w, http.StatusBadRequest, "the request is missing a model")
		return
	}

	ep, err := rt.endpoint(head.Model)
	if err != nil {
		writeResolveError(w, err)
		return
	}

	upstream, err := http.NewRequestWithContext(r.Context(), http.MethodPost, ep.BaseURL+path, bytes.NewReader(body))
	if err != nil {
		writeError(w, http.StatusInternalServerError, "could not build the upstream request")
		return
	}
	upstream.Header.Set("Content-Type", "application/json")
	if ep.APIKey != "" {
		upstream.Header.Set("Authorization", "Bearer "+ep.APIKey)
	}

	resp, err := rt.client.Do(upstream)
	if err != nil {
		writeError(w, http.StatusBadGateway, "could not reach the model endpoint")
		return
	}
	defer resp.Body.Close()

	if ct := resp.Header.Get("Content-Type"); ct != "" {
		w.Header().Set("Content-Type", ct)
	}
	w.WriteHeader(resp.StatusCode)
	streamCopy(w, resp.Body)
}

func streamCopy(w http.ResponseWriter, r io.Reader) {
	rc := http.NewResponseController(w)
	buf := make([]byte, 4096)
	for {
		n, readErr := r.Read(buf)
		if n > 0 {
			if _, writeErr := w.Write(buf[:n]); writeErr != nil {
				return
			}
			rc.Flush()
		}
		if readErr != nil {
			return
		}
	}
}

func writeResolveError(w http.ResponseWriter, err error) {
	switch {
	case errors.Is(err, runanywhere.ErrNotSignedIn):
		writeError(w, http.StatusUnauthorized, errmap.NewNotSignedIn().Error())
	case errors.Is(err, runanywhere.ErrOnDeviceNotEnabled):
		writeError(w, http.StatusNotImplemented, errmap.NewOnDeviceNotEnabled().Error())
	default:
		writeError(w, http.StatusBadGateway, err.Error())
	}
}

func writeError(w http.ResponseWriter, status int, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(map[string]any{
		"error": map[string]any{"message": message, "type": "wally_error"},
	})
}
