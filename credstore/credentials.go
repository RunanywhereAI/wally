// Package credstore stores the wally cloud session encrypted at rest.
package credstore

import "time"

// DefaultExpirySkew is the margin AccessTokenExpired treats an access token
// as expired ahead of its real expiry, so a request does not race a token
// that dies mid-flight.
const DefaultExpirySkew = 60 * time.Second

// Credentials is the wally cloud session. Field names and JSON tags match the
// legacy C++ store so a profile written by either binary reads back the same
// way.
type Credentials struct {
	ConsoleURL   string `json:"console_url"`
	Email        string `json:"email"`
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	ExpiresAt    int64  `json:"expires_at"`
}

// SignedIn reports whether these credentials carry a session at all.
func (c Credentials) SignedIn() bool {
	return c.AccessToken != ""
}

// AccessTokenExpired reports whether the access token is expired, or due to
// expire within skew. ExpiresAt of zero means the console did not report an
// expiry, which is never treated as expired.
func (c Credentials) AccessTokenExpired(now time.Time, skew time.Duration) bool {
	if c.ExpiresAt <= 0 {
		return false
	}
	return c.ExpiresAt <= now.Add(skew).Unix()
}
