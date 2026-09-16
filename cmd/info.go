package cmd

import (
	"fmt"
	"runtime"
	"text/tabwriter"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/config"
	"github.com/RunanywhereAI/wally/version"
)

func newInfoCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "info",
		Short:   "Show the wally version, build channel, and platform",
		GroupID: groupAbout,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			w := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 0, 2, ' ', 0)
			fmt.Fprintf(w, "wally\t%s\n", version.Version)
			fmt.Fprintf(w, "channel\t%s\n", config.Channel())
			fmt.Fprintf(w, "platform\t%s/%s\n", runtime.GOOS, runtime.GOARCH)
			return w.Flush()
		},
	}
}
