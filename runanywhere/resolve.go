package runanywhere

import "errors"

// Returned for an on-device model until the engine is linked in (milestone 5);
// the cloud branch is never gated.
var ErrOnDeviceNotEnabled = errors.New("on-device models are not enabled in this build yet")

var ErrNotSignedIn = errors.New("not signed in")

type Session interface {
	ConsoleBaseURL() string
	Token() (string, error)
}

func Resolve(model string, sess Session) (Endpoint, error) {
	if m, ok := findInstalledModel(model); ok {
		if engine == nil {
			return Endpoint{}, ErrOnDeviceNotEnabled
		}
		return engine.Start(m)
	}
	if sess == nil {
		return Endpoint{}, ErrNotSignedIn
	}
	token, err := sess.Token()
	if err != nil {
		return Endpoint{}, err
	}
	return Endpoint{
		BaseURL: sess.ConsoleBaseURL() + "/v1",
		APIKey:  token,
		Local:   false,
	}, nil
}
