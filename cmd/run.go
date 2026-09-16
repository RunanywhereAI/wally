package cmd

import (
	"context"
	"errors"
	"strings"

	"github.com/spf13/cobra"
)

func newRunCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "run [model] [prompt]",
		Short:   "Run a model once with a prompt, or open a chat when no prompt is given",
		GroupID: groupRun,
		RunE: func(cmd *cobra.Command, args []string) error {
			out := cmd.OutOrStdout()
			sess, err := newSession()
			if err != nil {
				return err
			}

			explicit := ""
			var prompt string
			if len(args) > 0 {
				explicit = args[0]
				prompt = strings.Join(args[1:], " ")
			}

			model, err := selectModel(explicit, sess.signedIn(), newStdinPrompter(cmd.InOrStdin(), out), out, colorFor(out))
			if err != nil {
				return err
			}
			if err := validateModel(model); err != nil {
				return err
			}
			if err := ensureDaemon(); err != nil {
				return err
			}

			if prompt != "" {
				_, err := streamTurn(model, []chatMessage{{Role: "user", Content: prompt}}, out)
				if errors.Is(err, context.Canceled) {
					return nil
				}
				return err
			}
			return chatREPL(cmd, model)
		},
	}
}
