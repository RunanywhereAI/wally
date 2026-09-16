package cmd

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"strings"

	"github.com/spf13/cobra"
)

func newChatCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "chat [model]",
		Short:   "Open an interactive chat with a model",
		GroupID: groupRun,
		Args:    cobra.MaximumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			out := cmd.OutOrStdout()
			sess, err := newSession()
			if err != nil {
				return err
			}
			explicit := ""
			if len(args) > 0 {
				explicit = args[0]
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
			return chatREPL(cmd, model)
		},
	}
}

func chatREPL(cmd *cobra.Command, model string) error {
	out := cmd.OutOrStdout()
	in := bufio.NewReader(cmd.InOrStdin())
	var messages []chatMessage

	fmt.Fprintf(out, "Chatting with %s. Type /exit to quit.\n", model)
	for {
		fmt.Fprint(out, "\n> ")
		line, err := in.ReadString('\n')
		text := strings.TrimSpace(line)
		if text == "/exit" || text == "/quit" {
			return nil
		}
		if text != "" {
			messages = append(messages, chatMessage{Role: "user", Content: text})
			reply, serr := streamTurn(model, messages, out)
			switch {
			case errors.Is(serr, context.Canceled):
				fmt.Fprintln(out, "(cancelled)")
			case serr != nil:
				fmt.Fprintf(out, "%s\n", serr)
			case reply != "":
				messages = append(messages, chatMessage{Role: "assistant", Content: reply})
			}
		}
		if err != nil {
			return nil
		}
	}
}
