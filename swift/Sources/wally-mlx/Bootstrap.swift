// wally-mlx: serves ONE installed MLX model over a minimal OpenAI-compatible
// HTTP server (GET /health, POST /v1/chat/completions). Spawned by
// runanywhere/engine_ondevice.go (ondevice tag) for a model whose framework
// is MLX. See swift/Package.swift for why this exists instead of a call to
// rac_server_start.
//
// Usage: wally-mlx <model-id> <port>
//
// Deliberately makes zero outbound network calls: no rac_desktop_http_transport
// is ever registered, and CppBridge.initialize() (the SDK's own bootstrap,
// which wires telemetry + device registration over HTTP) is never called.
// Bootstrap instead mirrors the narrow C-ABI sequence
// runanywhere/engine_ondevice.go's initRAC() uses, directly.

import CWallyRACBootstrap
import Darwin
import Foundation
import MLXRuntime
import RunAnywhere

// The SDK's own "generated proto ABI" surface (CppBridge.ModelRegistry,
// CppBridge.ModelLifecycle, CppBridge.LLM, ...) resolves every rac_*_proto
// entry point through dlsym(RTLD_DEFAULT, "rac_..."), never a direct symbol
// reference (Foundation/Bridge/Extensions/CppBridge+NativeProtoABI.swift).
// That is invisible to the static linker's dead-code analysis: a release
// build stripped rac_model_lifecycle_load_proto (confirmed live -- discovery
// worked, then load failed with "Native proto ABI is not exported by the
// linked RACommons binary") while rac_model_registry_discover_proto happened
// to survive because something else referenced it. A real Swift-level
// reference, even one that is never called, is enough to keep a symbol.
// CppBridge+PlatformAdapter.swift's own `_ = rac_device_manager_is_registered()`
// "Force link device manager symbols" comment is the SDK hitting this exact
// problem for a different symbol.
nonisolated(unsafe) private let protoABIKeepAlive: [Any] = [
    rac_model_registry_discover_proto,
    rac_model_lifecycle_load_proto,
    rac_llm_generate_proto,
    rac_proto_buffer_free,
]

// MARK: - Platform adapter (mirrors initRAC() in engine_ondevice.go)
//
// CppBridge.PlatformAdapter.register() (Sources/RunAnywhere/Foundation/Bridge/
// Extensions/CppBridge+PlatformAdapter.swift in the runanywhere-swift SDK)
// does the same job but is not `public`, and it is reached only through the
// heavier CppBridge.initialize() this binary must not call. rac_init rejects
// a struct-layout mismatch with RAC_ERROR_ABI_VERSION_MISMATCH rather than
// corrupting memory (rac_platform_adapter_t.abi_version/struct_size), so a
// header drift between this copy and the linked RACommonsBinary fails
// cleanly and visibly instead of silently.

// nonisolated(unsafe): plain C struct state, populated once during
// single-threaded bootstrap before any concurrent access is possible, then
// only read (as an immutable pointer) by commons on whichever thread it
// chooses to call the adapter callbacks from -- there is no Swift-side
// mutation after bootstrapRAC() returns.
nonisolated(unsafe) private var gAdapter = rac_platform_adapter_t()

private func waFileExists(_ path: UnsafePointer<CChar>?, _ userData: UnsafeMutableRawPointer?) -> rac_bool_t {
    guard let path else { return RAC_FALSE }
    return FileManager.default.fileExists(atPath: String(cString: path)) ? RAC_TRUE : RAC_FALSE
}

private func waFileRead(
    _ path: UnsafePointer<CChar>?,
    _ outData: UnsafeMutablePointer<UnsafeMutableRawPointer?>?,
    _ outSize: UnsafeMutablePointer<Int>?,
    _ userData: UnsafeMutableRawPointer?
) -> rac_result_t {
    guard let path, let outData, let outSize else { return RAC_ERROR_INVALID_ARGUMENT }
    guard let data = FileManager.default.contents(atPath: String(cString: path)) else {
        return RAC_ERROR_FILE_NOT_FOUND
    }
    let buffer = malloc(data.count)
    data.withUnsafeBytes { raw in
        if let base = raw.baseAddress, data.count > 0 {
            memcpy(buffer, base, data.count)
        }
    }
    outData.pointee = buffer
    outSize.pointee = data.count
    return RAC_SUCCESS
}

private func waFileWrite(
    _ path: UnsafePointer<CChar>?,
    _ data: UnsafeRawPointer?,
    _ size: Int,
    _ userData: UnsafeMutableRawPointer?
) -> rac_result_t {
    guard let path else { return RAC_ERROR_INVALID_ARGUMENT }
    let d = Data(bytes: data ?? UnsafeRawPointer(bitPattern: 1)!, count: data == nil ? 0 : size)
    do {
        try d.write(to: URL(fileURLWithPath: String(cString: path)))
        return RAC_SUCCESS
    } catch {
        return RAC_ERROR_INVALID_ARGUMENT
    }
}

private func waFileDelete(_ path: UnsafePointer<CChar>?, _ userData: UnsafeMutableRawPointer?) -> rac_result_t {
    guard let path else { return RAC_ERROR_INVALID_ARGUMENT }
    do {
        try FileManager.default.removeItem(atPath: String(cString: path))
        return RAC_SUCCESS
    } catch {
        return RAC_ERROR_FILE_NOT_FOUND
    }
}

// No real secure storage: this helper only reads already-installed local
// models, offline. Every call reports a clean miss/no-op, matching the
// not-found contract documented on rac_platform_adapter_t.secure_get.
private func waSecureGet(_ key: UnsafePointer<CChar>?, _ outValue: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?, _ userData: UnsafeMutableRawPointer?) -> rac_result_t {
    RAC_ERROR_FILE_NOT_FOUND
}
private func waSecureSet(_ key: UnsafePointer<CChar>?, _ value: UnsafePointer<CChar>?, _ userData: UnsafeMutableRawPointer?) -> rac_result_t {
    RAC_SUCCESS
}
private func waSecureDelete(_ key: UnsafePointer<CChar>?, _ userData: UnsafeMutableRawPointer?) -> rac_result_t {
    RAC_SUCCESS
}

private func waLog(_ level: rac_log_level_t, _ category: UnsafePointer<CChar>?, _ message: UnsafePointer<CChar>?, _ userData: UnsafeMutableRawPointer?) {
    let cat = category.map { String(cString: $0) } ?? ""
    let msg = message.map { String(cString: $0) } ?? ""
    FileHandle.standardError.write(Data("[wally-mlx][\(cat)] \(msg)\n".utf8))
}

private func waNowMs(_ userData: UnsafeMutableRawPointer?) -> Int64 {
    Int64(Date().timeIntervalSince1970 * 1000)
}

private func waGetMemoryInfo(_ outInfo: UnsafeMutablePointer<rac_memory_info_t>?, _ userData: UnsafeMutableRawPointer?) -> rac_result_t {
    guard let outInfo else { return RAC_ERROR_INVALID_ARGUMENT }
    let total = ProcessInfo.processInfo.physicalMemory
    outInfo.pointee = rac_memory_info_t(total_bytes: UInt64(total), available_bytes: UInt64(total / 2), used_bytes: UInt64(total / 2))
    return RAC_SUCCESS
}

// commons' model-registry folder-manifest restore (rac_model_registry_discover_proto
// -> restore_models_from_folder_manifests -> list_directory_via_adapter) calls
// this slot WITHOUT a null check despite the header documenting it as
// optional -- confirmed by crashing (EXC_BAD_ACCESS through a null function
// pointer) with this slot left nil and reading the live backtrace under
// lldb. Two-call semantics: out_entries == nil queries capacity.
private func waFileListDirectory(
    _ dirPath: UnsafePointer<CChar>?,
    _ outEntries: UnsafeMutablePointer<rac_directory_entry_t>?,
    _ inOutCount: UnsafeMutablePointer<Int>?,
    _ userData: UnsafeMutableRawPointer?
) -> rac_result_t {
    guard let dirPath, let inOutCount else { return RAC_ERROR_INVALID_ARGUMENT }
    let path = String(cString: dirPath)
    let fm = FileManager.default
    var isDir: ObjCBool = false
    guard fm.fileExists(atPath: path, isDirectory: &isDir), isDir.boolValue else {
        return RAC_ERROR_FILE_NOT_FOUND
    }
    guard let names = try? fm.contentsOfDirectory(atPath: path) else {
        return RAC_ERROR_FILE_NOT_FOUND
    }
    let visible = names.filter { !$0.hasPrefix(".") }

    guard let outEntries else {
        inOutCount.pointee = visible.count
        return RAC_SUCCESS
    }

    let capacity = inOutCount.pointee
    var written = 0
    for name in visible {
        if written >= capacity { break }
        let nameBytes = Array(name.utf8)
        guard nameBytes.count < 512 else { continue } // RAC_DIRECTORY_ENTRY_NAME_MAX
        var entry = rac_directory_entry_t()
        withUnsafeMutableBytes(of: &entry.name) { raw in
            let buf = raw.bindMemory(to: CChar.self)
            for (i, b) in nameBytes.enumerated() { buf[i] = CChar(bitPattern: b) }
            buf[nameBytes.count] = 0
        }
        let fullPath = path + "/" + name
        var childIsDir: ObjCBool = false
        fm.fileExists(atPath: fullPath, isDirectory: &childIsDir)
        entry.is_dir = childIsDir.boolValue ? RAC_TRUE : RAC_FALSE
        if !childIsDir.boolValue {
            let attrs = try? fm.attributesOfItem(atPath: fullPath)
            entry.size_bytes = Int64((attrs?[.size] as? UInt64) ?? 0)
        } else {
            entry.size_bytes = 0
        }
        outEntries[written] = entry
        written += 1
    }
    inOutCount.pointee = written
    return RAC_SUCCESS
}

private func waIsNonEmptyDirectory(_ path: UnsafePointer<CChar>?, _ userData: UnsafeMutableRawPointer?) -> rac_bool_t {
    guard let path else { return RAC_FALSE }
    let s = String(cString: path)
    var isDir: ObjCBool = false
    guard FileManager.default.fileExists(atPath: s, isDirectory: &isDir), isDir.boolValue else { return RAC_FALSE }
    let entries = (try? FileManager.default.contentsOfDirectory(atPath: s)) ?? []
    return entries.isEmpty ? RAC_FALSE : RAC_TRUE
}

/// Populates gAdapter and registers it, then rac_init()s commons. Mirrors
/// engine_ondevice.go's initRAC() minus llamacpp registration (GGUF stays on
/// the Go in-process path) and minus rac_desktop_http_transport_register
/// (this helper must make zero network calls).
nonisolated func bootstrapRAC(modelsBaseDir: String) -> Bool {
    gAdapter.abi_version = UInt32(RAC_PLATFORM_ADAPTER_ABI_VERSION)
    gAdapter.struct_size = UInt32(MemoryLayout<rac_platform_adapter_t>.stride)
    gAdapter.file_exists = waFileExists
    gAdapter.file_read = waFileRead
    gAdapter.file_write = waFileWrite
    gAdapter.file_delete = waFileDelete
    gAdapter.secure_get = waSecureGet
    gAdapter.secure_set = waSecureSet
    gAdapter.secure_delete = waSecureDelete
    gAdapter.log = waLog
    gAdapter.now_ms = waNowMs
    gAdapter.get_memory_info = waGetMemoryInfo
    gAdapter.http_download = nil
    gAdapter.http_download_cancel = nil
    gAdapter.extract_archive = nil
    gAdapter.get_vendor_id = nil
    gAdapter.file_list_directory = waFileListDirectory
    gAdapter.is_non_empty_directory = waIsNonEmptyDirectory
    gAdapter.user_data = nil

    FileHandle.standardError.write(Data("""
    wally-mlx: diag protoABIKeepAlive.count=\(protoABIKeepAlive.count) \
    struct_size=\(MemoryLayout<rac_platform_adapter_t>.stride) \
    off(file_list_directory)=\(MemoryLayout<rac_platform_adapter_t>.offset(of: \.file_list_directory) ?? -1) \
    off(is_non_empty_directory)=\(MemoryLayout<rac_platform_adapter_t>.offset(of: \.is_non_empty_directory) ?? -1) \
    off(user_data)=\(MemoryLayout<rac_platform_adapter_t>.offset(of: \.user_data) ?? -1)\n
    """.utf8))

    rac_logger_set_stderr_always(0)
    rac_logger_set_min_level(RAC_LOG_ERROR)

    let logTag = strdup("wally-mlx")
    var cfg = rac_config_t()
    let initResult: rac_result_t = withUnsafePointer(to: gAdapter) { adapterPtr in
        cfg.platform_adapter = adapterPtr
        cfg.log_level = RAC_LOG_ERROR
        cfg.log_tag = UnsafePointer(logTag)
        cfg.reserved = nil
        return rac_init(&cfg)
    }
    FileHandle.standardError.write(Data("wally-mlx: diag rac_init returned \(initResult)\n".utf8))
    guard initResult == RAC_SUCCESS else {
        FileHandle.standardError.write(Data("wally-mlx: rac_init failed: \(initResult)\n".utf8))
        return false
    }

    let baseResult = modelsBaseDir.withCString { rac_model_paths_set_base_dir($0) }
    FileHandle.standardError.write(Data("wally-mlx: diag base_dir=\(modelsBaseDir) result=\(baseResult)\n".utf8))
    guard baseResult == RAC_SUCCESS else {
        FileHandle.standardError.write(Data("wally-mlx: rac_model_paths_set_base_dir failed: \(baseResult)\n".utf8))
        return false
    }
    return true
}

/// Same layout convention runanywhere/localmodels.go's ModelsRoot() uses:
/// RUNANYWHERE_HOME (or the platform config dir)/RunAnywhere, nested under
/// RunAnywhere/Models if present, else Models directly. rac_model_paths_set_base_dir
/// wants the parent of the Models directory (it appends "Models" itself).
func modelsBaseDir() -> String {
    let env = ProcessInfo.processInfo.environment["RUNANYWHERE_HOME"]?.trimmingCharacters(in: .whitespaces) ?? ""
    let home: String
    if !env.isEmpty {
        home = env
    } else {
        let appSupport = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
        home = (appSupport?.path ?? NSHomeDirectory() + "/Library/Application Support") + "/RunAnywhere"
    }
    let nested = home + "/RunAnywhere/Models"
    var isDir: ObjCBool = false
    if FileManager.default.fileExists(atPath: nested, isDirectory: &isDir), isDir.boolValue {
        return home + "/RunAnywhere"
    }
    return home
}

// MARK: - Entry point

@main
struct WallyMLXServer {
    static func main() async {
        let args = CommandLine.arguments
        guard args.count >= 3, let port = UInt16(args[2]) else {
            FileHandle.standardError.write(Data("usage: wally-mlx <model-id> <port>\n".utf8))
            exit(2)
        }
        let modelID = args[1]

        guard bootstrapRAC(modelsBaseDir: modelsBaseDir()) else {
            exit(1)
        }

        guard await MainActor.run(body: { MLX.register() }) else {
            FileHandle.standardError.write(Data("wally-mlx: MLX.register() failed; this build cannot serve MLX\n".utf8))
            exit(1)
        }

        let discovery = await CppBridge.ModelRegistry.shared.discoverDownloadedModels()
        if discovery.hasError {
            FileHandle.standardError.write(Data("wally-mlx: model discovery error: \(discovery.error.message)\n".utf8))
        }

        guard await CppBridge.ModelRegistry.shared.get(modelId: modelID) != nil else {
            FileHandle.standardError.write(Data("wally-mlx: model '\(modelID)' was not found by registry discovery\n".utf8))
            exit(1)
        }

        var loadRequest = RAModelLoadRequest()
        loadRequest.modelID = modelID
        loadRequest.framework = .mlx
        loadRequest.validateAvailability = false
        let loadResult = await CppBridge.ModelLifecycle.load(loadRequest)
        if loadResult.hasError {
            FileHandle.standardError.write(Data("wally-mlx: model load failed: \(loadResult.error.message)\n".utf8))
            exit(1)
        }
        FileHandle.standardError.write(Data("wally-mlx: loaded '\(modelID)' (resolved: \(loadResult.resolvedPath))\n".utf8))

        do {
            try runServer(port: port, modelID: modelID)
        } catch {
            FileHandle.standardError.write(Data("wally-mlx: server failed: \(error)\n".utf8))
            exit(1)
        }
    }
}
