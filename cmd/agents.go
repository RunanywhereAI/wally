package cmd

import (
	"strings"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/harness"
)

func newAgentCmds() []*cobra.Command {
	cmds := make([]*cobra.Command, 0, len(harness.Registry))
	for _, h := range harness.Registry {
		h := h
		c := &cobra.Command{
			Use:     h.Name + " [model] [tool flags]",
			Short:   h.Summary,
			GroupID: groupAgents,
			// Every flag except wally's own --model belongs to the tool, so
			// parse nothing and forward the rest verbatim. Whitelisting unknown
			// flags is not enough: pflag drops them instead of passing them on.
			DisableFlagParsing: true,
			RunE: func(cmd *cobra.Command, args []string) error {
				model, passthrough := splitModelArgs(args)
				return launchHarness(cmd, h, model, passthrough)
			},
		}
		cmds = append(cmds, c)
	}
	return cmds
}

// splitModelArgs pulls wally's own --model out of an otherwise untouched arg
// list and hands everything else to the tool. A bare first positional that is
// not a flag is taken as the model too, so `wally opencode <model>` works.
func splitModelArgs(args []string) (model string, rest []string) {
	rest = make([]string, 0, len(args))
	for i := 0; i < len(args); i++ {
		a := args[i]
		switch {
		case a == "--model":
			if i+1 < len(args) {
				model = args[i+1]
				i++
			}
		case strings.HasPrefix(a, "--model="):
			model = strings.TrimPrefix(a, "--model=")
		default:
			rest = append(rest, a)
		}
	}
	if model == "" && len(rest) > 0 && !strings.HasPrefix(rest[0], "-") {
		model = rest[0]
		rest = rest[1:]
	}
	return model, rest
}
