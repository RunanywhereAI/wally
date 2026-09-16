//go:build ondevice

// Downloads a model by id through the kit's download orchestrator: register
// the source URL, plan, start, poll for progress, cancel on ctx.Done(). The
// wire format for the five calls this file makes is
// runanywhere.v1.{RegisterModelFromUrlRequest, DownloadPlanRequest,
// DownloadPlanResult, DownloadStartRequest, DownloadStartResult,
// DownloadSubscribeRequest, DownloadProgress, DownloadCancelRequest} from
// the kit's own share/runanywhere/idl/{model_types,download_service}.proto.
// Rather than vendor a protoc toolchain and the transitive proto graph
// (errors.proto, hardware_profile.proto, rac_options.proto,
// thinking_tag_pattern.proto) for eight messages, this file hand-encodes
// and hand-decodes proto3 wire bytes directly for the handful of scalar
// fields these calls actually need -- the field numbers below are read
// straight off the .proto files and are the wire contract, not a guess.
// Anything this file does not need (most of ModelInfo, most of
// DownloadPlanResult) is round-tripped as an opaque length-delimited blob
// rather than decoded, which is also why the register response's raw bytes
// get embedded as-is into the plan request instead of being rebuilt field
// by field.
package runanywhere

/*
#include <stdlib.h>
#include "rac/core/rac_core.h"
#include "rac/infrastructure/download/rac_download_orchestrator.h"
#include "rac/infrastructure/model_management/rac_model_registry.h"
*/
import "C"

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"net/http"
	"sort"
	"strings"
	"time"
	"unsafe"
)

// Proto field numbers, copied from the kit's share/runanywhere/idl/*.proto.
// "Core"-persisted messages (ModelInfo) declare their field numbers
// permanent in the schema comment; the download_service ones are wire-owned
// the same way commons treats the rest of its C ABI.
const (
	fieldRegisterURL       = 1 // RegisterModelFromUrlRequest.url
	fieldRegisterFramework = 3 // RegisterModelFromUrlRequest.framework
	fieldRegisterCategory  = 4 // RegisterModelFromUrlRequest.category

	fieldModelInfoID = 1 // ModelInfo.id

	fieldPlanReqModelID = 1 // DownloadPlanRequest.model_id
	fieldPlanReqModel   = 2 // DownloadPlanRequest.model

	fieldPlanCanStart   = 1  // DownloadPlanResult.can_start
	fieldPlanTotalBytes = 4  // DownloadPlanResult.total_bytes
	fieldPlanError      = 14 // DownloadPlanResult.error

	fieldStartReqModelID = 1 // DownloadStartRequest.model_id
	fieldStartReqPlan    = 2 // DownloadStartRequest.plan

	fieldStartAccepted = 1 // DownloadStartResult.accepted
	fieldStartTaskID   = 2 // DownloadStartResult.task_id
	fieldStartError    = 8 // DownloadStartResult.error

	fieldSubscribeModelID = 1 // DownloadSubscribeRequest.model_id
	fieldSubscribeTaskID  = 2 // DownloadSubscribeRequest.task_id

	fieldProgressBytesDownloaded = 3  // DownloadProgress.bytes_downloaded
	fieldProgressTotalBytes      = 4  // DownloadProgress.total_bytes
	fieldProgressState           = 8  // DownloadProgress.state
	fieldProgressError           = 21 // DownloadProgress.error

	fieldCancelReqTaskID  = 1 // DownloadCancelRequest.task_id
	fieldCancelReqModelID = 2 // DownloadCancelRequest.model_id

	fieldSDKErrorMessage = 3 // SDKError.message

	// RegisterMultiFileModelRequest (model_types.proto), for MLX and any other
	// multi-file (safetensors + tokenizer + config) model.
	fieldMFID        = 1  // id
	fieldMFName      = 2  // name
	fieldMFFramework = 3  // framework
	fieldMFFiles     = 4  // repeated ModelFileDescriptor
	fieldMFCategory  = 5  // optional category
	fieldMFFormat    = 6  // optional format
	fieldMFSource    = 13 // optional source

	// ModelFileDescriptor (model_types.proto).
	fieldFDURL      = 1 // url
	fieldFDFilename = 2 // filename
	fieldFDSize     = 4 // optional size_bytes
)

// DownloadState values, from download_service.proto's DownloadState enum.
const (
	downloadStateCompleted = 5
	downloadStateFailed    = 6
	downloadStateCancelled = 7
)

// InferenceFramework / ModelCategory values this file uses, from
// model_types.proto.
const (
	inferenceFrameworkLlamaCpp = 2
	inferenceFrameworkMLX      = 7
	modelCategoryLanguage      = 1
	modelCategoryEmbedding     = 8
	modelFormatSafetensors     = 10
	modelSourceRemote          = 1
)

// builtinModels ports wally-legacy's built-in catalog
// (wally-legacy/src/catalog/catalog.cpp, kCatalog[]) -- there is no kit-side
// catalog API to query instead: the kit's rac_model_registry_list_* family
// only lists models already saved to the in-memory registry (by this same
// Download call, or by rac_register_model_from_url_proto/_register_model),
// and RunAnywhere_HAS_CLOUD is FALSE in this kit, so there is nothing
// resembling a remote/built-in catalog to call. The alternative was porting
// wally-legacy's hand-maintained CLI-application data, so that is what this
// is: every LlamaCpp-framework, single-file (no mmproj/companion file)
// entry in kCatalog, extracted mechanically from the source (id, url,
// category) rather than retyped by hand. Excluded on purpose: VLM pairs and
// other multi-file artifacts (smolvlm2-256m-video-instruct-q8_0,
// lfm2-vl-450m-q8_0, lfm2.5-vl-3b-q4_k_m, qwen2-vl-2b-instruct-q4_k_m,
// fara1.5-4b-q4_k_m, muse-glimmer-30b-q4_k_xl,
// nemotron-3-nano-omni-30b-a3b-reasoning-q4_k_m) -- rac_register_model_from_url_proto
// registers exactly one URL -- and every non-LlamaCpp-framework entry (ONNX,
// Sherpa, CoreML). MLX is not excluded: it is multi-file, so it downloads
// through mlxModels + rac_register_multi_file_model_proto below, and runs via
// the wally-mlx helper. The remaining frameworks have no engine yet.
// EMBEDDING-category entries download and register the same as LANGUAGE
// ones, but nothing in this engine serves an embedding request yet (Start
// only wires /v1/chat/completions); they are listed for parity with the
// ported source, not because pull+serve is proven for them.
type modelSource struct {
	url      string
	category int32
}

var builtinModels = map[string]modelSource{
	"qwen3-0.6b":                        {url: "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf", category: modelCategoryLanguage},
	"qwen3-1.7b-q4_k_m":                 {url: "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q4_K_M.gguf", category: modelCategoryLanguage},
	"qwen3-4b-q4_k_m":                   {url: "https://huggingface.co/unsloth/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf", category: modelCategoryLanguage},
	"bonsai-1.7b-q1_0":                  {url: "https://huggingface.co/prism-ml/Bonsai-1.7B-gguf/resolve/main/Bonsai-1.7B-Q1_0.gguf", category: modelCategoryLanguage},
	"bonsai-4b-q1_0":                    {url: "https://huggingface.co/prism-ml/Bonsai-4B-gguf/resolve/main/Bonsai-4B-Q1_0.gguf", category: modelCategoryLanguage},
	"bonsai-8b-q1_0":                    {url: "https://huggingface.co/prism-ml/Bonsai-8B-gguf/resolve/main/Bonsai-8B-Q1_0.gguf", category: modelCategoryLanguage},
	"bonsai-27b-q1_0":                   {url: "https://huggingface.co/prism-ml/Bonsai-27B-gguf/resolve/main/Bonsai-27B-Q1_0.gguf", category: modelCategoryLanguage},
	"ternary-bonsai-1.7b-q2_0-g64":      {url: "https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-gguf/resolve/983b5dec2ff16aab79990711ba0f828a499a7e6a/Ternary-Bonsai-1.7B-Q2_0_g64.gguf", category: modelCategoryLanguage},
	"ternary-bonsai-4b-q2_0-g64":        {url: "https://huggingface.co/prism-ml/Ternary-Bonsai-4B-gguf/resolve/a3eb42bafe873f9686bc97486c43b72ef7d75ec8/Ternary-Bonsai-4B-Q2_0_g64.gguf", category: modelCategoryLanguage},
	"ternary-bonsai-8b-q2_0-g64":        {url: "https://huggingface.co/prism-ml/Ternary-Bonsai-8B-gguf/resolve/c2aefbeb4b24469cd11579c3384b990404c17a30/Ternary-Bonsai-8B-Q2_0_g64.gguf", category: modelCategoryLanguage},
	"maple-preview-tq1_0-q4_k":          {url: "https://huggingface.co/deepgrove/maple-preview-GGUF/resolve/f5466f918e0c50cdb9d4d47a6f35813509a42a30/maple-preview-TQ1_0-head-Q4_K.gguf", category: modelCategoryLanguage},
	"llama-3.2-3b":                      {url: "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/Llama-3.2-3B-Instruct-Q4_K_M.gguf", category: modelCategoryLanguage},
	"lfm2-350m-q8_0":                    {url: "https://huggingface.co/LiquidAI/LFM2-350M-GGUF/resolve/main/LFM2-350M-Q8_0.gguf", category: modelCategoryLanguage},
	"smollm2-360m-q8_0":                 {url: "https://huggingface.co/prithivMLmods/SmolLM2-360M-GGUF/resolve/main/SmolLM2-360M.Q8_0.gguf", category: modelCategoryLanguage},
	"gemma-4-e2b-it-q4_k_m":             {url: "https://huggingface.co/unsloth/gemma-4-E2B-it-GGUF/resolve/0314792d7f1f7e229411f620751375812bb9faf2/gemma-4-E2B-it-Q4_K_M.gguf", category: modelCategoryLanguage},
	"gemma-4-e4b-it-q4_k_m":             {url: "https://huggingface.co/unsloth/gemma-4-E4B-it-GGUF/resolve/bfc15c382204943c3a8fff0c750b94ae2364d7a3/gemma-4-E4B-it-Q4_K_M.gguf", category: modelCategoryLanguage},
	"gemma-4-12b-it-q4_k_m":             {url: "https://huggingface.co/unsloth/gemma-4-12b-it-GGUF/resolve/fc034cfff751157913579611efad8462ac1be606/gemma-4-12b-it-Q4_K_M.gguf", category: modelCategoryLanguage},
	"gemma-4-26b-a4b-it-q4_k_xl":        {url: "https://huggingface.co/unsloth/gemma-4-26B-A4B-it-GGUF/resolve/c099eb48e663fd284577b04978a94ffccb261841/gemma-4-26B-A4B-it-UD-Q4_K_XL.gguf", category: modelCategoryLanguage},
	"gemma-4-31b-it-q4_k_m":             {url: "https://huggingface.co/unsloth/gemma-4-31B-it-GGUF/resolve/c1ac76e99d5513b141e8adde7288b85c3f9c32ec/gemma-4-31B-it-Q4_K_M.gguf", category: modelCategoryLanguage},
	"gemma-4-31b-it-ud-q2_k_xl":         {url: "https://huggingface.co/unsloth/gemma-4-31B-it-GGUF/resolve/c1ac76e99d5513b141e8adde7288b85c3f9c32ec/gemma-4-31B-it-UD-Q2_K_XL.gguf", category: modelCategoryLanguage},
	"qwen3.6-35b-a3b-q4_k_m":            {url: "https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF/resolve/a483e9e6cbd595906af30beda3187c2663a1118c/Qwen3.6-35B-A3B-UD-Q4_K_M.gguf", category: modelCategoryLanguage},
	"qwen3.8-27b-q4_k_m":                {url: "https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/f1bfb127c64f7072bdd2cad55f258b9c8b2910fe/Qwen3.8-27B-Q4_K_M.gguf", category: modelCategoryLanguage},
	"granite-4.1-3b-q4_k_m":             {url: "https://huggingface.co/unsloth/granite-4.1-3b-GGUF/resolve/5b88826e4b80789548180f8faab39c5cf68772c9/granite-4.1-3b-Q4_K_M.gguf", category: modelCategoryLanguage},
	"granite-4.1-8b-q4_k_m":             {url: "https://huggingface.co/unsloth/granite-4.1-8b-GGUF/resolve/6f9671f73eb03273bc09319194b8a4e810e03a8f/granite-4.1-8b-Q4_K_M.gguf", category: modelCategoryLanguage},
	"granite-4.1-30b-q4_k_m":            {url: "https://huggingface.co/unsloth/granite-4.1-30b-GGUF/resolve/6cb34f31b11ca4c1433de1af7391dac46de4e666/granite-4.1-30b-Q4_K_M.gguf", category: modelCategoryLanguage},
	"nemotron-3-embed-1b-q4_k_m":        {url: "https://huggingface.co/zenmagnets/Nemotron-3-Embed-1B-Q4_K_M-GGUF/resolve/06df1fde6f7009c91f6cc3cd520081921929a678/nemotron-3-embed-1b-q4_k_m.gguf", category: modelCategoryEmbedding},
	"llama-nemotron-embed-1b-v2-q4_k_m": {url: "https://huggingface.co/mykor/llama-nemotron-embed-1b-v2-GGUF/resolve/bf7c9832b1d76f86777379e58b7b74805ee58006/llama-nemotron-embed-1B-v2-Q4_K_M.gguf", category: modelCategoryEmbedding},
	"llama-embed-nemotron-8b-q4_k_m":    {url: "https://huggingface.co/mradermacher/llama-embed-nemotron-8b-GGUF/resolve/e7ae3cbae4f7693bbd75ec959bf293f39e1f2e25/llama-embed-nemotron-8b.Q4_K_M.gguf", category: modelCategoryEmbedding},
	"bge-reranker-v2-m3-q4_k_m":         {url: "https://huggingface.co/gpustack/bge-reranker-v2-m3-GGUF/resolve/main/bge-reranker-v2-m3-Q4_K_M.gguf", category: modelCategoryEmbedding},
}

// mlxModels maps a pull id to its HuggingFace repo. MLX models are multi-file
// (config.json + model.safetensors + tokenizer), which rac_register_model_from_url_proto
// (one URL) cannot register, so these go through rac_register_multi_file_model_proto
// with a file list fetched from the HF tree API at pull time. Every repo here
// was verified to exist on huggingface.co. The download destination the kit
// computes for framework MLX is Models/MLX/<id>, exactly where InstalledModels
// scans and where the wally-mlx helper's registry discovery looks.
var mlxModels = map[string]string{
	"mlx-qwen3-0.6b-4bit":            "mlx-community/Qwen3-0.6B-4bit",
	"mlx-qwen3-1.7b-4bit":            "mlx-community/Qwen3-1.7B-4bit",
	"mlx-qwen2.5-0.5b-instruct-4bit": "mlx-community/Qwen2.5-0.5B-Instruct-4bit",
	"mlx-llama-3.2-1b-instruct-4bit": "mlx-community/Llama-3.2-1B-Instruct-4bit",
	"mlx-lfm2-350m-4bit":             "mlx-community/LFM2-350M-4bit",
	"mlx-lfm2-1.2b-4bit":             "mlx-community/LFM2-1.2B-4bit",
	"mlx-smollm2-360m-instruct-6bit": "mlx-community/SmolLM2-360M-Instruct-6bit",
}

type racDownloader struct{}

// DownloadCatalog implements runanywhere.CatalogLister: the ids pull accepts,
// GGUF (builtinModels) and MLX (mlxModels), sorted. Does not include the raw
// http(s):// URL fallback Download also accepts, since that is not a
// discoverable id.
func (racDownloader) DownloadCatalog() []string {
	ids := make([]string, 0, len(builtinModels)+len(mlxModels))
	for id := range builtinModels {
		ids = append(ids, id)
	}
	for id := range mlxModels {
		ids = append(ids, id)
	}
	sort.Strings(ids)
	return ids
}

func (racDownloader) Download(ctx context.Context, id string, onProgress func(DownloadProgress)) error {
	if err := initRAC(); err != nil {
		return fmt.Errorf("kit bootstrap: %w", err)
	}

	if repo, ok := mlxModels[id]; ok {
		return downloadMLX(ctx, id, repo, onProgress)
	}

	src, ok := builtinModels[id]
	if !ok {
		if strings.HasPrefix(id, "https://") || strings.HasPrefix(id, "http://") {
			src = modelSource{url: id, category: modelCategoryLanguage}
		} else {
			return fmt.Errorf(
				"unknown model id %q: not in the ported catalog (%d entries) and not a raw http(s):// URL",
				id, len(builtinModels),
			)
		}
	}

	modelID, modelInfoBytes, err := registerModelURL(src.url, src.category)
	if err != nil {
		return fmt.Errorf("register model: %w", err)
	}

	planBytes, totalBytes, err := planDownload(modelID, modelInfoBytes)
	if err != nil {
		return fmt.Errorf("plan download: %w", err)
	}

	taskID, err := startDownload(modelID, planBytes)
	if err != nil {
		return fmt.Errorf("start download: %w", err)
	}

	return pollUntilDone(ctx, modelID, taskID, totalBytes, onProgress)
}

// downloadMLX registers an MLX model as a multi-file artifact (its HF file
// list) and runs it through the same plan/start/poll flow as a GGUF pull. The
// kit's download worker already handles multi-file plans; only registration
// differs.
func downloadMLX(ctx context.Context, id, repo string, onProgress func(DownloadProgress)) error {
	files, err := fetchHFFiles(ctx, repo)
	if err != nil {
		return fmt.Errorf("list %s files: %w", repo, err)
	}

	modelID, modelInfoBytes, err := registerMultiFileModel(id, repo, files)
	if err != nil {
		return fmt.Errorf("register model: %w", err)
	}

	planBytes, totalBytes, err := planDownload(modelID, modelInfoBytes)
	if err != nil {
		return fmt.Errorf("plan download: %w", err)
	}

	taskID, err := startDownload(modelID, planBytes)
	if err != nil {
		return fmt.Errorf("start download: %w", err)
	}

	return pollUntilDone(ctx, modelID, taskID, totalBytes, onProgress)
}

// hfFile is one downloadable file in a HuggingFace repo.
type hfFile struct {
	name string
	url  string
	size int64
}

// fetchHFFiles lists a repo's files through the HF tree API, skipping the
// repo bookkeeping (.gitattributes) and docs (*.md) that inference does not
// need. size comes back in the same call so the download plan can report a
// real total.
func fetchHFFiles(ctx context.Context, repo string) ([]hfFile, error) {
	api := "https://huggingface.co/api/models/" + repo + "/tree/main"
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, api, nil)
	if err != nil {
		return nil, err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("HF API returned %s", resp.Status)
	}

	var entries []struct {
		Type string `json:"type"`
		Path string `json:"path"`
		Size int64  `json:"size"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&entries); err != nil {
		return nil, err
	}

	var files []hfFile
	for _, e := range entries {
		if e.Type != "file" || skipHFFile(e.Path) {
			continue
		}
		files = append(files, hfFile{
			name: e.Path,
			url:  "https://huggingface.co/" + repo + "/resolve/main/" + e.Path,
			size: e.Size,
		})
	}
	if len(files) == 0 {
		return nil, fmt.Errorf("no downloadable files")
	}
	return files, nil
}

func skipHFFile(name string) bool {
	if name == ".gitattributes" {
		return true
	}
	return strings.HasSuffix(strings.ToLower(name), ".md")
}

// registerMultiFileModel builds a RegisterMultiFileModelRequest (one
// ModelFileDescriptor per file) and persists it through
// rac_register_multi_file_model_proto, the multi-file counterpart to
// registerModelURL. The kit derives no id here (it is caller-supplied), so the
// registered id is the one passed in.
func registerMultiFileModel(id, name string, files []hfFile) (modelID string, modelInfoBytes []byte, err error) {
	req := appendStringField(nil, fieldMFID, id)
	req = appendStringField(req, fieldMFName, name)
	req = appendVarintField(req, fieldMFFramework, inferenceFrameworkMLX)
	for _, f := range files {
		fd := appendStringField(nil, fieldFDURL, f.url)
		fd = appendStringField(fd, fieldFDFilename, f.name)
		if f.size > 0 {
			fd = appendVarintField(fd, fieldFDSize, uint64(f.size))
		}
		req = appendLenField(req, fieldMFFiles, fd)
	}
	req = appendVarintField(req, fieldMFCategory, modelCategoryLanguage)
	req = appendVarintField(req, fieldMFFormat, modelFormatSafetensors)
	req = appendVarintField(req, fieldMFSource, modelSourceRemote)

	var out C.rac_proto_buffer_t
	C.rac_proto_buffer_init(&out)
	res := C.rac_register_multi_file_model_proto(bytesPtr(req), C.size_t(len(req)), &out)
	modelInfoBytes, err = decodeProtoBuffer(&out, res)
	if err != nil {
		return "", nil, err
	}
	modelID = fieldString(decodeProtoFields(modelInfoBytes), fieldModelInfoID)
	if modelID == "" {
		modelID = id
	}
	return modelID, modelInfoBytes, nil
}

// registerModelURL calls rac_register_model_from_url_proto, which infers
// format/artifact/id/name from the URL and saves the result to the kit's
// global model registry (rac_get_model_registry(), the same one
// rac_download_plan_proto and rac_server's loadModel read). Returns the
// kit-derived model id (not necessarily the caller's builtinModels key --
// the kit always derives id from the URL) and the raw serialized ModelInfo,
// which the caller round-trips into DownloadPlanRequest.model unparsed.
func registerModelURL(url string, category int32) (modelID string, modelInfoBytes []byte, err error) {
	req := appendStringField(nil, fieldRegisterURL, url)
	req = appendVarintField(req, fieldRegisterFramework, inferenceFrameworkLlamaCpp)
	req = appendVarintField(req, fieldRegisterCategory, uint64(category))

	var out C.rac_proto_buffer_t
	C.rac_proto_buffer_init(&out)
	res := C.rac_register_model_from_url_proto(bytesPtr(req), C.size_t(len(req)), &out)
	modelInfoBytes, err = decodeProtoBuffer(&out, res)
	if err != nil {
		return "", nil, err
	}
	modelID = fieldString(decodeProtoFields(modelInfoBytes), fieldModelInfoID)
	if modelID == "" {
		return "", nil, fmt.Errorf("kit returned no model id for %s", url)
	}
	return modelID, modelInfoBytes, nil
}

func planDownload(modelID string, modelInfoBytes []byte) (planBytes []byte, totalBytes int64, err error) {
	req := appendStringField(nil, fieldPlanReqModelID, modelID)
	req = appendLenField(req, fieldPlanReqModel, modelInfoBytes)

	var out C.rac_proto_buffer_t
	C.rac_proto_buffer_init(&out)
	res := C.rac_download_plan_proto(bytesPtr(req), C.size_t(len(req)), &out)
	planBytes, err = decodeProtoBuffer(&out, res)
	if err != nil {
		return nil, 0, err
	}
	fields := decodeProtoFields(planBytes)
	if fieldVarint(fields, fieldPlanCanStart) == 0 {
		return nil, 0, fmt.Errorf("plan rejected: %s", sdkErrorMessage(fields, fieldPlanError))
	}
	return planBytes, int64(fieldVarint(fields, fieldPlanTotalBytes)), nil
}

func startDownload(modelID string, planBytes []byte) (taskID string, err error) {
	req := appendStringField(nil, fieldStartReqModelID, modelID)
	req = appendLenField(req, fieldStartReqPlan, planBytes)

	var out C.rac_proto_buffer_t
	C.rac_proto_buffer_init(&out)
	res := C.rac_download_start_proto(bytesPtr(req), C.size_t(len(req)), &out)
	startBytes, err := decodeProtoBuffer(&out, res)
	if err != nil {
		return "", err
	}
	fields := decodeProtoFields(startBytes)
	if fieldVarint(fields, fieldStartAccepted) == 0 {
		return "", fmt.Errorf("rejected: %s", sdkErrorMessage(fields, fieldStartError))
	}
	return fieldString(fields, fieldStartTaskID), nil
}

// pollUntilDone polls rac_download_progress_poll_proto every 200ms -- the
// documented alternative to rac_download_set_progress_proto_callback, and
// the one that does not need a cgo-exported callback function or pinning a
// Go pointer for the C side to call back into. ctx cancellation sends
// rac_download_cancel_proto (partial bytes kept, matching wally-legacy's own
// SIGINT handling in cmd_pull.cpp) and returns ctx.Err().
func pollUntilDone(ctx context.Context, modelID, taskID string, totalBytes int64, onProgress func(DownloadProgress)) error {
	req := appendStringField(nil, fieldSubscribeModelID, modelID)
	req = appendStringField(req, fieldSubscribeTaskID, taskID)

	ticker := time.NewTicker(200 * time.Millisecond)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			cancelDownload(taskID, modelID)
			return ctx.Err()
		case <-ticker.C:
			var out C.rac_proto_buffer_t
			C.rac_proto_buffer_init(&out)
			res := C.rac_download_progress_poll_proto(bytesPtr(req), C.size_t(len(req)), &out)
			progBytes, err := decodeProtoBuffer(&out, res)
			if err != nil {
				return fmt.Errorf("poll progress: %w", err)
			}
			fields := decodeProtoFields(progBytes)

			downloaded := int64(fieldVarint(fields, fieldProgressBytesDownloaded))
			total := int64(fieldVarint(fields, fieldProgressTotalBytes))
			if total == 0 {
				total = totalBytes
			}
			if onProgress != nil {
				onProgress(DownloadProgress{Downloaded: downloaded, Total: total})
			}

			switch fieldVarint(fields, fieldProgressState) {
			case downloadStateCompleted:
				return nil
			case downloadStateFailed:
				return fmt.Errorf("download failed: %s", sdkErrorMessage(fields, fieldProgressError))
			case downloadStateCancelled:
				return fmt.Errorf("download cancelled")
			}
		}
	}
}

func cancelDownload(taskID, modelID string) {
	req := appendStringField(nil, fieldCancelReqTaskID, taskID)
	req = appendStringField(req, fieldCancelReqModelID, modelID)
	var out C.rac_proto_buffer_t
	C.rac_proto_buffer_init(&out)
	C.rac_download_cancel_proto(bytesPtr(req), C.size_t(len(req)), &out)
	C.rac_proto_buffer_free(&out)
}

// sdkErrorMessage decodes the embedded runanywhere.v1.SDKError at field num
// and returns its message (field 3), or a fallback when absent/unparsable.
func sdkErrorMessage(fields map[int]protoField, num int) string {
	b := fieldBytes(fields, num)
	if b == nil {
		return "unknown error"
	}
	if msg := fieldString(decodeProtoFields(b), fieldSDKErrorMessage); msg != "" {
		return msg
	}
	return "unknown error"
}

// --- minimal proto3 wire codec -------------------------------------------
//
// Encodes/decodes exactly what this file's messages use: varints
// (bool/int64/enum, wire type 0) and length-delimited values
// (string/bytes/embedded message, wire type 2). No groups, no fixed32/64,
// no repeated-field accumulation (last write wins, which is what every
// field this file reads needs).

type protoField struct {
	varint uint64
	bytes  []byte
	isLen  bool
}

func decodeProtoFields(data []byte) map[int]protoField {
	fields := make(map[int]protoField)
	i := 0
	for i < len(data) {
		tag, n := binary.Uvarint(data[i:])
		if n <= 0 {
			break
		}
		i += n
		num := int(tag >> 3)
		switch tag & 0x7 {
		case 0: // varint
			v, n := binary.Uvarint(data[i:])
			if n <= 0 {
				return fields
			}
			i += n
			fields[num] = protoField{varint: v}
		case 2: // length-delimited
			l, n := binary.Uvarint(data[i:])
			if n <= 0 || i+n > len(data) {
				return fields
			}
			i += n
			remaining := len(data) - i
			if l > uint64(remaining) {
				return fields
			}
			fields[num] = protoField{bytes: data[i : i+int(l)], isLen: true}
			i += int(l)
		case 1: // fixed64 -- unused by any field this file reads
			if i+8 > len(data) {
				return fields
			}
			i += 8
		case 5: // fixed32 -- unused by any field this file reads
			if i+4 > len(data) {
				return fields
			}
			i += 4
		default: // groups (3, 4): not used anywhere in these messages
			return fields
		}
	}
	return fields
}

func fieldString(fields map[int]protoField, num int) string {
	if f, ok := fields[num]; ok && f.isLen {
		return string(f.bytes)
	}
	return ""
}

func fieldBytes(fields map[int]protoField, num int) []byte {
	if f, ok := fields[num]; ok && f.isLen {
		return f.bytes
	}
	return nil
}

func fieldVarint(fields map[int]protoField, num int) uint64 {
	if f, ok := fields[num]; ok && !f.isLen {
		return f.varint
	}
	return 0
}

func appendVarint(buf []byte, v uint64) []byte {
	var tmp [binary.MaxVarintLen64]byte
	n := binary.PutUvarint(tmp[:], v)
	return append(buf, tmp[:n]...)
}

func appendVarintField(buf []byte, num int, v uint64) []byte {
	buf = appendVarint(buf, uint64(num)<<3)
	return appendVarint(buf, v)
}

func appendLenField(buf []byte, num int, data []byte) []byte {
	buf = appendVarint(buf, uint64(num)<<3|2)
	buf = appendVarint(buf, uint64(len(data)))
	return append(buf, data...)
}

func appendStringField(buf []byte, num int, s string) []byte {
	return appendLenField(buf, num, []byte(s))
}

// --- rac_proto_buffer_t <-> []byte bridge ---------------------------------

func bytesPtr(b []byte) *C.uint8_t {
	if len(b) == 0 {
		return nil
	}
	return (*C.uint8_t)(unsafe.Pointer(&b[0]))
}

// decodeProtoBuffer reads the result of a rac_*_proto call, freeing the
// buffer either way, matching rac_proto_buffer_free's "safe on every path"
// contract.
func decodeProtoBuffer(out *C.rac_proto_buffer_t, res C.rac_result_t) ([]byte, error) {
	defer C.rac_proto_buffer_free(out)
	if res != 0 {
		if out.error_message != nil {
			return nil, fmt.Errorf("error %d: %s", int(res), C.GoString(out.error_message))
		}
		return nil, fmt.Errorf("error %d", int(res))
	}
	if out.status != 0 {
		if out.error_message != nil {
			return nil, fmt.Errorf("error %d: %s", int(out.status), C.GoString(out.error_message))
		}
		return nil, fmt.Errorf("error %d", int(out.status))
	}
	if out.data == nil || out.size == 0 {
		return nil, nil
	}
	return C.GoBytes(unsafe.Pointer(out.data), C.int(out.size)), nil
}
