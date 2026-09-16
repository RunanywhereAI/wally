package cmd

import (
	"errors"
	"fmt"
	"os"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/version"
)

var verbose bool

const (
	groupRun     = "run"
	groupServe   = "serve"
	groupModels  = "models"
	groupAgents  = "agents"
	groupManage  = "manage"
	groupAccount = "account"
	groupAbout   = "about"
)

func newRootCmd() *cobra.Command {
	root := &cobra.Command{
		Use:           "wally",
		Short:         "Run language models on your own machine, and point your coding tools at them.",
		Version:       version.Version,
		SilenceUsage:  true,
		SilenceErrors: true,
	}

	root.PersistentFlags().BoolVar(&verbose, "verbose", false, "show technical detail when a command fails")

	root.AddGroup(
		&cobra.Group{ID: groupRun, Title: "Run"},
		&cobra.Group{ID: groupServe, Title: "Serve"},
		&cobra.Group{ID: groupModels, Title: "Models"},
		&cobra.Group{ID: groupAgents, Title: "Coding agents"},
		&cobra.Group{ID: groupManage, Title: "Manage"},
		&cobra.Group{ID: groupAccount, Title: "Account"},
		&cobra.Group{ID: groupAbout, Title: "About"},
	)

	root.AddCommand(
		newRunCmd(),
		newChatCmd(),
		newServeCmd(),
		newStopCmd(),
		newWebCmd(),
		newModelsCmd(),
		newHarnessCmd(),
		newLoginCmd(),
		newLogoutCmd(),
		newWhoamiCmd(),
		newUsageCmd(),
		newVersionCmd(),
		newInfoCmd(),
		newUpdateCmd(),
		newUninstallCmd(),
	)
	root.AddCommand(newAgentCmds()...)

	return root
}

func Execute() {
	err := newRootCmd().Execute()
	if err == nil {
		return
	}
	_, noColor := os.LookupEnv("NO_COLOR")
	fmt.Fprintln(os.Stderr, errmap.Format(err, errmap.ColorEnabled(noColor, isTerminal(os.Stderr))))
	if verbose {
		if detail := errDetail(err); detail != "" {
			fmt.Fprintln(os.Stderr, detail)
		}
	}
	os.Exit(1)
}

func errDetail(err error) string {
	var e *errmap.Error
	if errors.As(err, &e) {
		if d := e.Detail(); d != e.Error() {
			return d
		}
	}
	return ""
}
