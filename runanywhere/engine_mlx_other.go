//go:build ondevice && !darwin

package runanywhere

import "fmt"

// MLX is an Apple-silicon runtime; on Linux and Windows the on-device engine
// serves GGUF only, so an MLX model has nowhere to run.
func (e *racServerEngine) startMLX(m InstalledModel) (Endpoint, error) {
	return Endpoint{}, fmt.Errorf("MLX inference is only available on Apple silicon; %s cannot run on this platform", m.ID)
}
