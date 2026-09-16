package console

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"time"
)

// Authorization is what /auth/cli/start hands back: the code a person types
// or approves in the browser, and the secret that proves this process is the
// one that started the attempt. The console stores only a hash of PollSecret.
type Authorization struct {
	RequestCode     string
	PollSecret      string
	VerificationURL string
	ExpiresIn       time.Duration
	Interval        time.Duration
}

// Grant is a signed-in session as the console hands it back, from either a
// poll approval or a refresh.
type Grant struct {
	AccessToken  string
	RefreshToken string
	Email        string
	ExpiresIn    time.Duration
}

// PollStatus is the closed set /auth/cli/poll answers with. An unrecognized
// value is a parse failure, not a fifth status: treating it as Pending would
// keep a denied or expired login looping silently.
type PollStatus int

const (
	PollPending PollStatus = iota
	PollApproved
	PollDenied
	PollExpired
)

func (s PollStatus) String() string {
	switch s {
	case PollPending:
		return "pending"
	case PollApproved:
		return "approved"
	case PollDenied:
		return "denied"
	case PollExpired:
		return "expired"
	default:
		return "unknown"
	}
}

func parsePollStatus(raw string) (PollStatus, error) {
	switch raw {
	case "pending":
		return PollPending, nil
	case "approved":
		return PollApproved, nil
	case "denied":
		return PollDenied, nil
	case "expired":
		return PollExpired, nil
	default:
		return 0, fmt.Errorf("console returned an unknown poll status %q", raw)
	}
}

// PollOutcome is one /auth/cli/poll answer. Grant is set only when
// Status == PollApproved. RetryAfter carries the console's own requested
// delay when it answered "still waiting" only because it is busy (429/5xx),
// so a caller does not poll straight through a rate limit.
type PollOutcome struct {
	Status     PollStatus
	Grant      *Grant
	RetryAfter time.Duration
}

type cliStartRequest struct {
	Client   string `json:"client"`
	Hostname string `json:"hostname"`
}

type cliStartResponse struct {
	RequestCode     string `json:"request_code"`
	PollSecret      string `json:"poll_secret"`
	VerificationURL string `json:"verification_url"`
	ExpiresIn       int64  `json:"expires_in"`
	Interval        int64  `json:"interval"`
}

// maxStartRetries and maxStartWait bound how long StartAuthorization waits out
// a busy console on its own. A console asking for longer than maxStartWait is
// reported rather than slept through: a terminal sitting silent for minutes
// reads as a hang.
const (
	maxStartRetries = 3
	maxStartWait    = 10 * time.Second
)

// StartAuthorization begins the device flow. onRetry, when non-nil, is called
// before each wait while the console is rate limiting, so a caller can tell
// the person it is retrying rather than look hung.
func (c *Client) StartAuthorization(ctx context.Context, hostname string, onRetry func()) (*Authorization, error) {
	body := cliStartRequest{Client: "rcli", Hostname: hostname}

	var resp *http.Response
	var data []byte
	var err error
	for attempt := 0; ; attempt++ {
		resp, data, err = c.request(ctx, http.MethodPost, "/auth/cli/start", "", body)
		if err != nil {
			return nil, err
		}
		if resp.StatusCode != http.StatusTooManyRequests || attempt >= maxStartRetries {
			break
		}
		wait := retryAfterDuration(resp)
		if wait <= 0 {
			wait = time.Second
		}
		if wait > maxStartWait {
			return nil, c.apiError("authorization", resp)
		}
		if onRetry != nil {
			onRetry()
		}
		c.sleep(wait)
	}
	if resp.StatusCode != http.StatusOK {
		return nil, c.apiError("authorization", resp)
	}

	var parsed cliStartResponse
	if err := json.Unmarshal(data, &parsed); err != nil {
		return nil, fmt.Errorf("console returned a response that did not match the contract: %w", err)
	}
	if !requestCodeSafe(parsed.RequestCode) || !sessionTokenSafe(parsed.PollSecret) {
		return nil, errors.New("console returned an invalid authorization request")
	}
	return &Authorization{
		RequestCode:     parsed.RequestCode,
		PollSecret:      parsed.PollSecret,
		VerificationURL: parsed.VerificationURL,
		ExpiresIn:       clampSeconds(parsed.ExpiresIn, 30, 1800),
		Interval:        clampSeconds(parsed.Interval, 1, 30),
	}, nil
}

type cliPollRequest struct {
	RequestCode string `json:"request_code"`
	PollSecret  string `json:"poll_secret"`
}

type cliPollResponse struct {
	Status       string  `json:"status"`
	AccessToken  *string `json:"access_token"`
	RefreshToken *string `json:"refresh_token"`
	Email        *string `json:"email"`
	ExpiresIn    *int64  `json:"expires_in"`
}

// Poll makes one /auth/cli/poll attempt. It does not loop; PollUntilGranted
// does that, at the cadence NextPollDelay computes.
func (c *Client) Poll(ctx context.Context, auth *Authorization) (PollOutcome, error) {
	body := cliPollRequest{RequestCode: auth.RequestCode, PollSecret: auth.PollSecret}
	resp, data, err := c.request(ctx, http.MethodPost, "/auth/cli/poll", "", body)
	if err != nil {
		return PollOutcome{}, err
	}
	if resp.StatusCode != http.StatusOK {
		// A busy or briefly unavailable console has not denied anything, and the
		// person may still be approving in the browser: treat it as "still
		// waiting" so the poll loop keeps its normal cadence instead of failing
		// the whole login on one refusal.
		if resp.StatusCode == http.StatusTooManyRequests || resp.StatusCode >= 500 {
			return PollOutcome{Status: PollPending, RetryAfter: retryAfterDuration(resp)}, nil
		}
		return PollOutcome{}, c.apiError("poll", resp)
	}

	var parsed cliPollResponse
	if err := json.Unmarshal(data, &parsed); err != nil {
		return PollOutcome{}, fmt.Errorf("console returned a response that did not match the contract: %w", err)
	}
	status, err := parsePollStatus(parsed.Status)
	if err != nil {
		return PollOutcome{}, err
	}
	if status != PollApproved {
		return PollOutcome{Status: status}, nil
	}

	grant, err := mapGrant(stringOr(parsed.AccessToken), stringOr(parsed.RefreshToken), stringOr(parsed.Email), int64Or(parsed.ExpiresIn))
	if err != nil {
		return PollOutcome{}, err
	}
	if grant.AccessToken == "" || grant.RefreshToken == "" {
		return PollOutcome{}, errors.New("console approved the request without a complete session")
	}
	return PollOutcome{Status: PollApproved, Grant: grant}, nil
}

// NextPollDelay is how long to wait before polling again, given the
// authorization's own cadence and whatever extra delay the console just
// asked for. The floor is the authorization's interval; the console's own
// request only ever raises it, never lowers it. There is deliberately no
// ceiling: capping the wait and polling anyway is the bug this fixes, not a
// safety measure. A console asking for 60s and polled every 10s is refused
// six times as often as it asked for, and a rate limiter that penalizes
// repeat offenders may then never let the login through. The caller bounds
// the wait by the authorization's own expiry instead.
func NextPollDelay(interval, retryAfter time.Duration) time.Duration {
	floor := interval
	if floor < time.Second {
		floor = time.Second
	}
	asked := retryAfter
	if asked < 0 {
		asked = 0
	}
	if asked > floor {
		return asked
	}
	return floor
}

// PollUntilGranted polls at NextPollDelay's cadence until the login is
// approved, denied, expired, or outlasts the authorization's own expiry.
// onWait, when non-nil, is called with each computed delay before sleeping,
// so a caller can render a waiting indicator.
func (c *Client) PollUntilGranted(ctx context.Context, auth *Authorization, onWait func(time.Duration)) (*Grant, error) {
	deadline := time.Now().Add(auth.ExpiresIn)
	for {
		outcome, err := c.Poll(ctx, auth)
		if err != nil {
			return nil, err
		}
		switch outcome.Status {
		case PollApproved:
			return outcome.Grant, nil
		case PollDenied:
			return nil, errors.New("sign-in was denied")
		case PollExpired:
			return nil, errors.New("sign-in request expired, run wally login again")
		}

		delay := NextPollDelay(auth.Interval, outcome.RetryAfter)
		if time.Now().Add(delay).After(deadline) {
			return nil, errors.New("sign-in timed out waiting for approval")
		}
		if onWait != nil {
			onWait(delay)
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		default:
		}
		c.sleep(delay)
	}
}

type cliRefreshRequest struct {
	RefreshToken string `json:"refresh_token"`
}

type grantResponse struct {
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	Email        string `json:"email"`
	ExpiresIn    int64  `json:"expires_in"`
}

// Refresh exchanges a refresh token for a new Grant.
func (c *Client) Refresh(ctx context.Context, refreshToken string) (*Grant, error) {
	if !sessionTokenSafe(refreshToken) {
		return nil, errors.New("no refresh token is available")
	}
	resp, data, err := c.request(ctx, http.MethodPost, "/auth/cli/refresh", "", cliRefreshRequest{RefreshToken: refreshToken})
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, c.apiError("refresh", resp)
	}

	var parsed grantResponse
	if err := json.Unmarshal(data, &parsed); err != nil {
		return nil, fmt.Errorf("console returned a response that did not match the contract: %w", err)
	}
	grant, err := mapGrant(parsed.AccessToken, parsed.RefreshToken, parsed.Email, parsed.ExpiresIn)
	if err != nil {
		return nil, err
	}
	if grant.AccessToken == "" {
		return nil, errors.New("console refreshed the session without an access token")
	}
	return grant, nil
}

// Revoke ends the session on the console. Its failure says nothing about
// whether the local credential should stay: a caller that also holds a
// credstore.Store clears it unconditionally and only uses this error to
// decide what to tell the person.
func (c *Client) Revoke(ctx context.Context, accessToken, refreshToken string) error {
	if accessToken == "" && refreshToken == "" {
		return nil
	}
	if (accessToken != "" && !sessionTokenSafe(accessToken)) || (refreshToken != "" && !sessionTokenSafe(refreshToken)) {
		return errors.New("cloud session contains an invalid token encoding")
	}
	resp, _, err := c.request(ctx, http.MethodPost, "/auth/cli/revoke", accessToken, cliRefreshRequest{RefreshToken: refreshToken})
	if err != nil {
		return err
	}
	if resp.StatusCode != http.StatusOK && resp.StatusCode != http.StatusNoContent {
		return c.apiError("revoke", resp)
	}
	return nil
}

// mapGrant maps wire fields onto the domain Grant, sanitizing every string
// that will reach the terminal or the credential store.
func mapGrant(accessToken, refreshToken, email string, expiresInSeconds int64) (*Grant, error) {
	if (accessToken != "" && !sessionTokenSafe(accessToken)) ||
		(refreshToken != "" && !sessionTokenSafe(refreshToken)) ||
		(email != "" && !displaySafe(email, 320)) {
		return nil, errors.New("console returned an invalid cloud session")
	}
	if expiresInSeconds < 0 {
		expiresInSeconds = 0
	}
	return &Grant{
		AccessToken:  accessToken,
		RefreshToken: refreshToken,
		Email:        email,
		ExpiresIn:    time.Duration(expiresInSeconds) * time.Second,
	}, nil
}

func clampSeconds(seconds, min, max int64) time.Duration {
	if seconds < min {
		seconds = min
	}
	if seconds > max {
		seconds = max
	}
	return time.Duration(seconds) * time.Second
}

func stringOr(v *string) string {
	if v == nil {
		return ""
	}
	return *v
}

func int64Or(v *int64) int64 {
	if v == nil {
		return 0
	}
	return *v
}
