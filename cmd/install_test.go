package cmd

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/harness"
)

type stubInstaller struct{ cmd string }

func (s stubInstaller) InstallCommand() string { return s.cmd }

func skipOnWindows(t *testing.T) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip("exercises the unix (bash -c) install path")
	}
}

func TestRunInstall_StreamsOutput(t *testing.T) {
	skipOnWindows(t)
	var out bytes.Buffer
	if err := runInstall("echo hello-from-install", &out, &out, strings.NewReader("")); err != nil {
		t.Fatalf("runInstall: %v", err)
	}
	if !strings.Contains(out.String(), "hello-from-install") {
		t.Errorf("output = %q, want the command's stdout streamed through", out.String())
	}
}

func TestRunInstall_PropagatesFailure(t *testing.T) {
	skipOnWindows(t)
	var out bytes.Buffer
	err := runInstall("exit 7", &out, &out, strings.NewReader(""))
	if err == nil {
		t.Fatal("expected a failing install command to return an error")
	}
}

func TestCheckInstallerDependencies_BareCommandNeedsOnlyTheShell(t *testing.T) {
	skipOnWindows(t)
	if err := checkInstallerDependencies("echo hi"); err != nil {
		t.Errorf("unexpected error for a plain command: %v", err)
	}
}

func TestCheckInstallerDependencies_MissingInterpreterIsReported(t *testing.T) {
	skipOnWindows(t)
	t.Setenv("PATH", t.TempDir()) // bash itself cannot resolve
	if err := checkInstallerDependencies("curl -fsSL https://example.com | bash"); err == nil {
		t.Error("expected a missing interpreter to be reported before the installer ever runs")
	}
}

func TestEnsureHarnessInstalled_AlreadyOnPathSkipsPrompt(t *testing.T) {
	h := harness.Harness{Name: "sh", Command: "sh"}
	var out bytes.Buffer
	if err := ensureHarnessInstalled(h, strings.NewReader(""), &out, &out); err != nil {
		t.Fatalf("ensureHarnessInstalled: %v", err)
	}
	if out.Len() != 0 {
		t.Errorf("output = %q, want nothing printed when the harness is already installed", out.String())
	}
}

func TestEnsureHarnessInstalled_DeclineReturnsHarnessNotInstalled(t *testing.T) {
	h := harness.Harness{
		Name:    "demo",
		Command: "definitely-not-a-real-wally-harness-binary",
		Impl:    stubInstaller{cmd: "true"},
	}
	var out bytes.Buffer
	err := ensureHarnessInstalled(h, strings.NewReader("n\n"), &out, &out)
	var mapped *errmap.Error
	if !errors.As(err, &mapped) || mapped.Kind() != errmap.KindHarnessNotInstalled {
		t.Fatalf("err = %v, want errmap.KindHarnessNotInstalled", err)
	}
}

func TestEnsureHarnessInstalled_NoInstallCommandReportsHint(t *testing.T) {
	h := harness.Harness{Name: "demo", Command: "definitely-not-a-real-wally-harness-binary"}
	var out bytes.Buffer
	err := ensureHarnessInstalled(h, strings.NewReader(""), &out, &out)
	var mapped *errmap.Error
	if !errors.As(err, &mapped) || mapped.Kind() != errmap.KindHarnessNotInstalled {
		t.Fatalf("err = %v, want errmap.KindHarnessNotInstalled", err)
	}
}

// TestEnsureHarnessInstalled_RunsInstallerAndRechecksPath proves the auto-
// execute path end to end: confirming yes actually runs InstallCommand
// (through the same runInstall the launcher uses), and a subsequent
// exec.LookPath finds the binary it produced.
func TestEnsureHarnessInstalled_RunsInstallerAndRechecksPath(t *testing.T) {
	skipOnWindows(t)
	dir := t.TempDir()
	t.Setenv("PATH", dir+string(os.PathListSeparator)+os.Getenv("PATH"))
	binPath := filepath.Join(dir, "wally-test-harness-stub")

	h := harness.Harness{
		Name:    "stubharness",
		Command: "wally-test-harness-stub",
		Impl:    stubInstaller{cmd: fmt.Sprintf("printf '#!/bin/sh\\necho ran\\n' > %s && chmod +x %s", binPath, binPath)},
	}
	var out bytes.Buffer
	if err := ensureHarnessInstalled(h, strings.NewReader("y\n"), &out, &out); err != nil {
		t.Fatalf("ensureHarnessInstalled: %v", err)
	}
	if _, err := exec.LookPath(h.Command); err != nil {
		t.Fatalf("expected %s on PATH after the install ran: %v", h.Command, err)
	}
	if !strings.Contains(out.String(), "installed") {
		t.Errorf("output = %q, want a success line", out.String())
	}
}
