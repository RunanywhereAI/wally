#ifndef WALLY_GO_H
#define WALLY_GO_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// The Go CLI, compiled as a c-archive (mainexport_apple.go). The Swift @main
// host registers MLX in-process, then calls this — one application, the whole
// command surface, exactly as the OG wally handed off to the C++ wally_run_main.
int wally_run_main(int argc, char **argv);

// Answers MLXRuntime's "is the Metal shader library present beside me?" check
// by locating mlx-swift_Cmlx.bundle next to the real executable (resolving the
// Homebrew bin/->libexec symlink). Without it MLX reports itself unavailable on
// every installed copy while working from the build tree.
int32_t ra_mlx_metal_resource_anchor(void);

#ifdef __cplusplus
}
#endif

#endif /* WALLY_GO_H */
