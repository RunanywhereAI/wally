package console

import "testing"

func TestVerificationURLTrusted(t *testing.T) {
	t.Setenv(envWebOriginOverride, "https://console.runanywhere.ai")

	cases := []struct {
		name string
		url  string
		want bool
	}{
		{"matching origin", "https://console.runanywhere.ai/cloud/cli?code=abc", true},
		{"different host", "https://evil.example.com/cloud/cli?code=abc", false},
		{"http downgrade", "http://console.runanywhere.ai/cloud/cli", false},
		{"different port", "https://console.runanywhere.ai:8443/cloud/cli", false},
		{"malformed url", "not a url", false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := VerificationURLTrusted(tc.url); got != tc.want {
				t.Fatalf("VerificationURLTrusted(%q) = %v, want %v", tc.url, got, tc.want)
			}
		})
	}
}

func TestVerificationURLTrusted_LoopbackAllowsHTTP(t *testing.T) {
	t.Setenv(envWebOriginOverride, "http://localhost:8080")
	if !VerificationURLTrusted("http://localhost:8080/cloud/cli?code=abc") {
		t.Fatal("expected an exact loopback origin to be trusted over http")
	}
}

func TestVerificationURLTrusted_NoConfiguredOriginRefuses(t *testing.T) {
	t.Setenv(envWebOriginOverride, "not-a-url either")
	if VerificationURLTrusted("https://console.runanywhere.ai/cloud/cli") {
		t.Fatal("expected an unresolvable trusted origin to trust nothing")
	}
}
