package daemon

import (
	"embed"
	"encoding/json"
	"net/http"
	"os/exec"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/config"
	"github.com/RunanywhereAI/wally/harness"
	"github.com/RunanywhereAI/wally/runanywhere"
)

//go:embed web/index.html
var webAssets embed.FS

// dashboard serves the single embedded HTML page. It carries no endpoint or
// credential of its own; everything it shows comes from status, fetched by
// the page's own script.
func (rt *Router) dashboard(w http.ResponseWriter, _ *http.Request) {
	data, err := webAssets.ReadFile("web/index.html")
	if err != nil {
		http.Error(w, "the dashboard page is unavailable", http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Write(data)
}

type onDeviceModelStatus struct {
	ID          string `json:"id"`
	Framework   string `json:"framework"`
	ToolCalling bool   `json:"tool_calling"`
	Text        bool   `json:"text"`
}

type harnessStatus struct {
	Name        string `json:"name"`
	Command     string `json:"command"`
	Installed   bool   `json:"installed"`
	InstallHint string `json:"install_hint,omitempty"`
}

// dashboardStatus is what GET /status serves. It never carries a console URL,
// a web origin, or a token: those are exactly what wally info was scrubbed of,
// and a locally served dashboard is no less exposed than a printed one.
type dashboardStatus struct {
	OK              bool                  `json:"ok"`
	Channel         string                `json:"channel"`
	OnDeviceEnabled bool                  `json:"on_device_enabled"`
	CloudModels     []string              `json:"cloud_models"`
	OnDeviceModels  []onDeviceModelStatus `json:"on_device_models"`
	Harnesses       []harnessStatus       `json:"harnesses"`
}

func (rt *Router) status(w http.ResponseWriter, _ *http.Request) {
	// A missing or unreadable cache reads as "nothing cached yet", not an
	// error the dashboard needs to surface; catalog.Load documents the same
	// contract for its other callers.
	cloud, _ := catalog.Load()
	cloudIDs := make([]string, 0, len(cloud))
	for _, m := range cloud {
		cloudIDs = append(cloudIDs, m.ID)
	}

	installed := runanywhere.InstalledModels()
	onDevice := make([]onDeviceModelStatus, 0, len(installed))
	for _, m := range installed {
		onDevice = append(onDevice, onDeviceModelStatus{
			ID:          m.ID,
			Framework:   m.Framework,
			ToolCalling: runanywhere.SupportsToolCalling(m.ID),
			Text:        runanywhere.IsTextGenerationModel(m),
		})
	}

	harnesses := make([]harnessStatus, 0, len(harness.Registry))
	for _, h := range harness.Registry {
		hs := harnessStatus{Name: h.Name, Command: h.Command}
		_, lookErr := exec.LookPath(h.Command)
		hs.Installed = lookErr == nil
		if !hs.Installed {
			if hint, ok := h.InstallHint(); ok {
				hs.InstallHint = hint
			}
		}
		harnesses = append(harnesses, hs)
	}

	resp := dashboardStatus{
		OK:              true,
		Channel:         config.Channel(),
		OnDeviceEnabled: runanywhere.OnDeviceEnabled(),
		CloudModels:     cloudIDs,
		OnDeviceModels:  onDevice,
		Harnesses:       harnesses,
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(resp)
}
