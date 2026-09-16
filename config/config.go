package config

// Set at build time via -ldflags "-X github.com/RunanywhereAI/wally/config.BakedConsoleAPIURL=...".
// Empty in a normal build, where the defaults below apply.
var (
	BakedConsoleAPIURL    = ""
	BakedConsoleWebOrigin = ""
)

const (
	defaultConsoleAPIURL    = "https://inference.runanywhere.ai"
	defaultConsoleWebOrigin = "https://console.runanywhere.ai"
)

func ConsoleAPIURL() string {
	if BakedConsoleAPIURL != "" {
		return BakedConsoleAPIURL
	}
	return defaultConsoleAPIURL
}

func ConsoleWebOrigin() string {
	if BakedConsoleWebOrigin != "" {
		return BakedConsoleWebOrigin
	}
	return defaultConsoleWebOrigin
}

// Channel reports the build channel without revealing any endpoint: a dev build
// bakes a console URL, a release build bakes nothing.
func Channel() string {
	if BakedConsoleAPIURL != "" {
		return "development"
	}
	return "production"
}
