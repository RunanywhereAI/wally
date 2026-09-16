//go:build ondevice && darwin

package runanywhere

/*
#include <stdlib.h>
#include "rac/core/rac_core.h"
#include "rac/core/rac_model_lifecycle.h"
#include "rac/features/llm/rac_llm_service.h"
#include "rac/infrastructure/model_management/rac_model_registry.h"

// wally_mlx_start_server is a Swift @_cdecl entry (swift/Sources/wally-app):
// discover + load the MLX model, then serve it over a local OpenAI-compatible
// HTTP server, in-process. It blocks in its accept loop, so the Go caller runs
// it on a dedicated goroutine.
extern int wally_mlx_start_server(const char* model_id, unsigned short port);

// The Swift MLX serve resolves the proto ABI through dlsym (CppBridge), which
// the linker's dead-code pass cannot see, so a release build would strip these
// out of librac_commons. Taking their addresses keeps them, the same trick the
// SDK's own PlatformAdapter uses for a different symbol.
static void *const wally_mlx_keepalive[] = {
    (void *)rac_model_registry_discover_proto,
    (void *)rac_model_lifecycle_load_proto,
    (void *)rac_llm_generate_proto,
    (void *)rac_proto_buffer_free,
};
const void *wally_mlx_keepalive_anchor(void) { return (const void *)wally_mlx_keepalive; }
*/
import "C"

import (
	"fmt"
	"time"
)

func init() { _ = C.wally_mlx_keepalive_anchor() }

// startMLX serves an MLX model in-process. The @main Swift host already ran
// MLX.register() (callbacks installed), and Go's initRAC did rac_init, so this
// hands the model id to wally_mlx_start_server on a dedicated goroutine and
// proxies the daemon to its local port. No subprocess: MLX, GGUF, and cloud all
// live in this one binary.
func (e *racServerEngine) startMLX(m InstalledModel) (Endpoint, error) {
	if err := initRAC(); err != nil {
		return Endpoint{}, fmt.Errorf("kit bootstrap: %w", err)
	}

	e.mu.Lock()
	defer e.mu.Unlock()

	if e.mlxPort != 0 {
		if e.mlxModelID == m.ID {
			return e.mlxEndpoint(), nil
		}
		// The in-process server loads one model for its lifetime and cannot be
		// stopped (a blocking accept loop). A second MLX model in the same run
		// is not supported; restart wally to switch.
		return Endpoint{}, fmt.Errorf("this build serves one MLX model per run; %s is already loaded", e.mlxModelID)
	}

	port, err := freePort()
	if err != nil {
		return Endpoint{}, fmt.Errorf("pick mlx port: %w", err)
	}

	// Held by the C side for the whole process lifetime (the accept loop never
	// returns), so it is deliberately not freed.
	cModelID := C.CString(m.ID)
	go func() {
		C.wally_mlx_start_server(cModelID, C.ushort(port))
	}()

	if err := waitMLXReady(port, 180*time.Second); err != nil {
		return Endpoint{}, fmt.Errorf("in-process MLX server did not become ready for %s: %w", m.ID, err)
	}

	e.mlxModelID = m.ID
	e.mlxPort = port
	return e.mlxEndpoint(), nil
}
