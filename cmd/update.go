package cmd

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/version"
)

const releasesLatestURL = "https://api.github.com/repos/RunanywhereAI/wally/releases/latest"

func newUpdateCmd() *cobra.Command {
	var target string
	c := &cobra.Command{
		Use:     "update",
		Short:   "Check for a newer wally and how to install it",
		GroupID: groupAbout,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			out := cmd.OutOrStdout()
			current := version.Version

			latest := strings.TrimSpace(target)
			if latest == "" || latest == "*" {
				var err error
				latest, err = latestReleaseTag(cmd.Context())
				if err != nil {
					return err
				}
			}

			if !isNewer(latest, current) {
				fmt.Fprintf(out, "You already have the latest version (%s).\n", current)
				return nil
			}
			fmt.Fprintf(out, "A newer version is available: %s (you have %s).\n", latest, current)
			fmt.Fprintln(out, "Install it with: curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | bash")
			return nil
		},
	}
	c.Flags().StringVar(&target, "version", "", "check against a specific version instead of the latest release")
	return c
}

func latestReleaseTag(ctx context.Context) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, releasesLatestURL, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("Accept", "application/vnd.github+json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", fmt.Errorf("could not reach the release server. Check your connection and try again")
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("could not read the latest release (status %d)", resp.StatusCode)
	}
	var parsed struct {
		TagName string `json:"tag_name"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&parsed); err != nil {
		return "", err
	}
	if parsed.TagName == "" {
		return "", fmt.Errorf("the release server did not report a version")
	}
	return parsed.TagName, nil
}

// isNewer reports whether latest is a higher release than current. A prerelease
// suffix on either side (for example a local -dev build) is ignored: only the
// numeric major.minor.patch decides, so a dev build never treats a lower stable
// release as an update.
func isNewer(latest, current string) bool {
	lm, ln, lp := parseSemver(latest)
	cm, cn, cp := parseSemver(current)
	switch {
	case lm != cm:
		return lm > cm
	case ln != cn:
		return ln > cn
	default:
		return lp > cp
	}
}

func parseSemver(v string) (major, minor, patch int) {
	v = strings.TrimPrefix(strings.TrimSpace(v), "v")
	if i := strings.IndexAny(v, "-+"); i >= 0 {
		v = v[:i]
	}
	parts := strings.Split(v, ".")
	get := func(i int) int {
		if i >= len(parts) {
			return 0
		}
		n, _ := strconv.Atoi(parts[i])
		return n
	}
	return get(0), get(1), get(2)
}
