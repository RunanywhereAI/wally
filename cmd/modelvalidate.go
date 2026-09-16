package cmd

import (
	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
)

// catalogLoad is catalog.Load, indirected so tests can substitute a fixed
// catalog without depending on the cache file's on-disk shape.
var catalogLoad = catalog.Load

// validateModel reports errmap.NewModelNotAvailable when model is neither an
// installed on-device model nor a cloud model in the daemon's cached
// catalog. It never calls the console directly: catalog.Load reads a file
// the daemon keeps refreshed in the background, so a bad model name is
// caught instantly and a slow or unreachable console can never hang a
// launch or a default-model save on this path. An empty cache (nothing
// fetched yet, or offline before the first refresh) skips validation rather
// than rejecting every cloud model name.
func validateModel(model string) error {
	if runanywhere.IsLocalModel(model) {
		if !runanywhere.OnDeviceEnabled() {
			return errmap.NewOnDeviceNotEnabled()
		}
		m, _ := runanywhere.FindInstalledModel(model)
		switch {
		case m.Path == "":
			return errmap.NewModelNotInstalled(model)
		case !runanywhere.EngineServable(m):
			return errmap.NewModelBackendUnsupported(model)
		}
		return nil
	}
	models, err := catalogLoad()
	if err != nil {
		return nil
	}
	if len(models) == 0 {
		models = warmCatalog()
	}
	if len(models) == 0 {
		return nil
	}
	if catalog.Contains(models, model) {
		return nil
	}
	return errmap.NewModelNotAvailable(model)
}
