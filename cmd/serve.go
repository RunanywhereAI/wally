package cmd

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/daemon"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func newServeCmd() *cobra.Command {
	var foreground bool
	c := &cobra.Command{
		Use:     "serve",
		Short:   "Start the local server that wally clients and coding tools connect to",
		GroupID: groupServe,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			if foreground {
				return runServer(cmd)
			}
			out := cmd.OutOrStdout()
			if daemon.Running() {
				serveBanner(out, "wally is already running")
				return nil
			}
			if err := ensureDaemon(); err != nil {
				return err
			}
			serveBanner(out, "wally is running in the background")
			return nil
		},
	}
	c.Flags().BoolVar(&foreground, "foreground", false, "run in the foreground instead of detaching")
	c.Flags().MarkHidden("foreground")
	return c
}

// runServer is the blocking server. The background `wally serve` re-execs this
// with --foreground, as does ensureDaemon.
func runServer(cmd *cobra.Command) error {
	sess, err := newSession()
	if err != nil {
		return err
	}
	srv := daemon.New(sess)

	ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	errCh := make(chan error, 1)
	go func() { errCh <- srv.ListenAndServe() }()
	go refreshCatalogLoop(ctx, sess)

	select {
	case err := <-errCh:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	case <-ctx.Done():
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		err := srv.Shutdown(shutdownCtx)
		_ = runanywhere.StopEngine()
		return err
	}
}

func serveBanner(out io.Writer, headline string) {
	root := "http://" + daemon.Addr()
	color := false
	if f, ok := out.(*os.File); ok && isTerminal(f) {
		if _, noColor := os.LookupEnv("NO_COLOR"); !noColor {
			color = true
		}
	}
	green := func(s string) string {
		if color {
			return "\x1b[32m" + s + "\x1b[0m"
		}
		return s
	}
	dim := func(s string) string {
		if color {
			return "\x1b[2m" + s + "\x1b[0m"
		}
		return s
	}
	fmt.Fprintf(out, "\n%s\n\n", green(headline))
	fmt.Fprintf(out, "  API    %s/v1\n", root)
	fmt.Fprintf(out, "  Docs   %s\n\n", root)
	fmt.Fprintf(out, "%s\n\n", dim("Stop it with: wally stop"))
}
