//go:build ondevice

// The ondevice build links the packaged C++ kit's rac_server (an
// OpenAI-compatible HTTP server serving GGUF/llama.cpp models over
// /v1/chat/completions) via cgo and installs it as the Engine. This file
// carries no path to the kit: CGO_CFLAGS and CGO_LDFLAGS must already name
// the kit's include and lib directories in the environment before `go
// build -tags ondevice` runs. scripts/build-ondevice.sh derives both from
// the kit's own lib/cmake/RunAnywhere/RunAnywhereConfig.cmake, keyed off
// WALLY_SDK_KIT, so the link recipe tracks the kit instead of drifting from
// a hand-copied one. A plain `go build ./...` (no tag) never compiles this
// file, so CGO_ENABLED=0 and untagged CGO_ENABLED=1 builds are unaffected.
package runanywhere

/*
#include <stdlib.h>
#include "rac/core/rac_core.h"
#include "rac/core/rac_logger.h"
#include "rac/desktop/rac_desktop.h"
#include "rac/backends/rac_llm_llamacpp.h"
#include "rac/infrastructure/model_management/rac_model_paths.h"
#include "rac/server/rac_server.h"
*/
import "C"

import (
	"fmt"
	"math"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
	"unsafe"
)

func init() {
	SetEngine(&racServerEngine{})
	SetDownloader(&racDownloader{})
}

// racAdapter is borrowed by rac_init, not copied, so it has to outlive every
// call into the kit; a stack or heap value freed after initRAC returns would
// leave rac_init holding a dangling pointer. It is C memory, not a Go
// value, and deliberately never freed: cgo's pointer checker rejects a Go
// pointer that C retains past the call (a Go pointer nested inside another
// Go pointer, here rac_config_t.platform_adapter reachable from &cfg,
// panics at the call site with "argument of cgo function has Go pointer to
// unpinned Go pointer" the first time rac_init actually runs), and the
// adapter has to survive past this call by contract, so it cannot be pinned
// for the duration and released either.
var racAdapter *C.rac_platform_adapter_t

var (
	racInitOnce sync.Once
	racInitErr  error
)

// initRAC runs the kit's desktop bootstrap once, before the first model
// load or download: a desktop platform adapter (file I/O, clock, secure
// storage), rac_init itself, the llamacpp plugin's own registration call,
// the model store base directory, and the desktop libcurl HTTP transport
// the download orchestrator calls through.
//
// Skipping that last call is the actual cause of a -204
// (RAC_ERROR_SERVER_MODEL_LOAD_FAILED) on every .gguf, measured directly:
// rac_backend_llamacpp.a's static constructors link the object code in (so
// even a bare rac_server_start finds a plugin named "llamacpp"), but they do
// not call rac_backend_llamacpp_register() themselves. wally-legacy's
// bootstrap.cpp calls it explicitly (src/bootstrap.cpp:636), gated only on
// WALLY_HAS_LLAMACPP, and nothing else does. Verified with a standalone cgo
// build linked against the same kit: rac_server_start returns
// RAC_ERROR_SERVER_MODEL_LOAD_FAILED without this call and RAC_SUCCESS with
// it, both with an identical rac_init. The multi-phase rac_sdk_init /
// rac_sdk_init_phase1_proto / phase2_proto sequence bootstrap.cpp also runs
// is unrelated: that path gates telemetry and device registration against
// the control plane, not backend capability, and would add a network
// dependency an offline on-device engine has no reason to need. Ruled out by
// testing rac_server_start with and without it; the result did not change.
func initRAC() error {
	racInitOnce.Do(func() {
		racAdapter = (*C.rac_platform_adapter_t)(C.malloc(C.sizeof_rac_platform_adapter_t))
		if res := C.rac_desktop_adapter_init(nil, racAdapter); res != 0 {
			racInitErr = fmt.Errorf("rac_desktop_adapter_init: error %d", int(res))
			return
		}

		// Configure the logger BEFORE rac_init, same order and same reason
		// as wally-legacy's bootstrap.cpp (src/bootstrap.cpp:595-605): two
		// distinct knobs, not one. stderr_always defaults to true and mirrors
		// every log line straight to stderr *in addition to* the adapter's
		// own logging slot -- that duplication is why the kit's own
		// [RAC][INFO]/[RAC][WARN] lines showed up twice (once as "[RAC][INFO]
		// ...", once as "[INFO] ..."). min_level is the separate filter that
		// keeps INFO/WARN out entirely; rac_config_t.log_level below is a
		// third, unrelated gate on top of both. wally has no --verbose flag
		// yet, so this is unconditionally quiet, matching bootstrap.cpp's own
		// non-verbose default (RAC_LOG_ERROR).
		C.rac_logger_set_stderr_always(0)
		C.rac_logger_set_min_level(C.RAC_LOG_ERROR)

		// cfg itself is a transient argument (rac_init reads it synchronously
		// and does not keep &cfg), but its two pointer fields are not: the
		// platform_adapter field above, and log_tag, which rac_init keeps
		// logging against for the rest of the process. Never freed.
		cfg := C.rac_config_t{
			platform_adapter: racAdapter,
			log_level:        C.RAC_LOG_ERROR,
			log_tag:          C.CString("wally"),
		}
		if res := C.rac_init(&cfg); res != 0 {
			racInitErr = fmt.Errorf("rac_init: error %d", int(res))
			return
		}

		if res := C.rac_backend_llamacpp_register(); res != 0 {
			racInitErr = fmt.Errorf("rac_backend_llamacpp_register: error %d", int(res))
			return
		}

		// The kit lays a download out at <base>/Models/<framework>/<id>; it
		// appends "Models" itself (see rac_desktop.h's own doc comment on
		// rac_desktop_default_base_dir), so base is ModelsRoot()'s parent,
		// not ModelsRoot() itself. Computed from the same InstalledModels()
		// already uses, so a download lands exactly where the scanner that
		// finds installed models looks.
		cBaseDir := C.CString(filepath.Dir(ModelsRoot()))
		defer C.free(unsafe.Pointer(cBaseDir))
		if res := C.rac_model_paths_set_base_dir(cBaseDir); res != 0 {
			racInitErr = fmt.Errorf("rac_model_paths_set_base_dir: error %d", int(res))
			return
		}

		if res := C.rac_desktop_http_transport_register(); res != 0 {
			racInitErr = fmt.Errorf("rac_desktop_http_transport_register: error %d", int(res))
			return
		}
	})
	return racInitErr
}

// racServerEngine wraps rac_server, which is a process-wide singleton in the
// kit: one model loaded at a time, one background HTTP thread. Start stops
// whatever is running before loading a different model.
type racServerEngine struct {
	mu      sync.Mutex
	modelID string
	port    int

	// MLX runs out of process: rac_server is GGUF-only, so an MLX model is
	// served by the wally-mlx Swift helper, spawned here and proxied to.
	mlxCmd     *exec.Cmd
	mlxModelID string
	mlxPort    int
}

func (e *racServerEngine) Start(m InstalledModel) (Endpoint, error) {
	if ext := strings.ToLower(filepath.Ext(m.Path)); ext != ".gguf" {
		if strings.EqualFold(m.Framework, "MLX") {
			return e.startMLX(m)
		}
		return Endpoint{}, fmt.Errorf(
			"on-device inference for %s models is not supported yet; only GGUF and MLX",
			m.Framework,
		)
	}

	if err := initRAC(); err != nil {
		return Endpoint{}, fmt.Errorf("kit bootstrap: %w", err)
	}

	e.mu.Lock()
	defer e.mu.Unlock()

	if racServerRunning() {
		if e.modelID == m.ID {
			return e.endpoint(), nil
		}
		if res := C.rac_server_stop(); res != 0 {
			return Endpoint{}, fmt.Errorf("rac_server_stop: error %d", int(res))
		}
	}

	port, err := freePort()
	if err != nil {
		return Endpoint{}, fmt.Errorf("pick on-device port: %w", err)
	}

	cHost := C.CString("127.0.0.1")
	defer C.free(unsafe.Pointer(cHost))
	cPath := C.CString(m.Path)
	defer C.free(unsafe.Pointer(cPath))
	cID := C.CString(m.ID)
	defer C.free(unsafe.Pointer(cID))
	cCors := C.CString("*")
	defer C.free(unsafe.Pointer(cCors))

	cfg := C.rac_server_config_t{
		host:                    cHost,
		port:                    C.uint16_t(port),
		model_path:              cPath,
		model_id:                cID,
		context_size:            8192,
		threads:                 0,                        // 0 = auto, per rac_llm_llamacpp.h
		gpu_layers:              C.int32_t(math.MinInt32), // RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO
		enable_cors:             1,
		cors_origins:            cCors,
		request_timeout_seconds: 300,
		max_concurrent_requests: 4,
		verbose:                 0,
	}

	if res := C.rac_server_start(&cfg); res != 0 {
		return Endpoint{}, fmt.Errorf("rac_server_start(%s): error %d", m.ID, int(res))
	}

	e.modelID = m.ID
	e.port = port
	return e.endpoint(), nil
}

func (e *racServerEngine) Stop() error {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.stopMLXLocked()
	if !racServerRunning() {
		return nil
	}
	if res := C.rac_server_stop(); res != 0 {
		return fmt.Errorf("rac_server_stop: error %d", int(res))
	}
	e.modelID = ""
	return nil
}

// startMLX spawns the wally-mlx helper for an MLX model and returns its local
// endpoint once it is serving. rac_server cannot load MLX; the helper does the
// direct-generate path in Swift with the MLX runtime.
func (e *racServerEngine) startMLX(m InstalledModel) (Endpoint, error) {
	e.mu.Lock()
	defer e.mu.Unlock()

	if e.mlxCmd != nil {
		if e.mlxModelID == m.ID {
			return e.mlxEndpoint(), nil
		}
		e.stopMLXLocked()
	}

	helper := mlxHelperPath()
	if helper == "" {
		return Endpoint{}, fmt.Errorf("the wally-mlx helper is not installed next to wally; MLX inference is unavailable")
	}
	port, err := freePort()
	if err != nil {
		return Endpoint{}, fmt.Errorf("pick mlx port: %w", err)
	}

	cmd := exec.Command(helper, m.ID, strconv.Itoa(port))
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		return Endpoint{}, fmt.Errorf("start wally-mlx: %w", err)
	}
	if err := waitMLXReady(port, 180*time.Second); err != nil {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
		return Endpoint{}, fmt.Errorf("wally-mlx did not become ready for %s: %w", m.ID, err)
	}

	e.mlxCmd = cmd
	e.mlxModelID = m.ID
	e.mlxPort = port
	return e.mlxEndpoint(), nil
}

func (e *racServerEngine) stopMLXLocked() {
	if e.mlxCmd != nil && e.mlxCmd.Process != nil {
		_ = e.mlxCmd.Process.Kill()
		_ = e.mlxCmd.Wait()
	}
	e.mlxCmd = nil
	e.mlxModelID = ""
}

func (e *racServerEngine) mlxEndpoint() Endpoint {
	return Endpoint{
		BaseURL: "http://127.0.0.1:" + strconv.Itoa(e.mlxPort) + "/v1",
		APIKey:  "",
		Local:   true,
	}
}

// mlxHelperPath finds the wally-mlx binary: WALLY_MLX_HELPER, then beside the
// wally executable (where the installer puts it with its Metal bundles), then a
// dev build/ fallback. Empty when none exists.
func mlxHelperPath() string {
	if p := os.Getenv("WALLY_MLX_HELPER"); p != "" {
		return p
	}
	if exe, err := os.Executable(); err == nil {
		cand := filepath.Join(filepath.Dir(exe), "wally-mlx")
		if fi, err := os.Stat(cand); err == nil && !fi.IsDir() {
			return cand
		}
	}
	if fi, err := os.Stat("build/wally-mlx"); err == nil && !fi.IsDir() {
		return "build/wally-mlx"
	}
	return ""
}

func waitMLXReady(port int, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	url := "http://127.0.0.1:" + strconv.Itoa(port) + "/health"
	client := http.Client{Timeout: 2 * time.Second}
	for time.Now().Before(deadline) {
		resp, err := client.Get(url)
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode == http.StatusOK {
				return nil
			}
		}
		time.Sleep(300 * time.Millisecond)
	}
	return fmt.Errorf("timeout after %s", timeout)
}

func (e *racServerEngine) endpoint() Endpoint {
	return Endpoint{
		BaseURL: "http://127.0.0.1:" + strconv.Itoa(e.port) + "/v1",
		APIKey:  "",
		Local:   true,
	}
}

func racServerRunning() bool {
	return C.rac_server_is_running() != 0
}

// freePort asks the kernel for an unused loopback port and releases it
// immediately; rac_server binds it a moment later. This is never the
// daemon's own port (127.0.0.1:11500): the kernel does not hand out a port
// that is already bound.
func freePort() (int, error) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer ln.Close()
	return ln.Addr().(*net.TCPAddr).Port, nil
}
