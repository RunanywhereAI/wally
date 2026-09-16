package cmd

import (
	"io"
	"os"
	"os/exec"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/daemon"
	"github.com/RunanywhereAI/wally/harness"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func launchHarness(cmd *cobra.Command, h harness.Harness, explicitModel string, passthrough []string) error {
	out := cmd.OutOrStdout()

	if err := ensureHarnessInstalled(h, cmd.InOrStdin(), out, cmd.ErrOrStderr()); err != nil {
		return err
	}

	explicitModel, err := resolveExplicitModel(explicitModel, h.Name)
	if err != nil {
		return err
	}

	sess, err := newSession()
	if err != nil {
		return err
	}

	model, err := selectModel(explicitModel, sess.signedIn(), newStdinPrompter(cmd.InOrStdin(), out), out, colorFor(out))
	if err != nil {
		return err
	}
	if err := validateModel(model); err != nil {
		return err
	}

	if err := ensureDaemon(); err != nil {
		return err
	}

	ep := runanywhere.Endpoint{BaseURL: daemon.LocalBaseURL(), Local: true}
	env, argv, cleanup, err := h.Wire(ep, model, passthrough)
	if err != nil {
		return err
	}
	defer cleanup()

	child := exec.Command(h.Command, argv...)
	child.Env = append(os.Environ(), env...)
	child.Stdin = os.Stdin
	child.Stdout = os.Stdout
	child.Stderr = os.Stderr
	return child.Run()
}

// resolveExplicitModel returns explicit unchanged when the caller named one
// (a positional arg or --model flag). Otherwise it falls back to
// harnessName's own stored default, set through `wally harness`. An empty
// result still leaves room for selectModel's own further fallbacks: the
// global config.Prefs.DefaultModel, then the interactive prompt.
func resolveExplicitModel(explicit, harnessName string) (string, error) {
	if explicit != "" {
		return explicit, nil
	}
	return loadDefaultModel(harnessName)
}

func colorFor(w io.Writer) bool {
	if _, noColor := os.LookupEnv("NO_COLOR"); noColor {
		return false
	}
	if f, ok := w.(*os.File); ok {
		return isTerminal(f)
	}
	return false
}
