package cmd

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"text/tabwriter"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func newModelsCmd() *cobra.Command {
	models := &cobra.Command{
		Use:     "models",
		Short:   "List, inspect, pull, and remove on-device models",
		GroupID: groupModels,
	}
	models.AddCommand(
		newModelsListCmd(),
		newModelsShowCmd(),
		newModelsRmCmd(),
		newModelsPullCmd(),
	)
	return models
}

func newModelsListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List models available in the cloud and installed on this machine",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			out := cmd.OutOrStdout()
			online, err := catalogLoad()
			if err != nil {
				return err
			}
			if err := renderOnlineModels(out, online); err != nil {
				return err
			}
			fmt.Fprintln(out)
			installed := runanywhere.InstalledModels()
			if err := renderOfflineModels(out, installed); err != nil {
				return err
			}
			if avail := runanywhere.DownloadableModels(); len(avail) > 0 {
				fmt.Fprintln(out)
				renderDownloadableModels(out, avail, installed)
			}
			return nil
		},
	}
}

// renderOnlineModels prints the cloud catalog the daemon keeps cached
// locally (catalogLoad, never a live console call: this list has to render
// instantly and work offline). An empty cache points at wally login rather
// than showing an empty table.
func renderOnlineModels(out io.Writer, models []catalog.Model) error {
	fmt.Fprintln(out, "ONLINE (cloud)")
	if len(models) == 0 {
		fmt.Fprintln(out, "Cloud catalog not cached yet. Run: wally login")
		return nil
	}
	w := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
	fmt.Fprintln(w, "ID\tINPUT $/Mtok\tOUTPUT $/Mtok")
	for _, m := range models {
		fmt.Fprintf(w, "%s\t%.2f\t%.2f\n", m.ID, float64(m.InputPerMTok)/1_000_000, float64(m.OutputPerMTok)/1_000_000)
	}
	return w.Flush()
}

func renderOfflineModels(out io.Writer, installed []runanywhere.InstalledModel) error {
	fmt.Fprintln(out, "OFFLINE (on-device)")
	if len(installed) == 0 {
		fmt.Fprintln(out, "No models installed. Run: wally models pull <id>")
		return nil
	}
	w := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
	fmt.Fprintln(w, "ID\tFRAMEWORK\tSIZE\tTOOL CALLING")
	for _, m := range installed {
		fmt.Fprintf(w, "%s\t%s\t%s\t%s\n", m.ID, m.Framework, formatBytes(m.Bytes), toolCallingLabel(m.ID))
	}
	return w.Flush()
}

// renderDownloadableModels lists the ids `wally models pull` accepts, marking
// the ones already on disk so the person knows what is left to fetch.
func renderDownloadableModels(out io.Writer, ids []string, installed []runanywhere.InstalledModel) error {
	have := make(map[string]bool, len(installed))
	for _, m := range installed {
		have[strings.ToLower(m.ID)] = true
	}
	fmt.Fprintln(out, "AVAILABLE TO DOWNLOAD (wally models pull <id>)")
	w := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
	fmt.Fprintln(w, "ID\tSTATUS")
	for _, id := range ids {
		status := ""
		if have[strings.ToLower(id)] {
			status = "installed"
		}
		fmt.Fprintf(w, "%s\t%s\n", id, status)
	}
	return w.Flush()
}

func newModelsShowCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "show <id>",
		Short: "Show details for one installed model",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			id := args[0]
			m, ok := findInstalledModel(id)
			if !ok {
				return errmap.NewModelNotInstalled(id)
			}

			w := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 0, 2, ' ', 0)
			fmt.Fprintf(w, "id\t%s\n", m.ID)
			fmt.Fprintf(w, "framework\t%s\n", m.Framework)
			fmt.Fprintf(w, "directory\t%s\n", m.Dir)
			fmt.Fprintf(w, "weights\t%s\n", m.Path)
			fmt.Fprintf(w, "size\t%s\n", formatBytes(m.Bytes))
			fmt.Fprintf(w, "tool calling\t%s\n", toolCallingLabel(m.ID))
			return w.Flush()
		},
	}
}

func newModelsRmCmd() *cobra.Command {
	var yes bool
	c := &cobra.Command{
		Use:   "rm <id>",
		Short: "Delete an installed model",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			id := args[0]
			m, ok := findInstalledModel(id)
			if !ok {
				return errmap.NewModelNotInstalled(id)
			}

			out := cmd.OutOrStdout()
			if !yes {
				confirm, err := newStdinPrompter(cmd.InOrStdin(), out).Confirm(
					fmt.Sprintf("Delete %s (%s)?", m.ID, formatBytes(m.Bytes)))
				if err != nil {
					return err
				}
				if !confirm {
					fmt.Fprintln(out, "Nothing removed.")
					return nil
				}
			}

			freed, err := removeInstalledModel(m)
			if err != nil {
				return err
			}
			fmt.Fprintf(out, "Removed %s, freed %s.\n", m.ID, formatBytes(freed))
			return nil
		},
	}
	c.Flags().BoolVarP(&yes, "yes", "y", false, "delete without asking for confirmation")
	return c
}

// removeInstalledModel deletes m's on-disk directory and reports the bytes
// freed. `wally models rm` calls it after its own confirm.
func removeInstalledModel(m runanywhere.InstalledModel) (int64, error) {
	freed := m.Bytes
	if err := os.RemoveAll(m.Dir); err != nil {
		return 0, fmt.Errorf("removing %s: %w", m.Dir, err)
	}
	return freed, nil
}

func newModelsPullCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "pull <id>",
		Short: "Download a model into the on-device store",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			id := args[0]
			out := cmd.OutOrStdout()

			// Already on disk: the installed id (kit-derived, e.g. LFM2-350M-Q8_0)
			// differs from the catalog pull id (lfm2-350m-q8_0), so match loosely
			// and say so instead of "unknown model".
			for _, m := range runanywhere.InstalledModels() {
				if strings.EqualFold(m.ID, id) {
					fmt.Fprintf(out, "%s is already installed.\n", m.ID)
					return nil
				}
			}

			if runanywhere.DownloadEnabled() {
				resolved := id
				if !strings.HasPrefix(strings.ToLower(id), "http") {
					avail := runanywhere.DownloadableModels()
					match := ""
					for _, c := range avail {
						if strings.EqualFold(c, id) {
							match = c
							break
						}
					}
					if match == "" {
						return fmt.Errorf("unknown model %q. Run `wally models list` and use an id from the AVAILABLE TO DOWNLOAD section, or pass a raw https:// URL", id)
					}
					resolved = match
				}
				return pullModelCLI(cmd.Context(), resolved, out)
			}
			return runModelsPull(id, cmd.InOrStdin(), out, cmd.ErrOrStderr())
		},
	}
}

// pullModelCLI runs the real downloader with a single-line progress readout,
// for `wally models pull`. The models TUI uses its own bubbletea bar.
func pullModelCLI(ctx context.Context, id string, out io.Writer) error {
	fmt.Fprintf(out, "Downloading %s...\n", id)
	err := runanywhere.DownloadModel(ctx, id, func(p runanywhere.DownloadProgress) {
		if p.Total > 0 {
			fmt.Fprintf(out, "\r%.1f%% (%s / %s)      ", float64(p.Downloaded)/float64(p.Total)*100, formatBytes(p.Downloaded), formatBytes(p.Total))
		}
	})
	fmt.Fprintln(out)
	if err != nil {
		return err
	}
	fmt.Fprintf(out, "Downloaded %s\n", id)
	return nil
}

// runModelsPull is the script-seam download path: WALLY_PULL_SCRIPT or
// scripts/pull-model.sh, run with id and the on-device store as its
// arguments, output streamed live. Both `wally models pull` and
// pullModelWithProgress's no-downloader fallback share it, so a build
// without the linked runanywhere.Downloader keeps behaving exactly as this
// command line always has.
func runModelsPull(id string, in io.Reader, out, errOut io.Writer) error {
	script := pullScriptPath()
	if script == "" {
		return fmt.Errorf(
			"%w for %q. Set WALLY_PULL_SCRIPT to a downloader, or place one at scripts/pull-model.sh; "+
				"until then, fetch the model's weights by hand into %s",
			ErrModelPullNotWired, id, filepath.Join(runanywhere.ModelsRoot(), "<framework>", id))
	}

	dest := runanywhere.ModelsRoot()
	c := exec.Command(script, id, dest)
	c.Stdout = out
	c.Stderr = errOut
	c.Stdin = in
	if err := c.Run(); err != nil {
		return fmt.Errorf("running %s: %w", script, err)
	}
	return nil
}

// pullScriptPath resolves the pull-model script seam: WALLY_PULL_SCRIPT
// overrides it, otherwise it is scripts/pull-model.sh next to the wally
// binary's working tree. Empty when neither exists or is executable, which
// callers treat as the download path not being wired into this build yet.
func pullScriptPath() string {
	if p := os.Getenv("WALLY_PULL_SCRIPT"); p != "" {
		if isExecutableFile(p) {
			return p
		}
		return ""
	}
	p := filepath.Join("scripts", "pull-model.sh")
	if isExecutableFile(p) {
		return p
	}
	return ""
}

func isExecutableFile(p string) bool {
	fi, err := os.Stat(p)
	if err != nil || fi.IsDir() {
		return false
	}
	return fi.Mode()&0o111 != 0
}

func findInstalledModel(id string) (runanywhere.InstalledModel, bool) {
	for _, m := range runanywhere.InstalledModels() {
		if m.ID == id {
			return m, true
		}
	}
	return runanywhere.InstalledModel{}, false
}

func toolCallingLabel(id string) string {
	if runanywhere.SupportsToolCalling(id) {
		return "yes"
	}
	return "no"
}

// ErrModelPullNotWired reports that no download seam is configured, so
// "wally models pull" cannot fetch the model itself in this build.
var ErrModelPullNotWired = errors.New("on-device model download is not wired into this build yet")
