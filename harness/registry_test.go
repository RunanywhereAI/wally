package harness

import "testing"

func TestRegistryHasAllFourOpenAIShapedHarnesses(t *testing.T) {
	want := map[string]string{
		"opencode": "opencode",
		"hermes":   "hermes",
		"openclaw": "openclaw",
		"deepseek": "dsh",
	}
	got := make(map[string]string, len(Registry))
	for _, h := range Registry {
		got[h.Name] = h.Command
		if h.Summary == "" {
			t.Errorf("harness %q has no Summary", h.Name)
		}
		if h.Wire == nil {
			t.Errorf("harness %q has no Wire func", h.Name)
		}
	}
	for name, command := range want {
		if got[name] != command {
			t.Errorf("Registry[%q].Command = %q, want %q", name, got[name], command)
		}
	}
}
