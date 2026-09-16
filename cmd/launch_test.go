package cmd

import "testing"

func TestResolveExplicitModel_ExplicitWins(t *testing.T) {
	newTestStore(t)
	if err := saveDefaultModel("deepseek", "stored-default"); err != nil {
		t.Fatal(err)
	}
	got, err := resolveExplicitModel("from-flag", "deepseek")
	if err != nil {
		t.Fatal(err)
	}
	if got != "from-flag" {
		t.Errorf("got %q, want the explicit model to win over the stored default", got)
	}
}

func TestResolveExplicitModel_FallsBackToHarnessDefault(t *testing.T) {
	newTestStore(t)
	if err := saveDefaultModel("deepseek", "stored-default"); err != nil {
		t.Fatal(err)
	}
	got, err := resolveExplicitModel("", "deepseek")
	if err != nil {
		t.Fatal(err)
	}
	if got != "stored-default" {
		t.Errorf("got %q, want the harness's stored default", got)
	}
}

func TestResolveExplicitModel_EmptyWhenNothingStored(t *testing.T) {
	newTestStore(t)
	got, err := resolveExplicitModel("", "deepseek")
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Errorf("got %q, want empty so selectModel's own fallbacks apply", got)
	}
}

func TestResolveExplicitModel_DoesNotLeakAcrossHarnesses(t *testing.T) {
	newTestStore(t)
	if err := saveDefaultModel("opencode", "opencode-default"); err != nil {
		t.Fatal(err)
	}
	got, err := resolveExplicitModel("", "deepseek")
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Errorf("got %q, want deepseek's own (unset) default, not opencode's", got)
	}
}
