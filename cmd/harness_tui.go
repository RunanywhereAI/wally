package cmd

import (
	"bufio"
	"errors"
	"fmt"
	"io"
	"os/exec"
	"strings"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/config"
	"github.com/RunanywhereAI/wally/harness"
	"github.com/RunanywhereAI/wally/runanywhere"
	"github.com/RunanywhereAI/wally/tui"
)

// harnessAction is one row of the per-harness action menu. Install actually
// runs the installer (ensureHarnessInstalled, shared with the launcher);
// Uninstall runs the harness's own uninstaller, only after a destructive
// confirm, then clears wally's own stored state for it.
type harnessAction int

const (
	actionInstall harnessAction = iota
	actionUninstall
	actionSetDefault
)

// wallyChatHarness is not a registered harness: `wally chat` is wally's own
// built-in chat, always present, never installed or uninstalled through this
// menu. It stands in only so pickDefaultModel can name it in a title and a
// prompt the same way it names any registered harness.
var wallyChatHarness = harness.Harness{Name: "wally chat"}

// harnessManagerRow is one entry in the top-level `wally harness` list: the
// global wally chat default, or a registered harness. Only harness is set
// when isGlobal is false.
type harnessManagerRow struct {
	isGlobal bool
	harness  harness.Harness
}

func harnessInstalled(h harness.Harness) bool {
	_, err := exec.LookPath(h.Command)
	return err == nil
}

// loadGlobalDefaultModel and saveGlobalDefaultModel read and write
// config.Prefs.DefaultModel directly: the same field selectModel falls back
// to for any harness with no default of its own, and the same field `wally
// chat` launches against. The manager's "wally chat" row is a front end onto
// this one value, not a separate store.
func loadGlobalDefaultModel() (string, error) {
	prefs, err := config.LoadPrefs()
	if err != nil {
		return "", err
	}
	return prefs.DefaultModel, nil
}

func saveGlobalDefaultModel(model string) error {
	prefs, err := config.LoadPrefs()
	if err != nil {
		return err
	}
	prefs.DefaultModel = model
	return config.SavePrefs(prefs)
}

// harnessMenuItems lists wally chat first, then every registered harness,
// each item paired with the harnessManagerRow that says what it is, so the
// manager loop never has to re-derive that from an index into
// harness.Registry.
func harnessMenuItems() ([]tui.Item, []harnessManagerRow, error) {
	rows := make([]harnessManagerRow, 0, len(harness.Registry)+1)
	items := make([]tui.Item, 0, len(harness.Registry)+1)

	globalModel, err := loadGlobalDefaultModel()
	if err != nil {
		return nil, nil, err
	}
	detail := "wally's own chat; the default for `wally chat` and the fallback for any harness below with no default of its own"
	if globalModel != "" {
		detail += ", default model: " + globalModel
	}
	items = append(items, tui.Item{Title: "wally chat", Detail: detail})
	rows = append(rows, harnessManagerRow{isGlobal: true})

	for _, h := range harness.Registry {
		status := "not installed"
		if harnessInstalled(h) {
			status = "installed"
		}
		model, err := loadDefaultModel(h.Name)
		if err != nil {
			return nil, nil, err
		}
		detail := h.Summary + " (" + status + ")"
		if model != "" {
			detail += ", default model: " + model
		}
		items = append(items, tui.Item{Title: h.Name, Detail: detail})
		rows = append(rows, harnessManagerRow{harness: h})
	}
	return items, rows, nil
}

func harnessActionItems(h harness.Harness, installed bool) ([]tui.Item, []harnessAction) {
	items := make([]tui.Item, 0, 3)
	keys := make([]harnessAction, 0, 3)

	if !installed {
		item := tui.Item{Title: "Install"}
		if hint, ok := h.InstallHint(); ok {
			item.Detail = hint
		} else {
			item.Detail = "no install instructions available for this harness"
			item.Disabled = true
		}
		items = append(items, item)
		keys = append(keys, actionInstall)
	}

	uninstall := tui.Item{Title: "Uninstall"}
	if command, ok := h.UninstallCommand(); ok {
		uninstall.Detail = "removes the tool itself; runs: " + command
	} else {
		uninstall.Detail = "no automatic uninstaller for this tool; choosing this explains how to remove it by hand"
	}
	items = append(items, uninstall)
	keys = append(keys, actionUninstall)

	items = append(items, tui.Item{
		Title:  "Set default model",
		Detail: "store the model wally launches this harness against by default",
	})
	keys = append(keys, actionSetDefault)

	return items, keys
}

// globalChatActionItems is the action menu for the wally chat row: only its
// default model can be managed here, since wally chat is built into wally
// itself and is never installed or uninstalled through this menu.
func globalChatActionItems() ([]tui.Item, []harnessAction) {
	return []tui.Item{{
		Title:  "Set default model",
		Detail: "store the model `wally chat` uses by default, and the fallback for any harness without its own",
	}}, []harnessAction{actionSetDefault}
}

// removeHarness clears wally's own state for h and resets any wiring a
// harness left behind (Restore, a no-op for a harness that isn't
// Restorable). It never touches the harness's install; it is the cleanup
// step that follows a successful uninstall, not the uninstall itself.
func removeHarness(h harness.Harness) (string, error) {
	existed, err := clearHarnessState(h.Name)
	if err != nil {
		return "", err
	}
	if err := h.Restore(); err != nil {
		return "", fmt.Errorf("reset failed: %w", err)
	}
	if existed {
		return "cleared the stored default model and reset any wiring left on this harness", nil
	}
	return "nothing stored for this harness; reset any wiring left just in case", nil
}

// runHarnessUninstall runs h's own UninstallCommand through the same shell
// runner the installer uses (runInstall, cmd/install.go), so a person
// watches a removal the same way they watch an install. The caller
// (runHarnessManager) never reaches this without confirming first and
// without h.UninstallCommand() already having reported ok.
func runHarnessUninstall(h harness.Harness, in io.Reader, out, errOut io.Writer) error {
	command, ok := h.UninstallCommand()
	if !ok {
		return fmt.Errorf("%s has no automatic uninstaller", h.Name)
	}
	if err := checkInstallerDependencies(command); err != nil {
		return err
	}
	fmt.Fprintf(errOut, "\nUninstalling %s...\n", h.Name)
	if err := runInstall(command, out, errOut, in); err != nil {
		return fmt.Errorf("uninstalling %s: %w", h.Name, err)
	}
	fmt.Fprintf(errOut, "%s%s uninstalled%s\n\n", ansiGreen, h.Name, ansiReset)
	return nil
}

// modelPickerRow pairs a picker list item with what choosing it means: a
// concrete model id, or the free-text fallback when manual is true. A
// disabled row (a section header, or a note in an empty section) carries
// neither and is never reachable through the list widget.
type modelPickerRow struct {
	item   tui.Item
	id     string
	manual bool
}

// buildModelPickerRows lays out the categorized default-model picker: cloud
// models from the daemon's cached catalog, on-device models from the local
// store, and a free-text fallback row. online empty (nothing cached yet, or
// never signed in) renders a note instead of an empty section; this never
// triggers a live console fetch, so the picker opens instantly and works
// offline.
func buildModelPickerRows(online []catalog.Model, offline []runanywhere.InstalledModel) []modelPickerRow {
	rows := []modelPickerRow{{item: tui.Item{Title: "ONLINE (cloud)", Disabled: true, Accent: tui.AccentPrimary}}}
	if len(online) == 0 {
		rows = append(rows, modelPickerRow{item: tui.Item{Title: "  cloud catalog not cached yet; run wally login", Disabled: true}})
	} else {
		for _, m := range online {
			rows = append(rows, modelPickerRow{item: tui.Item{Title: m.ID, Detail: formatCatalogPrice(m)}, id: m.ID})
		}
	}

	// A default model must be a text-generation LLM: embeddings, STT, TTS,
	// VAD and diarization models installed for other on-device features have
	// no business showing up as a harness default.
	offline = runanywhere.TextGenerationModels(offline)
	runnable := make([]runanywhere.InstalledModel, 0, len(offline))
	for _, m := range offline {
		if runanywhere.EngineServable(m) {
			runnable = append(runnable, m)
		}
	}
	offline = runnable

	rows = append(rows, modelPickerRow{item: tui.Item{Title: "OFFLINE (on-device)", Disabled: true, Accent: tui.AccentSecondary}})
	switch {
	case !runanywhere.OnDeviceEnabled():
		rows = append(rows, modelPickerRow{item: tui.Item{Title: "  on-device inference is not enabled in this build yet", Disabled: true}})
	case len(offline) == 0:
		rows = append(rows, modelPickerRow{item: tui.Item{Title: "  no models installed", Disabled: true}})
	default:
		for _, m := range offline {
			detail := m.Framework + ", no tool calling"
			if runanywhere.SupportsToolCalling(m.ID) {
				detail = m.Framework + ", supports tool calling"
			}
			rows = append(rows, modelPickerRow{item: tui.Item{Title: m.ID, Detail: detail}, id: m.ID})
		}
	}

	rows = append(rows, modelPickerRow{item: tui.Item{Title: "Enter manually..."}, manual: true})
	return rows
}

// formatCatalogPrice renders a cached catalog price as dollars per million
// tokens. InputPerMTok/OutputPerMTok are micro-dollars (1 USD = 1,000,000
// micros), matching the console's own units.
func formatCatalogPrice(m catalog.Model) string {
	return fmt.Sprintf("$%.2f in / $%.2f out per Mtok", float64(m.InputPerMTok)/1_000_000, float64(m.OutputPerMTok)/1_000_000)
}

// pickDefaultModel shows the categorized picker and, when the person chooses
// "Enter manually...", falls back to readModel for a free-text name.
// catalog.Load reads a local cache the daemon keeps refreshed in the
// background, so this never blocks on the console.
func pickDefaultModel(h harness.Harness, readModel func(prompt string) (string, error)) (string, error) {
	online, _ := catalogLoad() // an error is treated the same as an empty cache: render the note, never block
	rows := buildModelPickerRows(online, runanywhere.InstalledModels())
	items := make([]tui.Item, len(rows))
	for i, r := range rows {
		items[i] = r.item
	}

	idx, err := tui.RunList("Default model for "+h.Name, items, "")
	if err != nil {
		return "", err
	}
	row := rows[idx]
	if row.manual {
		return readModel(fmt.Sprintf("Default model for %s: ", h.Name))
	}
	return row.id, nil
}

// setDefaultModel runs the shared pick-validate-save sequence for one row:
// wally chat (global) or a registered harness. name is only used for the
// messages printed to out.
func setDefaultModel(
	out io.Writer,
	name string,
	pickModel func(h harness.Harness) (string, error),
	h harness.Harness,
	save func(model string) error,
) error {
	model, err := pickModel(h)
	if errors.Is(err, tui.ErrCancelled) {
		return nil
	}
	if err != nil {
		return err
	}
	model = strings.TrimSpace(model)
	if model == "" {
		fmt.Fprintln(out, "No model entered; unchanged.")
		return nil
	}
	if err := validateModel(model); err != nil {
		fmt.Fprintf(out, "%s: %v\n", name, err)
		return nil
	}
	if err := save(model); err != nil {
		return err
	}
	fmt.Fprintf(out, "%s: default model set to %s\n", name, model)
	return nil
}

// runHarnessManager is the full `wally harness` loop: pick a row (wally chat
// or a harness), pick an action, act, and return to the list until the
// person cancels out. selectHarness/selectAction/pickModel/installHarness/
// confirmUninstall/uninstallHarness are injected so this is testable without
// a terminal.
func runHarnessManager(
	out io.Writer,
	selectHarness func(title string, items []tui.Item, help string) (int, error),
	selectAction func(title string, items []tui.Item, help string) (int, error),
	pickModel func(h harness.Harness) (string, error),
	installHarness func(h harness.Harness) error,
	confirmUninstall func(prompt string) (bool, error),
	uninstallHarness func(h harness.Harness) error,
) error {
	if len(harness.Registry) == 0 {
		fmt.Fprintln(out, "No harnesses are registered.")
		return nil
	}

	for {
		items, rows, err := harnessMenuItems()
		if err != nil {
			return err
		}
		idx, err := selectHarness("Harnesses", items, "")
		if errors.Is(err, tui.ErrCancelled) {
			return nil
		}
		if err != nil {
			return err
		}
		row := rows[idx]
		name := "wally chat"
		if !row.isGlobal {
			name = row.harness.Name
		}

		var actionItems []tui.Item
		var actionKeys []harnessAction
		if row.isGlobal {
			actionItems, actionKeys = globalChatActionItems()
		} else {
			actionItems, actionKeys = harnessActionItems(row.harness, harnessInstalled(row.harness))
		}
		aidx, err := selectAction("Manage "+name, actionItems, "")
		if errors.Is(err, tui.ErrCancelled) {
			continue
		}
		if err != nil {
			return err
		}

		switch actionKeys[aidx] {
		case actionInstall:
			if err := installHarness(row.harness); err != nil {
				fmt.Fprintf(out, "%s: could not install: %v\n", row.harness.Name, err)
			}
		case actionUninstall:
			h := row.harness
			if _, ok := h.UninstallCommand(); !ok {
				fmt.Fprintf(out, "%s: no automatic uninstaller for this tool; remove it manually the same way you installed it (its own package manager or app uninstaller, or deleting its binary from PATH).\n", h.Name)
				continue
			}
			yes, err := confirmUninstall(fmt.Sprintf("Uninstall %s from this machine? This removes the tool itself, not just wally's setup.", h.Name))
			if errors.Is(err, tui.ErrCancelled) {
				continue
			}
			if err != nil {
				return err
			}
			if !yes {
				continue
			}
			if err := uninstallHarness(h); err != nil {
				fmt.Fprintf(out, "%s: could not uninstall: %v\n", h.Name, err)
				continue
			}
			note, err := removeHarness(h)
			if err != nil {
				fmt.Fprintf(out, "%s: %v\n", h.Name, err)
				continue
			}
			fmt.Fprintf(out, "%s: uninstalled; %s\n", h.Name, note)
		case actionSetDefault:
			h := wallyChatHarness
			save := saveGlobalDefaultModel
			if !row.isGlobal {
				h = row.harness
				save = func(model string) error { return saveDefaultModel(h.Name, model) }
			}
			if err := setDefaultModel(out, name, pickModel, h, save); err != nil {
				return err
			}
		}
	}
}

func findHarness(name string) (harness.Harness, bool) {
	for _, h := range harness.Registry {
		if h.Name == name {
			return h, true
		}
	}
	return harness.Harness{}, false
}

func newHarnessCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:     "harness",
		Short:   "Install, remove, and configure the coding harnesses wally can launch",
		GroupID: groupManage,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			in := cmd.InOrStdin()
			out := cmd.OutOrStdout()
			errOut := cmd.ErrOrStderr()
			reader := bufio.NewReader(in)
			readModel := func(prompt string) (string, error) {
				fmt.Fprint(out, prompt)
				line, err := reader.ReadString('\n')
				if err != nil && !errors.Is(err, io.EOF) {
					return "", err
				}
				return strings.TrimSpace(line), nil
			}
			pickModel := func(h harness.Harness) (string, error) {
				return pickDefaultModel(h, readModel)
			}
			installHarness := func(h harness.Harness) error {
				return ensureHarnessInstalled(h, in, out, errOut)
			}
			confirmUninstall := func(prompt string) (bool, error) {
				return tui.RunConfirm(prompt, false, true)
			}
			uninstallHarness := func(h harness.Harness) error {
				return runHarnessUninstall(h, in, out, errOut)
			}
			return runHarnessManager(out, tui.RunList, tui.RunList, pickModel, installHarness, confirmUninstall, uninstallHarness)
		},
	}
	cmd.AddCommand(newHarnessSetDefaultModelCmd())
	return cmd
}

func newHarnessSetDefaultModelCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "set-default-model <harness> <model>",
		Short: "Set a harness's default model without the interactive menu",
		Args:  cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			name, model := args[0], args[1]
			if _, ok := findHarness(name); !ok {
				return fmt.Errorf("no harness named %q is registered", name)
			}
			if err := validateModel(model); err != nil {
				return err
			}
			if err := saveDefaultModel(name, model); err != nil {
				return err
			}
			fmt.Fprintf(cmd.OutOrStdout(), "%s: default model set to %s\n", name, model)
			return nil
		},
	}
}
