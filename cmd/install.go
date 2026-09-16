package cmd

import (
	"errors"
	"fmt"
	"io"
	"os/exec"
	"runtime"
	"strings"

	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/harness"
)

const ansiGreen = "\x1b[32m"

// runInstall runs command through the platform shell, streaming its stdio
// live so the person watches the installer run instead of staring at a
// blank terminal until it exits. Matches ollama/cmd/launch's own install
// wrapper: bash -c on unix, PowerShell on Windows, rather than sh, since a
// harness's InstallCommand pipes into or otherwise assumes bash syntax.
func runInstall(command string, out, errOut io.Writer, in io.Reader) error {
	var c *exec.Cmd
	if runtime.GOOS == "windows" {
		c = exec.Command("powershell", "-NoProfile", "-Command", command)
	} else {
		c = exec.Command("bash", "-c", command)
	}
	c.Stdout = out
	c.Stderr = errOut
	c.Stdin = in
	return c.Run()
}

// checkInstallerDependencies reports a missing interpreter or fetcher before
// a harness install is even attempted, so the failure names what to install
// rather than surfacing as an inscrutable "command not found" partway
// through the pipeline.
func checkInstallerDependencies(command string) error {
	if runtime.GOOS == "windows" {
		if _, err := exec.LookPath("powershell"); err != nil {
			return errors.New("PowerShell is required to install this tool: https://learn.microsoft.com/powershell/")
		}
		return nil
	}
	if _, err := exec.LookPath("bash"); err != nil {
		return errors.New("bash is required to install this tool: https://www.gnu.org/software/bash/")
	}
	if strings.Contains(command, "curl") {
		if _, err := exec.LookPath("curl"); err != nil {
			return errors.New("curl is required to install this tool: https://curl.se/")
		}
	}
	if strings.Contains(command, "npm") {
		if _, err := exec.LookPath("npm"); err != nil {
			return errors.New("npm is required to install this tool: https://nodejs.org/")
		}
	}
	return nil
}

// ensureHarnessInstalled makes sure h.Command resolves on PATH, prompting to
// install it and actually running the installer when it does not. It is the
// one place both the launcher (cmd/launch.go) and the harness TUI manager
// (cmd/harness_tui.go) call, so an install behaves identically wherever a
// person triggers it, and the command that runs is always the exact one
// InstallHint just showed them.
func ensureHarnessInstalled(h harness.Harness, in io.Reader, out, errOut io.Writer) error {
	if _, err := exec.LookPath(h.Command); err == nil {
		return nil
	}

	command, ok := h.InstallCommand()
	if !ok {
		hint, ok := h.InstallHint()
		if !ok {
			hint = "install " + h.Command + ", then run this again"
		}
		return errmap.NewHarnessNotInstalled(h.Name, hint)
	}

	if err := checkInstallerDependencies(command); err != nil {
		return fmt.Errorf("%s is not installed, and %w", h.Name, err)
	}

	yes, err := newStdinPrompter(in, out).Confirm(fmt.Sprintf("%s is not installed. Install now?", h.Name))
	if err != nil {
		return err
	}
	if !yes {
		return errmap.NewHarnessNotInstalled(h.Name, command)
	}

	fmt.Fprintf(errOut, "\nInstalling %s...\n", h.Name)
	if err := runInstall(command, out, errOut, in); err != nil {
		return fmt.Errorf("installing %s: %w", h.Name, err)
	}

	if _, err := exec.LookPath(h.Command); err != nil {
		return fmt.Errorf("%s was installed but not found on PATH; you may need to restart your shell", h.Name)
	}
	fmt.Fprintf(errOut, "%s%s installed%s\n\n", ansiGreen, h.Name, ansiReset)
	return nil
}
