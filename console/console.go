// Package console is the wally cloud client and browser device-flow login.
package console

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/RunanywhereAI/wally/config"
)

const (
	envBaseURLOverride   = "WALLY_CONSOLE_URL"
	envWebOriginOverride = "WALLY_CONSOLE_WEB_URL"

	defaultTimeout   = 30 * time.Second
	maxResponseBytes = 1 << 20 // matches the console's own request-body cap
)

// ErrUnauthorized means the console rejected the session outright: the access
// token is expired or revoked, and the fix is `wally login`, not a retry.
var ErrUnauthorized = errors.New("your cloud session is no longer valid, run wally login")

// ErrUnavailable means the console could not be asked right now (429 or 5xx).
// It is not a verdict on the session: a caller already holding one may keep
// using it, and a poll loop should treat this as still pending rather than
// denied.
var ErrUnavailable = errors.New("wally cloud is temporarily unavailable")

// ResolveBaseURL is the console API origin: WALLY_CONSOLE_URL when set,
// otherwise config.ConsoleAPIURL().
func ResolveBaseURL() string {
	if v := strings.TrimSpace(os.Getenv(envBaseURLOverride)); v != "" {
		return v
	}
	return config.ConsoleAPIURL()
}

// ResolveWebOrigin is the browser origin allowed to host the device-flow
// approval page: WALLY_CONSOLE_WEB_URL when set, otherwise
// config.ConsoleWebOrigin().
func ResolveWebOrigin() string {
	if v := strings.TrimSpace(os.Getenv(envWebOriginOverride)); v != "" {
		return v
	}
	return config.ConsoleWebOrigin()
}

// Client talks to the wally control plane. The zero value is not usable; build
// one with New.
type Client struct {
	baseURL    string
	httpClient *http.Client
	sleep      func(time.Duration)
}

// Option configures a Client built by New.
type Option func(*Client)

// WithBaseURL points the client at a specific console origin instead of
// ResolveBaseURL(), for tests and for a future --console-url flag.
func WithBaseURL(url string) Option {
	return func(c *Client) { c.baseURL = strings.TrimRight(url, "/") }
}

// WithHTTPClient swaps the transport, for tests that need a custom
// http.RoundTripper.
func WithHTTPClient(hc *http.Client) Option {
	return func(c *Client) { c.httpClient = hc }
}

// New builds a Client against ResolveBaseURL() unless overridden.
func New(opts ...Option) *Client {
	c := &Client{
		baseURL:    strings.TrimRight(ResolveBaseURL(), "/"),
		httpClient: &http.Client{Timeout: defaultTimeout},
		sleep:      time.Sleep,
	}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

// request sends a JSON body (or none, when body is nil) and returns the raw
// response and its bytes for the caller to decode. accessToken is optional;
// an empty string sends no Authorization header.
func (c *Client) request(ctx context.Context, method, path, accessToken string, body any) (*http.Response, []byte, error) {
	var reader io.Reader
	if body != nil {
		encoded, err := json.Marshal(body)
		if err != nil {
			return nil, nil, fmt.Errorf("console: encoding request: %w", err)
		}
		reader = bytes.NewReader(encoded)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return nil, nil, fmt.Errorf("could not reach wally cloud: %w", err)
	}
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if accessToken != "" {
		if !sessionTokenSafe(accessToken) {
			return nil, nil, errors.New("refusing to send an invalid session token")
		}
		req.Header.Set("Authorization", "Bearer "+accessToken)
	}

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, nil, fmt.Errorf("could not reach wally cloud, check your internet connection: %w", err)
	}
	defer resp.Body.Close()

	data, err := io.ReadAll(io.LimitReader(resp.Body, maxResponseBytes+1))
	if err != nil {
		return resp, nil, fmt.Errorf("could not read the wally cloud response: %w", err)
	}
	if len(data) > maxResponseBytes {
		return resp, nil, errors.New("wally cloud sent an unexpectedly large response")
	}
	return resp, data, nil
}

// APIError is a console response outside the 2xx range, carrying enough to
// render a one-line, actionable message and to decide whether retrying makes
// sense. Unwrap yields ErrUnauthorized or ErrUnavailable where one applies, so
// callers can branch with errors.Is instead of reading StatusCode by hand.
type APIError struct {
	Op         string
	StatusCode int
	RetryAfter time.Duration
	Origin     string
}

func (e *APIError) Error() string {
	switch {
	case e.StatusCode == http.StatusTooManyRequests:
		if e.RetryAfter > 0 {
			return fmt.Sprintf("wally cloud is busy, try again in %s", e.RetryAfter)
		}
		return "wally cloud is busy, try again in a moment"
	case e.StatusCode == http.StatusUnauthorized || e.StatusCode == http.StatusForbidden:
		return "your cloud session is no longer valid, run wally login"
	case e.StatusCode == http.StatusNotFound:
		return fmt.Sprintf("wally cloud has no such endpoint (%s)", e.Origin)
	case e.StatusCode >= 500:
		return fmt.Sprintf("wally cloud is temporarily unavailable, try again shortly (HTTP %d)", e.StatusCode)
	default:
		return fmt.Sprintf("wally cloud could not complete the %s (HTTP %d)", e.Op, e.StatusCode)
	}
}

func (e *APIError) Unwrap() error {
	switch {
	case e.StatusCode == http.StatusUnauthorized || e.StatusCode == http.StatusForbidden:
		return ErrUnauthorized
	case e.StatusCode == http.StatusTooManyRequests || e.StatusCode >= 500:
		return ErrUnavailable
	default:
		return nil
	}
}

func (c *Client) apiError(op string, resp *http.Response) *APIError {
	return &APIError{
		Op:         op,
		StatusCode: resp.StatusCode,
		RetryAfter: retryAfterDuration(resp),
		Origin:     c.baseURL,
	}
}

// retryAfterDuration reads Retry-After as whole seconds, the only form the
// console sends. A day is the ceiling: anything larger is a misconfiguration,
// and honoring it would hang a terminal for hours.
func retryAfterDuration(resp *http.Response) time.Duration {
	raw := strings.TrimSpace(resp.Header.Get("Retry-After"))
	if raw == "" {
		return 0
	}
	seconds, err := strconv.Atoi(raw)
	if err != nil || seconds < 0 {
		return 0
	}
	if seconds > 86400 {
		seconds = 86400
	}
	return time.Duration(seconds) * time.Second
}
