package harness

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"runtime"

	"github.com/RunanywhereAI/wally/runanywhere"
)

// claudeDesktopProfileID identifies wally's own entry in Claude Desktop's
// third-party profile library. Fixed, so a second run replaces the first
// rather than stacking entries, and so a restore recognizes and removes
// only wally's own profile, leaving any gateway the person configured by
// hand alone. The tail is "RunAny" in hex, so it still reads as ours in a
// file someone opens by hand. Ported from
// wally-legacy/src/desktop/claude_profile.cpp, the shape of which was in
// turn learned from ollama/cmd/launch/claude_desktop.go.
const claudeDesktopProfileID = "00000000-0000-4000-8000-52756e416e79"

// claudeDesktopAdvertisedModel is the id Claude Desktop is told answers,
// because its model picker only accepts a gateway model it can map onto an
// Anthropic family; naming the real id gets "Gateway returned no usable
// models" back from the app. The picker's label still names the real model
// (see applyClaudeDesktopGateway), so nothing about what is actually
// serving is hidden, only which family the app files it under.
const claudeDesktopAdvertisedModel = "claude-sonnet-4-5"

// claudeDesktopProfileKeys are every field applyClaudeDesktopGateway writes
// into the profile document, and so every field a restore has to strip. Any
// other field in the file was there before wally touched it and is left
// alone.
var claudeDesktopProfileKeys = []string{
	"inferenceProvider", "inferenceGatewayBaseUrl", "inferenceGatewayApiKey",
	"inferenceGatewayAuthScheme", "deploymentDisplayName", "inferenceModels",
	"coworkEgressAllowedHosts", "autoModeEnabled", "coworkTabEnabled",
	"modelDiscoveryEnabled",
}

// ClaudeDesktop wires the endpoint into the Claude Desktop app through its
// third-party gateway profile. Unlike claude-code, the app has no env-based
// wiring: it ignores ANTHROPIC_BASE_URL entirely and reads its deployment
// mode and gateway settings from files under its own Application Support
// tree on every launch, so that tree is the only way in, and a running
// instance never sees the change until it is relaunched.
//
// Wire mutates those files directly, there is no temp-copy option here,
// only a revert: PreservesConfig is deliberately not implemented, since
// unlike the other harnesses this one really does touch the person's own
// config, if only for the run. What makes that safe is that the revert is
// exact and field-scoped rather than a whole-file backup, so a change
// Claude Desktop itself makes to the file while it's running is never
// clobbered: Restore only ever removes the keys ApplyGateway added and the
// one profile entry it created.
type ClaudeDesktop struct{}

func (ClaudeDesktop) Wire(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error) {
	if runtime.GOOS != "darwin" {
		return nil, nil, nil, errors.New("claude-desktop is a macOS application")
	}

	token := ep.APIKey
	if token == "" {
		token = "local"
	}
	label := "RunAnywhere: " + model
	// Claude Desktop forces its own model name into the request body and appends
	// /v1/messages to the gateway base itself. Pin the real model in the path so
	// the daemon rewrites it, and give the root (not /v1) to avoid /v1/v1.
	gateway := anthropicBaseURL(ep.BaseURL) + "/gw/" + url.PathEscape(model)
	if err := applyClaudeDesktopGateway(token, gateway, label); err != nil {
		return nil, nil, nil, err
	}

	argv = []string{"-n", "-W", "-a", "Claude"}
	if len(args) > 0 {
		argv = append(argv, "--args")
		argv = append(argv, args...)
	}
	cleanup = func() {
		if err := restoreClaudeDesktopGateway(); err != nil {
			fmt.Fprintln(os.Stderr, "wally: could not restore the claude desktop config:", err)
		}
	}
	return nil, argv, cleanup, nil
}

// InstallCommand opens the app's own download page in the default browser.
// Verified 2026-09-16: https://claude.ai/download 301s to
// https://claude.com/download, which lists the macOS build. There is no
// shell installer for a desktop app; "open" is macOS's own always-present
// command for handing a URL to the default browser, so it is the closest
// thing to a bare, runnable command line that still gets a person to the
// same page a manual download would.
func (ClaudeDesktop) InstallCommand() string {
	return "open https://claude.ai/download"
}

// InstallHint is the exact command InstallCommand runs, so what a person
// sees and what wally executes never drift apart.
func (c ClaudeDesktop) InstallHint() string {
	return "Run: " + c.InstallCommand()
}

// Restore puts Claude Desktop back on Anthropic and strips the profile
// wally wrote, without launching anything. Safe to call when nothing was
// ever applied, and it is exactly what Wire's own cleanup runs, so this
// exists mainly for the run that was interrupted before cleanup could fire.
func (ClaudeDesktop) Restore() error {
	return restoreClaudeDesktopGateway()
}

func init() {
	c := ClaudeDesktop{}
	Registry = append(Registry, Harness{
		Name:    "claude-desktop",
		Command: "open",
		Summary: "open Claude Desktop against a model",
		Wire:    c.Wire,
		Impl:    c,
	})
}

func claudeDesktopHome() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return "", errors.New("no home directory to write the claude desktop profile into")
	}
	return home, nil
}

// claudeDesktopSupportRoot is where the app keeps one of its two config
// trees: the normal one, and "Claude-3p", the third-party gateway tree the
// profile and its library live in. The app decides which mode it is in from
// deploymentMode in the normal tree's claude_desktop_config.json, so both
// get updated together.
func claudeDesktopSupportRoot(home string, thirdParty bool) string {
	name := "Claude"
	if thirdParty {
		name = "Claude-3p"
	}
	return filepath.Join(home, "Library", "Application Support", name)
}

func claudeDesktopProfilePath(home string) string {
	return filepath.Join(claudeDesktopSupportRoot(home, true), "configLibrary", claudeDesktopProfileID+".json")
}

func claudeDesktopMetaPath(home string) string {
	return filepath.Join(claudeDesktopSupportRoot(home, true), "configLibrary", "_meta.json")
}

func claudeDesktopConfigPath(home string, thirdParty bool) string {
	return filepath.Join(claudeDesktopSupportRoot(home, thirdParty), "claude_desktop_config.json")
}

// readClaudeDesktopJSON treats a missing or blank file as an empty object,
// the same as a person who has never opened Claude Desktop's third-party
// mode. A file that exists but fails to parse is reported rather than
// silently replaced: it is the person's own configuration, and overwriting
// it on a read error would be worse than refusing to proceed.
func readClaudeDesktopJSON(path string) (map[string]any, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return map[string]any{}, nil
	}
	if err != nil {
		return nil, err
	}
	if len(bytes.TrimSpace(data)) == 0 {
		return map[string]any{}, nil
	}
	var parsed map[string]any
	if err := json.Unmarshal(data, &parsed); err != nil {
		return nil, fmt.Errorf("could not read %s: %w", path, err)
	}
	return parsed, nil
}

// writeClaudeDesktopJSON writes at 0600: the profile file this can write
// carries the gateway credential, and every file in this tree gets the
// same mode rather than singling one out.
func writeClaudeDesktopJSON(path string, value map[string]any) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')
	return os.WriteFile(path, data, 0o600)
}

func setClaudeDesktopDeploymentMode(path, mode string) error {
	config, err := readClaudeDesktopJSON(path)
	if err != nil {
		return err
	}
	config["deploymentMode"] = mode
	return writeClaudeDesktopJSON(path, config)
}

// claudeDesktopMetaEntriesWithout returns meta's "entries" array with any
// entry whose id matches id removed, tolerating a missing or malformed
// array the same way the app itself would treat one.
func claudeDesktopMetaEntriesWithout(meta map[string]any, id string) []any {
	raw, _ := meta["entries"].([]any)
	entries := make([]any, 0, len(raw))
	for _, e := range raw {
		if entry, ok := e.(map[string]any); ok {
			if entryID, _ := entry["id"].(string); entryID == id {
				continue
			}
		}
		entries = append(entries, e)
	}
	return entries
}

// applyClaudeDesktopGateway writes the gateway profile naming token and
// baseURL, marks it applied, and switches both config trees to third-party
// mode. Takes effect on the app's next launch, never on one already
// running.
func applyClaudeDesktopGateway(token, baseURL, label string) error {
	home, err := claudeDesktopHome()
	if err != nil {
		return err
	}

	profile, err := readClaudeDesktopJSON(claudeDesktopProfilePath(home))
	if err != nil {
		return err
	}
	profile["inferenceProvider"] = "gateway"
	profile["inferenceGatewayBaseUrl"] = baseURL
	profile["inferenceGatewayApiKey"] = token
	profile["inferenceGatewayAuthScheme"] = "bearer"
	profile["deploymentDisplayName"] = label
	profile["chatTabEnabled"] = true
	// Cowork reaches plugins and MCP servers over the network; a profile
	// that does not say so leaves it unable to use them.
	profile["coworkEgressAllowedHosts"] = []any{"*"}
	profile["autoModeEnabled"] = false
	profile["coworkTabEnabled"] = true
	profile["inferenceModels"] = []any{
		map[string]any{"name": claudeDesktopAdvertisedModel, "labelOverride": label},
	}
	// Asked for explicitly: with inferenceModels set, the app would
	// otherwise skip discovery, and the picker would have nothing to
	// reconcile the served model against.
	profile["modelDiscoveryEnabled"] = true
	if err := writeClaudeDesktopJSON(claudeDesktopProfilePath(home), profile); err != nil {
		return err
	}

	meta, err := readClaudeDesktopJSON(claudeDesktopMetaPath(home))
	if err != nil {
		return err
	}
	meta["appliedId"] = claudeDesktopProfileID
	entries := claudeDesktopMetaEntriesWithout(meta, claudeDesktopProfileID)
	entries = append(entries, map[string]any{"id": claudeDesktopProfileID, "name": label})
	meta["entries"] = entries
	if err := writeClaudeDesktopJSON(claudeDesktopMetaPath(home), meta); err != nil {
		return err
	}

	if err := setClaudeDesktopDeploymentMode(claudeDesktopConfigPath(home, true), "3p"); err != nil {
		return err
	}
	return setClaudeDesktopDeploymentMode(claudeDesktopConfigPath(home, false), "3p")
}

// restoreClaudeDesktopGateway puts both config trees back to first-party
// mode and strips wally's own profile entry and fields, leaving everything
// else, including a gateway someone else configured by hand, untouched.
func restoreClaudeDesktopGateway() error {
	home, err := claudeDesktopHome()
	if err != nil {
		// Nothing was ever written without a home directory to write it to.
		return nil
	}

	if err := setClaudeDesktopDeploymentMode(claudeDesktopConfigPath(home, false), "1p"); err != nil {
		return err
	}
	if err := setClaudeDesktopDeploymentMode(claudeDesktopConfigPath(home, true), "1p"); err != nil {
		return err
	}

	meta, err := readClaudeDesktopJSON(claudeDesktopMetaPath(home))
	if err != nil {
		return err
	}
	if len(meta) > 0 {
		if applied, _ := meta["appliedId"].(string); applied == claudeDesktopProfileID {
			delete(meta, "appliedId")
		}
		meta["entries"] = claudeDesktopMetaEntriesWithout(meta, claudeDesktopProfileID)
		if err := writeClaudeDesktopJSON(claudeDesktopMetaPath(home), meta); err != nil {
			return err
		}
	}

	profile, err := readClaudeDesktopJSON(claudeDesktopProfilePath(home))
	if err != nil {
		return err
	}
	if len(profile) == 0 {
		return nil
	}
	for _, key := range claudeDesktopProfileKeys {
		delete(profile, key)
	}
	return writeClaudeDesktopJSON(claudeDesktopProfilePath(home), profile)
}
