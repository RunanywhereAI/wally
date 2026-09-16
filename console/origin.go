package console

import (
	"fmt"
	"net/url"
	"strings"
)

// TrustedOrigin is the browser origin allowed to host the device-flow
// approval page, normalized to scheme://host. Empty when ResolveWebOrigin()
// is not a valid origin.
func TrustedOrigin() string {
	origin, err := normalizeOrigin(ResolveWebOrigin())
	if err != nil {
		return ""
	}
	return origin
}

// VerificationURLTrusted reports whether verificationURL sits on the
// configured approval origin. The device flow's verification_url comes back
// from the server on every /auth/cli/start call; a compromised or
// misconfigured response must not silently open an unrelated sign-in page, so
// this runs before a caller opens the browser.
func VerificationURLTrusted(verificationURL string) bool {
	trusted := TrustedOrigin()
	if trusted == "" {
		return false
	}
	origin, err := normalizeOrigin(verificationURL)
	if err != nil {
		return false
	}
	return origin == trusted
}

// normalizeOrigin validates and canonicalizes a URL's origin. HTTPS is
// required except for an exact loopback host, matching the console's own
// rule for the API origin.
func normalizeOrigin(raw string) (string, error) {
	u, err := url.Parse(raw)
	if err != nil {
		return "", fmt.Errorf("console: could not parse origin: %w", err)
	}
	if u.User != nil {
		return "", fmt.Errorf("console: origin may not contain credentials")
	}
	scheme := strings.ToLower(u.Scheme)
	host := strings.ToLower(u.Hostname())
	if host == "" {
		return "", fmt.Errorf("console: origin has no host")
	}
	loopback := host == "localhost" || host == "127.0.0.1" || host == "::1"
	if scheme != "https" && !(scheme == "http" && loopback) {
		return "", fmt.Errorf("console: origin must be https, or http on exact loopback")
	}
	rendered := scheme + "://" + host
	if port := u.Port(); port != "" {
		rendered += ":" + port
	}
	return rendered, nil
}
