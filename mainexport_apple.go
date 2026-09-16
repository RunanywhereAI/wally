//go:build ondevice && darwin

package main

// The single-binary Apple build compiles the Go CLI as a c-archive and lets a
// Swift @main host own the final link, exactly as the OG wally did with the
// C++ wally_run_main: the host registers the MLX runtime in-process, then calls
// this. That is what keeps MLX, GGUF, and cloud in one executable instead of a
// separate wally-mlx helper.

// #include <stdlib.h>
import "C"

import (
	"os"
	"unsafe"

	"github.com/RunanywhereAI/wally/cmd"
)

//export wally_run_main
func wally_run_main(argc C.int, argv **C.char) C.int {
	if argc > 0 && argv != nil {
		in := unsafe.Slice(argv, int(argc))
		args := make([]string, 0, int(argc))
		for _, a := range in {
			args = append(args, C.GoString(a))
		}
		os.Args = args
	}
	// Execute runs the cobra root and os.Exit(1) on failure, so a return here
	// is the success path.
	cmd.Execute()
	return 0
}
