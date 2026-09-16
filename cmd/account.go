package cmd

import (
	"context"
	"errors"
	"fmt"
	"os"
	"text/tabwriter"
	"time"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/console"
	"github.com/RunanywhereAI/wally/credstore"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func newLoginCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "login",
		Short:   "Sign in to Wally Cloud in your browser",
		GroupID: groupAccount,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			out := cmd.OutOrStdout()
			client := console.New()
			store, err := credstore.New()
			if err != nil {
				return err
			}

			hostname, _ := os.Hostname()
			auth, err := client.StartAuthorization(cmd.Context(), hostname, func() {
				fmt.Fprintln(out, "Waiting for the console to be ready...")
			})
			if err != nil {
				return err
			}
			if !console.VerificationURLTrusted(auth.VerificationURL) {
				return fmt.Errorf("the console returned an approval URL wally does not trust: %s", auth.VerificationURL)
			}

			fmt.Fprintf(out, "Approve this sign in at:\n  %s\n\n", auth.VerificationURL)
			if err := openBrowser(auth.VerificationURL); err != nil {
				fmt.Fprintln(out, "Open that URL in your browser to continue.")
			}

			grant, err := client.PollUntilGranted(cmd.Context(), auth, func(time.Duration) {})
			if err != nil {
				return err
			}

			creds := credstore.Credentials{
				ConsoleURL:   console.ResolveBaseURL(),
				Email:        grant.Email,
				AccessToken:  grant.AccessToken,
				RefreshToken: grant.RefreshToken,
				ExpiresAt:    expiryUnix(grant.ExpiresIn),
			}
			if err := store.Save(creds); err != nil {
				return err
			}
			if sess, err := newSession(); err == nil {
				ctx, cancel := context.WithTimeout(cmd.Context(), 5*time.Second)
				_ = refreshCatalog(ctx, sess)
				cancel()
			}
			fmt.Fprintf(out, "Signed in as %s\n", grant.Email)
			return nil
		},
	}
}

func newLogoutCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "logout",
		Short:   "Sign out of Wally Cloud on this machine",
		GroupID: groupAccount,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			out := cmd.OutOrStdout()
			store, err := credstore.New()
			if err != nil {
				return err
			}
			creds, err := store.Load()
			if err != nil {
				return err
			}
			if !creds.SignedIn() {
				fmt.Fprintln(out, "You are not signed in.")
				return nil
			}

			revokeErr := console.New().Revoke(cmd.Context(), creds.AccessToken, creds.RefreshToken)
			if err := store.Clear(); err != nil {
				return err
			}
			if revokeErr != nil {
				fmt.Fprintln(out, "Signed out on this machine. The console could not be reached to end the session there.")
				return nil
			}
			fmt.Fprintln(out, "Signed out.")
			return nil
		},
	}
}

func newWhoamiCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "whoami",
		Short:   "Show the signed-in account and this month's usage",
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
			id, err := sess.client.WhoAmI(cmd.Context(), token)
			if err != nil {
				return err
			}

			w := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 0, 2, ' ', 0)
			fmt.Fprintf(w, "email\t%s\n", id.Email)
			fmt.Fprintf(w, "plan\t%s\n", id.Plan)
			fmt.Fprintf(w, "tokens this month\t%d\n", id.TokensThisMonth)
			if id.MonthlyTokenLimit > 0 {
				fmt.Fprintf(w, "monthly limit\t%d\n", id.MonthlyTokenLimit)
			}
			return w.Flush()
		},
	}
}

func expiryUnix(d time.Duration) int64 {
	if d <= 0 {
		return 0
	}
	return time.Now().Add(d).Unix()
}
