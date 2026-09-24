// swift-tools-version: 6.0
import PackageDescription
import Foundation

// The Apple build of wally.
//
// MLX inference is written in Swift; the MLX engine inside commons is only a
// table of callbacks. A pure C++ binary therefore cannot run MLX. This package
// registers the callbacks and then calls `wally_run_main`, the same entry the
// CMake `wally-cxx` binary uses.
//
// SwiftPM owns the final link so MLX Metal shader bundles land beside the
// executable. The C++ objects arrive as linker flags from
// `scripts/build/build-mlx.sh` (merged archive + system libs), not as a path listed
// here — SwiftPM caches the manifest and would not notice a rebuilt archive.
//
// The Swift MLX runtime is a product of the SDK. Independent clones pin the
// published Swift distribution. In this monorepo, set WALLY_SDK_SWIFT_PATH to
// the SDK root (the directory with Package.swift).

let mlxPackage: Package.Dependency = {
    if let local = Context.environment["WALLY_SDK_SWIFT_PATH"], !local.isEmpty {
        return .package(path: local)
    }
    return .package(url: "https://github.com/RunanywhereAI/runanywhere-swift.git", exact: "0.20.37")
}()

let mlxPackageName: String = {
    if let local = Context.environment["WALLY_SDK_SWIFT_PATH"], !local.isEmpty {
        // SwiftPM identity for a path dependency is the directory name, not
        // Package.swift's `name:`. Canonicalize so `/foo/bar/../..` is not `..`.
        return URL(fileURLWithPath: local).standardizedFileURL.lastPathComponent
    }
    return "runanywhere-swift"
}()
let mlxProductName = Context.environment["WALLY_SDK_SWIFT_PATH"].map { _ in "RunAnywhereMLXRuntime" }
    ?? "RunAnywhereMLX"

let package = Package(
    name: "wally-mlx",
    platforms: [.macOS("14.5")],
    dependencies: [mlxPackage],
    targets: [
        .target(name: "CWallyApp"),
        .executableTarget(
            name: "WallyMLX",
            dependencies: [
                "CWallyApp",
                .product(name: mlxProductName, package: mlxPackageName),
            ]
        ),
    ]
)
