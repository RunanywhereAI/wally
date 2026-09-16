package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/daemon"
)

func newWebCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "web",
		Short:   "Open the wally dashboard in your browser",
		GroupID: groupServe,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			if err := ensureDaemon(); err != nil {
				return err
			}
			url := "http://" + daemon.Addr() + "/"
			fmt.Fprintf(cmd.OutOrStdout(), "wally dashboard: %s\n", url)
			return openBrowser(url)
		},
	}
}
