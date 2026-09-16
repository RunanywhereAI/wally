package runanywhere

import "testing"

func TestIsTextGenerationModel(t *testing.T) {
	tests := []struct {
		id   string
		want bool
	}{
		{"qwen2.5-3b", true},
		{"llama-3.1-8b-instruct", true},
		{"lfm2.5-1.2b-instruct-q4_k_m", true},
		{"all-minilm-l6-v2", false},
		{"gte-base", false},
		{"lfm2.5-embedding-350m-8bit", false},
		{"whisper-tiny.en", false},
		{"sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8", false},
		{"silero-vad", false},
		{"diar-streaming-sortformer-4spk-v2.1", false},
	}
	for _, tt := range tests {
		t.Run(tt.id, func(t *testing.T) {
			got := IsTextGenerationModel(InstalledModel{ID: tt.id})
			if got != tt.want {
				t.Errorf("IsTextGenerationModel(%q) = %v, want %v", tt.id, got, tt.want)
			}
		})
	}
}

func TestTextGenerationModelsFiltersOutNonText(t *testing.T) {
	models := []InstalledModel{
		{ID: "qwen2.5-3b", Framework: "LlamaCpp"},
		{ID: "all-minilm-l6-v2", Framework: "ONNX"},
		{ID: "whisper-tiny.en", Framework: "Sherpa"},
		{ID: "lfm2.5-1.2b-instruct-mlx-4bit", Framework: "MLX"},
	}
	got := TextGenerationModels(models)
	if len(got) != 2 || got[0].ID != "qwen2.5-3b" || got[1].ID != "lfm2.5-1.2b-instruct-mlx-4bit" {
		t.Fatalf("TextGenerationModels = %+v, want only the two text models", got)
	}
}
