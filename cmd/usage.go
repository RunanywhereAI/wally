package cmd

import (
	"errors"
	"fmt"
	"text/tabwriter"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/console"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func newUsageCmd() *cobra.Command {
	var days int
	c := &cobra.Command{
		Use:     "usage",
		Short:   "Show your Wally Cloud credit and recent usage",
		GroupID: groupAccount,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			sess, err := newSession()
			if err != nil {
				return err
			}
			token, err := sess.Token()
			if err != nil {
				if errors.Is(err, runanywhere.ErrNotSignedIn) {
					return errmap.NewNotSignedIn()
				}
				return err
			}
			u, err := sess.client.Usage(cmd.Context(), token, console.UsageQuery{Days: days})
			if err != nil {
				return err
			}

			out := cmd.OutOrStdout()
			w := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
			fmt.Fprintf(w, "credit balance\t$%.2f\n", dollars(u.Credit.BalanceMicros))
			fmt.Fprintf(w, "granted\t$%.2f\n", dollars(u.Credit.GrantedMicros))
			fmt.Fprintf(w, "spent\t$%.2f\n", dollars(u.Credit.SpentMicros))
			fmt.Fprintf(w, "\t\n")
			fmt.Fprintf(w, "last %d days\t\n", days)
			fmt.Fprintf(w, "requests\t%d\n", u.Totals.Requests)
			fmt.Fprintf(w, "prompt tokens\t%d\n", u.Totals.PromptTokens)
			fmt.Fprintf(w, "completion tokens\t%d\n", u.Totals.CompletionTokens)
			if u.Totals.CachedTokens > 0 {
				fmt.Fprintf(w, "cached tokens\t%d\n", u.Totals.CachedTokens)
			}
			fmt.Fprintf(w, "cost\t$%.2f\n", dollars(u.Totals.CostMicros))
			w.Flush()

			if len(u.Models) > 0 {
				fmt.Fprintln(out, "\nby model")
				mw := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
				fmt.Fprintln(mw, "MODEL\tREQUESTS\tTOKENS\tCOST")
				for _, m := range u.Models {
					fmt.Fprintf(mw, "%s\t%d\t%d\t$%.2f\n", m.Model, m.Requests, m.PromptTokens+m.CompletionTokens, dollars(m.CostMicros))
				}
				mw.Flush()
			}
			return nil
		},
	}
	c.Flags().IntVar(&days, "days", 30, "number of days to summarize")
	return c
}

func dollars(micros int64) float64 { return float64(micros) / 1_000_000 }
