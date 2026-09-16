package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/version"
)

func newVersionCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "version",
		Short:   "Print the wally version",
		GroupID: groupAbout,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			fmt.Fprintln(cmd.OutOrStdout(), version.Version)
			return nil
		},
	}
}
