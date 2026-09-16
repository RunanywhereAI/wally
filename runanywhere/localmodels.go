package runanywhere

import (
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

const manifestName = ".rac-manifest.binpb"

// ModelsRoot is the on-device model store: {home}/RunAnywhere/Models, or
// {home}/Models when home already ends in the RunAnywhere base. home is
// RUNANYWHERE_HOME, else the platform config dir. Empty when none resolves.
func ModelsRoot() string {
	home := strings.TrimSpace(os.Getenv("RUNANYWHERE_HOME"))
	if home == "" {
		dir, err := os.UserConfigDir()
		if err != nil {
			return ""
		}
		home = filepath.Join(dir, "RunAnywhere")
	}
	nested := filepath.Join(home, "RunAnywhere", "Models")
	if isDir(nested) {
		return nested
	}
	return filepath.Join(home, "Models")
}

// InstalledModels walks the model store for models on disk. A directory counts
// when it holds a manifest or a weight file, matching a hand-placed model as
// readily as a downloaded one.
func InstalledModels() []InstalledModel {
	root := ModelsRoot()
	if !isDir(root) {
		return nil
	}
	var models []InstalledModel
	frameworks, _ := os.ReadDir(root)
	for _, fw := range frameworks {
		if !fw.IsDir() {
			continue
		}
		fwDir := filepath.Join(root, fw.Name())
		entries, _ := os.ReadDir(fwDir)
		for _, e := range entries {
			if !e.IsDir() {
				continue
			}
			dir := filepath.Join(fwDir, e.Name())
			weights, bytes, manifest := scanModelDir(dir)
			if !manifest && weights == "" {
				continue
			}
			models = append(models, InstalledModel{
				ID:        e.Name(),
				Framework: fw.Name(),
				Dir:       dir,
				Path:      weights,
				Bytes:     bytes,
			})
		}
	}
	sort.Slice(models, func(i, j int) bool { return models[i].ID < models[j].ID })
	return models
}

func scanModelDir(dir string) (weights string, bytes int64, manifest bool) {
	filepath.WalkDir(dir, func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return nil
		}
		if info, e := d.Info(); e == nil {
			bytes += info.Size()
		}
		switch {
		case d.Name() == manifestName:
			manifest = true
		case weights == "" && isWeightFile(d.Name()):
			weights = p
		}
		return nil
	})
	return weights, bytes, manifest
}

func findInstalledModel(id string) (InstalledModel, bool) {
	for _, m := range InstalledModels() {
		if m.ID == id {
			return m, true
		}
	}
	return InstalledModel{}, false
}

// FindInstalledModel returns the on-disk record for id, if present.
func FindInstalledModel(id string) (InstalledModel, bool) {
	return findInstalledModel(id)
}

// EngineServable reports whether the linked engine can serve this model. GGUF
// runs in-process via rac_server; MLX runs through the spawned wally-mlx helper.
// Other formats (ONNX/CoreML/Sherpa) have no engine yet. An empty Path means
// the weights are not downloaded.
func EngineServable(m InstalledModel) bool {
	if m.Path == "" {
		return false
	}
	if strings.HasSuffix(strings.ToLower(m.Path), ".gguf") {
		return true
	}
	return strings.EqualFold(m.Framework, "MLX")
}

func IsLocalModel(model string) bool {
	_, ok := findInstalledModel(model)
	return ok
}

// toolCallingFamilies are the on-device model families known to support tool
// calling, used to pick a default for the not-signed-in launch path.
var toolCallingFamilies = []string{
	"qwen", "hermes", "llama-3", "llama3", "mistral", "functionary",
	"firefunction", "command-r", "gemma-3", "gemma3",
}

func SupportsToolCalling(id string) bool {
	low := strings.ToLower(id)
	for _, fam := range toolCallingFamilies {
		if strings.Contains(low, fam) {
			return true
		}
	}
	return false
}

func FirstToolCallingLocalModel() (InstalledModel, bool) {
	for _, m := range InstalledModels() {
		if SupportsToolCalling(m.ID) {
			return m, true
		}
	}
	return InstalledModel{}, false
}

// nonTextKeywords are id substrings that mark a model as something other
// than a text-generation LLM: embedding, rerank, speech-to-text,
// text-to-speech, voice activity detection, diarization, segmentation, or
// vision-language. The on-device kit manifest (.rac-manifest.binpb next to
// the weights) carries an id and a marketing tagline but no decodable
// modality/task field short of vendoring the kit's protobuf schema, which
// this stdlib-only package does not do, so IsTextGenerationModel falls back
// to this documented id heuristic instead of guessing at the manifest's wire
// format. A model is text generation unless its id contains one of these.
var nonTextKeywords = []string{
	"embed", "embedding", "minilm", "gte", "rerank",
	"whisper", "parakeet", "stt", "asr",
	"tts",
	"vad", "silero",
	"diar", "sortformer", "nemo",
	"segment",
	"vlm", "clip",
}

// IsTextGenerationModel reports whether m is a text-generation LLM, the only
// kind of model a default-model picker for a coding harness or chat should
// offer. LlamaCpp and text MLX/CoreML LLMs (qwen, llama, gemma, mistral,
// lfm2.5-*-instruct, phi, ...) match none of nonTextKeywords and are kept.
func IsTextGenerationModel(m InstalledModel) bool {
	low := strings.ToLower(m.ID)
	for _, kw := range nonTextKeywords {
		if strings.Contains(low, kw) {
			return false
		}
	}
	return true
}

// TextGenerationModels filters models down to text-generation LLMs, for the
// harness default-model picker.
func TextGenerationModels(models []InstalledModel) []InstalledModel {
	out := make([]InstalledModel, 0, len(models))
	for _, m := range models {
		if IsTextGenerationModel(m) {
			out = append(out, m)
		}
	}
	return out
}

func isDir(p string) bool {
	fi, err := os.Stat(p)
	return err == nil && fi.IsDir()
}

func isWeightFile(name string) bool {
	switch strings.ToLower(filepath.Ext(name)) {
	case ".gguf", ".safetensors", ".onnx", ".bin":
		return true
	}
	return false
}
