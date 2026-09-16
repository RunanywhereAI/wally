package cmd

import (
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"runtime"
	"strings"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/daemon"
)

func newStopCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "stop",
		Short:   "Stop the local wally server",
		GroupID: groupServe,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			out := cmd.OutOrStdout()
			if !daemon.Running() {
				printGreen(out, "No local server is running.")
				return nil
			}
			if err := stopDaemon(); err != nil {
				return err
			}
			fmt.Fprintln(out, "Stopped the local server.")
			return nil
		},
	}
}

func stopDaemon() error {
	_, port, err := net.SplitHostPort(daemon.Addr())
	if err != nil {
		return err
	}
	if runtime.GOOS == "windows" {
		return errors.New("stopping the server is not supported on Windows yet. Close the window running wally serve")
	}
	out, err := exec.Command("lsof", "-ti", "tcp:"+port, "-sTCP:LISTEN").Output()
	if err != nil {
		// lsof exits non-zero with no match, which means nothing is listening.
		return nil
	}
	pids := strings.Fields(string(out))
	if len(pids) == 0 {
		return nil
	}
	return exec.Command("kill", pids...).Run()
}

func printGreen(out io.Writer, msg string) {
	if f, ok := out.(*os.File); ok && isTerminal(f) {
		if _, noColor := os.LookupEnv("NO_COLOR"); !noColor {
			fmt.Fprintf(out, "\x1b[32m%s\x1b[0m\n", msg)
			return
		}
	}
	fmt.Fprintln(out, msg)
}
