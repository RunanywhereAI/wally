package console

// sessionTokenSafe constrains bearer and refresh tokens to RFC 6750 b64token
// characters, so a value read off the wire or off disk cannot inject an HTTP
// header when it is later sent back.
func sessionTokenSafe(token string) bool {
	if token == "" || len(token) > 8192 {
		return false
	}
	for _, r := range token {
		if !isAlphaNumeric(r) && r != '-' && r != '.' && r != '_' && r != '~' && r != '+' && r != '/' && r != '=' {
			return false
		}
	}
	return true
}

// requestCodeSafe matches the control plane's token_urlsafe alphabet: letters,
// digits, '-' and '_'.
func requestCodeSafe(code string) bool {
	if len(code) < 4 || len(code) > 64 {
		return false
	}
	for _, r := range code {
		if !isAlphaNumeric(r) && r != '-' && r != '_' {
			return false
		}
	}
	return true
}

// displaySafe reports whether value is plain printable ASCII short enough to
// land in a terminal or a credential file without carrying an escape
// sequence. The console is not a trusted source for either.
func displaySafe(value string, max int) bool {
	if value == "" || len(value) > max {
		return false
	}
	for _, r := range value {
		if r < 0x20 || r > 0x7e {
			return false
		}
	}
	return true
}

func isAlphaNumeric(r rune) bool {
	return (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9')
}
