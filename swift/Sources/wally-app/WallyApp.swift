// The single Apple wally binary: register the MLX runtime in-process, then run
// the Go CLI (wally_run_main). This is the OG shape — one executable serving
// MLX, GGUF, and cloud — with the Go code standing in for the OG's C++
// wally_run_main. MLX Metal shaders load from mlx-swift_Cmlx.bundle beside the
// executable (see ra_mlx_metal_resource_anchor in CWallyGo).

import CWallyGo
import Darwin
import Foundation
import MLXRuntime

@main
struct WallyApp {
    static func main() {
        var arguments = CommandLine.arguments.map { strdup($0) }
        defer { arguments.forEach { free($0) } }

        // Install the MLX callbacks into commons before the Go CLI runs its own
        // rac_init (in runanywhere/engine_ondevice.go). MLX.register only sets a
        // callback table, so it is safe ahead of rac_init, matching the OG host.
        if !MLX.register() {
            FileHandle.standardError.write(
                Data("wally: MLX unavailable in this build; GGUF and cloud still work\n".utf8))
        }

        let status = arguments.withUnsafeMutableBufferPointer { buffer in
            wally_run_main(Int32(buffer.count), buffer.baseAddress)
        }
        Darwin.exit(status)
    }
}
