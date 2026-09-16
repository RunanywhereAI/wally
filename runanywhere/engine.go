package runanywhere

type InstalledModel struct {
	ID        string
	Framework string
	Dir       string
	Path      string
	Bytes     int64
}

// Engine runs an on-device model as a local OpenAI server. The ondevice build
// links the kit and installs one via SetEngine; a plain build leaves it nil, so
// Resolve reports ErrOnDeviceNotEnabled for a local model.
type Engine interface {
	Start(m InstalledModel) (Endpoint, error)
	Stop() error
}

var engine Engine

func SetEngine(e Engine) { engine = e }

// StopEngine releases the on-device model and kills any helper process the
// engine spawned. serve calls it on shutdown so an MLX helper never outlives
// the daemon that started it. A build with no engine linked is a no-op.
func StopEngine() error {
	if engine == nil {
		return nil
	}
	return engine.Stop()
}

// OnDeviceEnabled reports whether a build has linked an engine, so a caller can
// avoid offering an on-device model it cannot actually run.
func OnDeviceEnabled() bool { return engine != nil }
