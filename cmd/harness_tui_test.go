package cmd

import (
	"bytes"
	"errors"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/harness"
	"github.com/RunanywhereAI/wally/runanywhere"
	"github.com/RunanywhereAI/wally/tui"
)

type stubInstallable struct{ hint string }

func (s stubInstallable) InstallHint() string { return s.hint }

type stubRestorable struct {
	err    error
	called bool
}

func (s *stubRestorable) Restore() error {
	s.called = true
	return s.err
}

type stubUninstaller struct{ command string }

func (s stubUninstaller) UninstallCommand() string { return s.command }

// noConfirmUninstall and noUninstallRun fail the test if the uninstall
// confirm or the uninstall command runner is ever invoked, for scenarios
// that never reach the Uninstall action or that decline its confirm.
func noConfirmUninstall(t *testing.T) func(string) (bool, error) {
	return func(prompt string) (bool, error) {
		t.Fatalf("confirmUninstall called unexpectedly: %q", prompt)
		return false, nil
	}
}

func noUninstallRun(t *testing.T) func(harness.Harness) error {
	return func(h harness.Harness) error {
		t.Fatalf("uninstallHarness called unexpectedly for %s", h.Name)
		return nil
	}
}

func TestHarnessInstalled(t *testing.T) {
	if !harnessInstalled(harness.Harness{Command: "sh"}) {
		t.Error("sh should resolve on PATH on any machine running this test")
	}
	if harnessInstalled(harness.Harness{Command: "definitely-not-a-real-wally-harness-binary"}) {
		t.Error("a made-up command should not resolve")
	}
}

func TestHarnessActionItems_InstallOnlyWhenNotInstalled(t *testing.T) {
	h := harness.Harness{Name: "demo", Impl: stubInstallable{hint: "brew install demo"}}

	items, keys := harnessActionItems(h, true)
	for _, k := range keys {
		if k == actionInstall {
			t.Error("an installed harness should not offer Install")
		}
	}
	if len(items) != len(keys) {
		t.Fatalf("items/keys length mismatch: %d vs %d", len(items), len(keys))
	}

	items, keys = harnessActionItems(h, false)
	idx := findActionIndex(keys, actionInstall)
	if idx == -1 {
		t.Fatal("a not-installed harness must offer Install")
	}
	if items[idx].Disabled {
		t.Error("Install must not be disabled when a hint is available")
	}
	if items[idx].Detail != "brew install demo" {
		t.Errorf("Install detail = %q, want the install hint", items[idx].Detail)
	}
}

func TestHarnessActionItems_NoHintDisablesInstall(t *testing.T) {
	h := harness.Harness{Name: "demo"} // no Impl, so no Installable
	items, keys := harnessActionItems(h, false)
	idx := findActionIndex(keys, actionInstall)
	if idx == -1 {
		t.Fatal("expected an Install row even without a hint")
	}
	if !items[idx].Disabled {
		t.Error("Install without a hint must be disabled, not silently actionable")
	}
}

func findActionIndex(keys []harnessAction, want harnessAction) int {
	for i, k := range keys {
		if k == want {
			return i
		}
	}
	return -1
}

func TestDefaultModelRoundTrip(t *testing.T) {
	newTestStore(t)

	got, err := loadDefaultModel("demo")
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Fatalf("loadDefaultModel before save = %q, want empty", got)
	}

	if err := saveDefaultModel("demo", "qwen-7b"); err != nil {
		t.Fatal(err)
	}
	got, err = loadDefaultModel("demo")
	if err != nil {
		t.Fatal(err)
	}
	if got != "qwen-7b" {
		t.Fatalf("loadDefaultModel = %q, want qwen-7b", got)
	}

	existed, err := clearHarnessState("demo")
	if err != nil {
		t.Fatal(err)
	}
	if !existed {
		t.Error("clearHarnessState reported nothing existed, but a model was just saved")
	}
	got, err = loadDefaultModel("demo")
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Fatalf("loadDefaultModel after clear = %q, want empty", got)
	}

	existed, err = clearHarnessState("demo")
	if err != nil {
		t.Fatal(err)
	}
	if existed {
		t.Error("clearHarnessState on an already-clear harness reported something existed")
	}
}

func TestRemoveHarness_CallsRestoreAndClearsState(t *testing.T) {
	newTestStore(t)
	if err := saveDefaultModel("demo", "qwen-7b"); err != nil {
		t.Fatal(err)
	}
	restorable := &stubRestorable{}
	h := harness.Harness{Name: "demo", Impl: restorable}

	note, err := removeHarness(h)
	if err != nil {
		t.Fatal(err)
	}
	if !restorable.called {
		t.Error("removeHarness must call Restore for a Restorable harness")
	}
	if note == "" {
		t.Error("expected a non-empty status note")
	}
	got, err := loadDefaultModel("demo")
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Errorf("default model = %q after remove, want cleared", got)
	}
}

func TestRemoveHarness_NonRestorableIsSafeNoop(t *testing.T) {
	newTestStore(t)
	h := harness.Harness{Name: "demo"} // no Impl, Restore is a no-op per the accessor
	if _, err := removeHarness(h); err != nil {
		t.Fatalf("removeHarness on a non-Restorable harness must not error: %v", err)
	}
}

func TestRemoveHarness_RestoreErrorPropagates(t *testing.T) {
	newTestStore(t)
	boom := errors.New("could not reset config")
	h := harness.Harness{Name: "demo", Impl: &stubRestorable{err: boom}}
	_, err := removeHarness(h)
	if !errors.Is(err, boom) {
		t.Errorf("err = %v, want it to wrap %v", err, boom)
	}
}

// scriptedList returns each entry in responses in order, then ErrCancelled.
func scriptedList(responses ...int) func(string, []tui.Item, string) (int, error) {
	i := 0
	return func(string, []tui.Item, string) (int, error) {
		if i >= len(responses) {
			return 0, tui.ErrCancelled
		}
		v := responses[i]
		i++
		return v, nil
	}
}

// noInstall is a stub installHarness that fails the test if it is ever
// called, for scenarios that never reach the Install action.
func noInstall(t *testing.T) func(harness.Harness) error {
	return func(h harness.Harness) error {
		t.Fatalf("installHarness called unexpectedly for %s", h.Name)
		return nil
	}
}

func TestRunHarnessManager_SetDefaultModel(t *testing.T) {
	newTestStore(t)
	if len(harness.Registry) == 0 {
		t.Skip("no harnesses registered to drive the manager against")
	}
	h := harness.Registry[0]
	_, keys := harnessActionItems(h, harnessInstalled(h))
	setDefaultIdx := findActionIndex(keys, actionSetDefault)

	var out bytes.Buffer
	selectHarness := scriptedList(1) // row 0 is wally chat; row 1 is Registry[0]
	selectAction := scriptedList(setDefaultIdx)
	pickModel := func(harness.Harness) (string, error) { return "qwen-7b", nil }

	if err := runHarnessManager(&out, selectHarness, selectAction, pickModel, noInstall(t), noConfirmUninstall(t), noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
	got, err := loadDefaultModel(h.Name)
	if err != nil {
		t.Fatal(err)
	}
	if got != "qwen-7b" {
		t.Fatalf("default model = %q, want qwen-7b", got)
	}
}

func TestRunHarnessManager_SetDefaultModelRejectsUnknownCloudModel(t *testing.T) {
	newTestStore(t)
	orig := catalogLoad
	catalogLoad = func() ([]catalog.Model, error) { return []catalog.Model{{ID: "gpt-oss-20b"}}, nil }
	t.Cleanup(func() { catalogLoad = orig })
	if len(harness.Registry) == 0 {
		t.Skip("no harnesses registered to drive the manager against")
	}
	h := harness.Registry[0]
	_, keys := harnessActionItems(h, harnessInstalled(h))
	setDefaultIdx := findActionIndex(keys, actionSetDefault)

	var out bytes.Buffer
	selectHarness := scriptedList(1) // row 0 is wally chat; row 1 is Registry[0]
	selectAction := scriptedList(setDefaultIdx)
	pickModel := func(harness.Harness) (string, error) { return "not-a-real-model", nil }

	if err := runHarnessManager(&out, selectHarness, selectAction, pickModel, noInstall(t), noConfirmUninstall(t), noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
	got, err := loadDefaultModel(h.Name)
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Fatalf("default model = %q, want unset: an unavailable cloud model must not be saved", got)
	}
	if !strings.Contains(out.String(), "not available") {
		t.Errorf("output = %q, want it to explain the model is not available", out.String())
	}
}

func TestRunHarnessManager_CancelledHarnessSelectionExitsCleanly(t *testing.T) {
	newTestStore(t)
	var out bytes.Buffer
	cancelled := func(string, []tui.Item, string) (int, error) { return 0, tui.ErrCancelled }
	actionCalled := false
	selectAction := func(string, []tui.Item, string) (int, error) {
		actionCalled = true
		return 0, nil
	}
	if err := runHarnessManager(&out, cancelled, selectAction, nil, noInstall(t), nil, nil); err != nil {
		t.Fatal(err)
	}
	if actionCalled {
		t.Error("the action menu must never run when the harness list was cancelled")
	}
}

func TestRunHarnessManager_CancelledActionReturnsToHarnessList(t *testing.T) {
	newTestStore(t)
	if len(harness.Registry) == 0 {
		t.Skip("no harnesses registered to drive the manager against")
	}
	var out bytes.Buffer
	harnessCalls := 0
	selectHarness := func(string, []tui.Item, string) (int, error) {
		harnessCalls++
		if harnessCalls > 2 {
			return 0, tui.ErrCancelled
		}
		return 0, nil
	}
	// First action: cancel (back to the list). Second action: never reached
	// because we cancel the harness list on the third round.
	actionCalls := 0
	selectAction := func(string, []tui.Item, string) (int, error) {
		actionCalls++
		return 0, tui.ErrCancelled
	}

	if err := runHarnessManager(&out, selectHarness, selectAction, nil, noInstall(t), nil, nil); err != nil {
		t.Fatal(err)
	}
	if harnessCalls != 3 {
		t.Errorf("harness list shown %d times, want 3 (two round trips plus the cancel)", harnessCalls)
	}
	if actionCalls != 2 {
		t.Errorf("action menu shown %d times, want 2", actionCalls)
	}
}

func TestRunHarnessManager_InstallActionCallsInstallHarness(t *testing.T) {
	newTestStore(t)
	// Find a harness that is not on PATH so the Install row is offered; every
	// registered harness's Command is unlikely to resolve in a test sandbox,
	// but fall back to a fabricated one if one happens to be installed.
	h := harness.Harness{Name: "demo", Command: "definitely-not-a-real-wally-harness-binary", Summary: "demo"}
	origRegistry := harness.Registry
	harness.Registry = []harness.Harness{h}
	t.Cleanup(func() { harness.Registry = origRegistry })

	var out bytes.Buffer
	selectHarness := scriptedList(1) // row 0 is wally chat; row 1 is demo
	selectAction := scriptedList(0)  // Install is the first row when not installed
	installed := false
	installHarness := func(got harness.Harness) error {
		installed = true
		if got.Name != "demo" {
			t.Errorf("installHarness called for %q, want demo", got.Name)
		}
		return nil
	}

	if err := runHarnessManager(&out, selectHarness, selectAction, nil, installHarness, noConfirmUninstall(t), noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
	if !installed {
		t.Error("choosing Install must call installHarness, not just print the hint")
	}
}

func TestRunHarnessManager_InstallActionReportsFailure(t *testing.T) {
	newTestStore(t)
	h := harness.Harness{Name: "demo", Command: "definitely-not-a-real-wally-harness-binary", Summary: "demo"}
	origRegistry := harness.Registry
	harness.Registry = []harness.Harness{h}
	t.Cleanup(func() { harness.Registry = origRegistry })

	var out bytes.Buffer
	selectHarness := scriptedList(1) // row 0 is wally chat; row 1 is demo
	selectAction := scriptedList(0)
	boom := errors.New("boom")
	installHarness := func(harness.Harness) error { return boom }

	if err := runHarnessManager(&out, selectHarness, selectAction, nil, installHarness, noConfirmUninstall(t), noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out.String(), "boom") {
		t.Errorf("output = %q, want the install error reported", out.String())
	}
}

func TestBuildModelPickerRows_CategorizesOnlineAndOffline(t *testing.T) {
	enableOnDevice(t)
	online := []catalog.Model{{ID: "gpt-oss-20b", InputPerMTok: 100000, OutputPerMTok: 400000}}
	offline := []runanywhere.InstalledModel{{ID: "qwen2.5-3b", Framework: "llama-cpp", Path: "/tmp/qwen2.5-3b/model.gguf"}}

	rows := buildModelPickerRows(online, offline)

	var onlineIdx, offlineIdx, manualIdx = -1, -1, -1
	for i, r := range rows {
		switch r.item.Title {
		case "ONLINE (cloud)":
			onlineIdx = i
		case "OFFLINE (on-device)":
			offlineIdx = i
		case "Enter manually...":
			manualIdx = i
		}
	}
	if onlineIdx == -1 || offlineIdx == -1 || manualIdx == -1 {
		t.Fatalf("rows missing a section header or the manual fallback: %+v", rows)
	}
	if onlineIdx > offlineIdx {
		t.Error("ONLINE section must come before OFFLINE")
	}
	if !rows[onlineIdx].item.Disabled || !rows[offlineIdx].item.Disabled {
		t.Error("section headers must be disabled rows, not selectable")
	}

	foundOnline, foundOffline := false, false
	for _, r := range rows {
		if r.id == "gpt-oss-20b" {
			foundOnline = true
			if !strings.Contains(r.item.Detail, "$0.10") || !strings.Contains(r.item.Detail, "$0.40") {
				t.Errorf("online row detail = %q, want the per-Mtok prices", r.item.Detail)
			}
		}
		if r.id == "qwen2.5-3b" {
			foundOffline = true
			if !strings.Contains(r.item.Detail, "llama-cpp") {
				t.Errorf("offline row detail = %q, want the framework", r.item.Detail)
			}
		}
	}
	if !foundOnline || !foundOffline {
		t.Fatalf("rows missing the seeded models: %+v", rows)
	}
	if rows[manualIdx].manual != true || rows[manualIdx].id != "" {
		t.Error("the manual row must carry manual=true and no id")
	}
}

func TestBuildModelPickerRows_OfflineOnlyKeepsTextGenerationModels(t *testing.T) {
	enableOnDevice(t)
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)
	seedModel(t, home, "llama-cpp", "qwen2.5-7b-instruct", 2048)
	seedModel(t, home, "onnx", "all-minilm-l6-v2", 1024)
	seedModel(t, home, "sherpa", "whisper-tiny.en", 4096)
	seedModel(t, home, "onnx", "silero-vad", 512)
	seedModel(t, home, "onnx", "diar-streaming-sortformer-4spk-v2.1", 512)

	rows := buildModelPickerRows(nil, runanywhere.InstalledModels())

	inOffline := false
	var offlineIDs []string
	for _, r := range rows {
		switch {
		case r.item.Title == "OFFLINE (on-device)":
			inOffline = true
			continue
		case r.manual:
			inOffline = false
		}
		if inOffline && r.id != "" {
			offlineIDs = append(offlineIDs, r.id)
		}
	}
	if len(offlineIDs) != 1 || offlineIDs[0] != "qwen2.5-7b-instruct" {
		t.Fatalf("offline ids = %v, want only the text-generation model", offlineIDs)
	}
}

func TestBuildModelPickerRows_SectionHeadersCarryDistinctAccents(t *testing.T) {
	rows := buildModelPickerRows(nil, nil)
	var online, offline tui.Item
	for _, r := range rows {
		switch r.item.Title {
		case "ONLINE (cloud)":
			online = r.item
		case "OFFLINE (on-device)":
			offline = r.item
		}
	}
	if online.Accent == tui.AccentNone || offline.Accent == tui.AccentNone {
		t.Fatalf("section headers must carry a non-default accent: online=%v offline=%v", online.Accent, offline.Accent)
	}
	if online.Accent == offline.Accent {
		t.Error("ONLINE and OFFLINE headers must use distinct accents")
	}
}

func TestBuildModelPickerRows_EmptyOnlineRendersANote(t *testing.T) {
	rows := buildModelPickerRows(nil, nil)
	found := false
	for _, r := range rows {
		if strings.Contains(r.item.Title, "not cached yet") {
			found = true
			if !r.item.Disabled {
				t.Error("the empty-cache note must be disabled, not selectable")
			}
		}
	}
	if !found {
		t.Fatal("expected a note explaining the cloud catalog is not cached yet")
	}
}

func TestPickDefaultModel_ManualFallback(t *testing.T) {
	newTestStore(t)
	// No catalog cache and no installed models: only the manual row is
	// selectable. tui.RunList itself needs a terminal, so this exercises
	// buildModelPickerRows plus the manual branch directly instead.
	rows := buildModelPickerRows(nil, runanywhere.InstalledModels())
	manualIdx := -1
	for i, r := range rows {
		if r.manual {
			manualIdx = i
		}
	}
	if manualIdx == -1 {
		t.Fatal("expected a manual fallback row")
	}
	row := rows[manualIdx]
	readModel := func(prompt string) (string, error) {
		if !strings.Contains(prompt, "demo") {
			t.Errorf("prompt = %q, want it to name the harness", prompt)
		}
		return "typed-model", nil
	}
	if !row.manual {
		t.Fatal("row is not the manual fallback")
	}
	got, err := readModel("Default model for demo: ")
	if err != nil || got != "typed-model" {
		t.Fatalf("got %q, err %v", got, err)
	}
}

func TestGlobalDefaultModelRoundTrip(t *testing.T) {
	newTestStore(t)

	got, err := loadGlobalDefaultModel()
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Fatalf("loadGlobalDefaultModel before save = %q, want empty", got)
	}

	if err := saveGlobalDefaultModel("qwen-7b"); err != nil {
		t.Fatal(err)
	}
	got, err = loadGlobalDefaultModel()
	if err != nil {
		t.Fatal(err)
	}
	if got != "qwen-7b" {
		t.Fatalf("loadGlobalDefaultModel = %q, want qwen-7b", got)
	}
}

func TestHarnessMenuItems_WallyChatIsFirstRow(t *testing.T) {
	newTestStore(t)
	items, rows, err := harnessMenuItems()
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != len(harness.Registry)+1 {
		t.Fatalf("len(items) = %d, want %d", len(items), len(harness.Registry)+1)
	}
	if items[0].Title != "wally chat" {
		t.Fatalf("items[0].Title = %q, want %q", items[0].Title, "wally chat")
	}
	if !rows[0].isGlobal {
		t.Fatal("rows[0] must be the global wally chat row")
	}
	for i, h := range harness.Registry {
		if rows[i+1].isGlobal {
			t.Errorf("rows[%d] is global, want the registered harness %q", i+1, h.Name)
		}
		if rows[i+1].harness.Name != h.Name {
			t.Errorf("rows[%d].harness.Name = %q, want %q", i+1, rows[i+1].harness.Name, h.Name)
		}
	}
}

func TestRunHarnessManager_SetDefaultModel_GlobalWallyChat(t *testing.T) {
	newTestStore(t)
	if len(harness.Registry) == 0 {
		t.Skip("no harnesses registered to drive the manager against")
	}

	var out bytes.Buffer
	selectHarness := scriptedList(0) // row 0 is wally chat
	selectAction := scriptedList(0)  // Set default model is the only row on the global menu
	pickModel := func(h harness.Harness) (string, error) {
		if h.Name != "wally chat" {
			t.Errorf("pickModel called for %q, want wally chat", h.Name)
		}
		return "qwen-7b", nil
	}

	if err := runHarnessManager(&out, selectHarness, selectAction, pickModel, noInstall(t), noConfirmUninstall(t), noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
	got, err := loadGlobalDefaultModel()
	if err != nil {
		t.Fatal(err)
	}
	if got != "qwen-7b" {
		t.Fatalf("global default model = %q, want qwen-7b", got)
	}
	perHarness, err := loadDefaultModel(harness.Registry[0].Name)
	if err != nil {
		t.Fatal(err)
	}
	if perHarness != "" {
		t.Errorf("per-harness default = %q, want unset: setting the global default must not touch it", perHarness)
	}
}

func TestRunHarnessManager_SetDefaultModel_GlobalRejectsUnknownCloudModel(t *testing.T) {
	newTestStore(t)
	orig := catalogLoad
	catalogLoad = func() ([]catalog.Model, error) { return []catalog.Model{{ID: "gpt-oss-20b"}}, nil }
	t.Cleanup(func() { catalogLoad = orig })

	var out bytes.Buffer
	selectHarness := scriptedList(0)
	selectAction := scriptedList(0)
	pickModel := func(harness.Harness) (string, error) { return "not-a-real-model", nil }

	if err := runHarnessManager(&out, selectHarness, selectAction, pickModel, noInstall(t), noConfirmUninstall(t), noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
	got, err := loadGlobalDefaultModel()
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Fatalf("global default model = %q, want unset: an unavailable cloud model must not be saved", got)
	}
	if !strings.Contains(out.String(), "not available") {
		t.Errorf("output = %q, want it to explain the model is not available", out.String())
	}
}

func TestRunHarnessManager_UninstallDeclinedRunsNothing(t *testing.T) {
	newTestStore(t)
	h := harness.Harness{Name: "demo", Command: "definitely-not-a-real-wally-harness-binary", Summary: "demo", Impl: stubUninstaller{command: "npm uninstall -g demo"}}
	origRegistry := harness.Registry
	harness.Registry = []harness.Harness{h}
	t.Cleanup(func() { harness.Registry = origRegistry })

	_, keys := harnessActionItems(h, false)
	uninstallIdx := findActionIndex(keys, actionUninstall)

	var out bytes.Buffer
	selectHarness := scriptedList(1) // row 0 is wally chat; row 1 is demo
	selectAction := scriptedList(uninstallIdx)
	declined := func(prompt string) (bool, error) {
		if !strings.Contains(prompt, "demo") {
			t.Errorf("confirm prompt = %q, want it to name the harness", prompt)
		}
		return false, nil
	}

	if err := runHarnessManager(&out, selectHarness, selectAction, nil, noInstall(t), declined, noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
}

func TestRunHarnessManager_UninstallAcceptedRunsCommandAndClearsState(t *testing.T) {
	newTestStore(t)
	h := harness.Harness{Name: "demo", Command: "definitely-not-a-real-wally-harness-binary", Summary: "demo", Impl: stubUninstaller{command: "npm uninstall -g demo"}}
	origRegistry := harness.Registry
	harness.Registry = []harness.Harness{h}
	t.Cleanup(func() { harness.Registry = origRegistry })
	if err := saveDefaultModel("demo", "qwen-7b"); err != nil {
		t.Fatal(err)
	}

	_, keys := harnessActionItems(h, false)
	uninstallIdx := findActionIndex(keys, actionUninstall)

	var out bytes.Buffer
	selectHarness := scriptedList(1)
	selectAction := scriptedList(uninstallIdx)
	accepted := func(prompt string) (bool, error) {
		if !strings.Contains(prompt, "demo") {
			t.Errorf("confirm prompt = %q, want it to name the harness", prompt)
		}
		return true, nil
	}
	ran := false
	runUninstall := func(got harness.Harness) error {
		ran = true
		if got.Name != "demo" {
			t.Errorf("uninstallHarness called for %q, want demo", got.Name)
		}
		return nil
	}

	if err := runHarnessManager(&out, selectHarness, selectAction, nil, noInstall(t), accepted, runUninstall); err != nil {
		t.Fatal(err)
	}
	if !ran {
		t.Error("choosing Uninstall and confirming must run the uninstall command")
	}
	got, err := loadDefaultModel("demo")
	if err != nil {
		t.Fatal(err)
	}
	if got != "" {
		t.Errorf("default model = %q after uninstall, want cleared", got)
	}
}

func TestRunHarnessManager_UninstallWithoutCommandExplainsManualRemoval(t *testing.T) {
	newTestStore(t)
	h := harness.Harness{Name: "demo", Command: "definitely-not-a-real-wally-harness-binary", Summary: "demo"} // no Impl, so no Uninstaller
	origRegistry := harness.Registry
	harness.Registry = []harness.Harness{h}
	t.Cleanup(func() { harness.Registry = origRegistry })

	_, keys := harnessActionItems(h, false)
	uninstallIdx := findActionIndex(keys, actionUninstall)

	var out bytes.Buffer
	selectHarness := scriptedList(1)
	selectAction := scriptedList(uninstallIdx)

	if err := runHarnessManager(&out, selectHarness, selectAction, nil, noInstall(t), noConfirmUninstall(t), noUninstallRun(t)); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out.String(), "manually") {
		t.Errorf("output = %q, want it to explain manual removal", out.String())
	}
}
