import Foundation
import RunAnywhere

// wally_mlx_start_server is called by the Go engine (runanywhere/engine_mlx_darwin.go)
// for an MLX model, in-process on a dedicated goroutine. rac_init and
// MLX.register() have already run (Go's initRAC and the @main host in
// WallyApp.swift), so this only discovers the installed models, loads the one
// asked for, and serves it. It blocks in the accept loop; the Go side polls
// /health for readiness.
@_cdecl("wally_mlx_start_server")
public func wally_mlx_start_server(_ modelIDPtr: UnsafePointer<CChar>, _ port: UInt16) -> Int32 {
    let modelID = String(cString: modelIDPtr)

    let loaded: Bool = (try? runBlocking { () async -> Bool in
        let discovery = await CppBridge.ModelRegistry.shared.discoverDownloadedModels()
        if discovery.hasError {
            FileHandle.standardError.write(Data("wally: mlx discovery: \(discovery.error.message)\n".utf8))
        }
        guard await CppBridge.ModelRegistry.shared.get(modelId: modelID) != nil else {
            FileHandle.standardError.write(Data("wally: mlx model '\(modelID)' not found by discovery\n".utf8))
            return false
        }
        var req = RAModelLoadRequest()
        req.modelID = modelID
        req.framework = .mlx
        req.validateAvailability = false
        let res = await CppBridge.ModelLifecycle.load(req)
        if res.hasError {
            FileHandle.standardError.write(Data("wally: mlx load failed: \(res.error.message)\n".utf8))
            return false
        }
        return true
    }) ?? false

    guard loaded else { return 1 }

    do {
        try runServer(port: port, modelID: modelID)
    } catch {
        FileHandle.standardError.write(Data("wally: mlx server failed: \(error)\n".utf8))
        return 1
    }
    return 0
}
