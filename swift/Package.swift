// swift-tools-version: 6.0
import PackageDescription
import Foundation

// wally-mlx: a standalone Swift helper that serves ONE installed MLX model
// over a minimal OpenAI-compatible HTTP server. Spawned by the Go daemon
// (runanywhere/engine_ondevice.go, ondevice tag) for a model whose framework
// is MLX; GGUF models never reach this binary, they stay on the in-process
// cgo rac_server path.
//
// rac_server (the C++ desktop kit's HTTP server) is GGUF-only regardless of
// which backends are registered -- confirmed against wally-legacy/.deps/kit
// (zero MLX/safetensors awareness in librac_server.a) and against
// wally-legacy/src/harness/harness.cpp's own Resolve(), which refuses any
// non-LlamaCpp local model for exactly that reason. MLX only ever runs
// through the registry-aware lifecycle/generate ABI (rac_model_lifecycle_load_proto
// + rac_llm_generate_proto), so this binary implements that path itself with
// a hand-rolled server instead of calling rac_server_start.
//
// This links the PUBLISHED runanywhere-swift SDK (RACommonsBinary.xcframework
// + RABackendMLXBinary.xcframework, both remote, checksum-verified), not the
// packaged C++ desktop kit: RunAnywhere + RunAnywhereMLX both depend on the
// SAME RACommonsBinary artifact, so there is exactly one commons instance in
// this process. Deliberately NOT linking wally-legacy/.deps/kit's static
// archives too -- that would be a second, independent copy of commons in the
// same binary (the exact problem wally-legacy's build-mlx.sh works around
// with WALLY_SDK_SWIFT_PATH for its CMake+SwiftPM hybrid link, which does not
// apply here since this binary is pure SwiftPM).
//
// The kit's headers ARE reused, header-only, for the one piece the SDK does
// not expose publicly: the desktop platform-adapter bootstrap
// (rac_platform_adapter_t / rac_config_t / rac_init / rac_logger_* /
// rac_model_paths_set_base_dir). CppBridge.PlatformAdapter.register() does
// this too, but internally (not `public`) and via its OWN heavier
// CppBridge.initialize(), which this binary must not call: that path also
// wires telemetry + device registration (real HTTP), and this helper has to
// make zero network calls to serve a local model. The struct/prototype
// declarations are ABI-guarded (rac_platform_adapter_t.abi_version /
// struct_size) by commons itself, so a mismatch between this header copy and
// the linked RACommonsBinary build fails init cleanly rather than silently.
let kitInclude: String = {
    if let path = Context.environment["WALLY_SDK_KIT"], !path.isEmpty {
        return path + "/include"
    }
    return "/Users/home/Project/wally-work-sapce/wally-legacy/.deps/kit/include"
}()

let package = Package(
    name: "wally-mlx",
    platforms: [.macOS("14.5")],
    dependencies: [
        .package(url: "https://github.com/RunanywhereAI/runanywhere-swift.git", exact: "0.20.25")
    ],
    targets: [
        .target(
            name: "CWallyRACBootstrap",
            cSettings: [
                .unsafeFlags(["-I", kitInclude])
            ]
        ),
        .executableTarget(
            name: "wally-mlx",
            dependencies: [
                "CWallyRACBootstrap",
                .product(name: "RunAnywhere", package: "runanywhere-swift"),
                .product(name: "RunAnywhereMLX", package: "runanywhere-swift"),
            ],
        ),
    ]
)
