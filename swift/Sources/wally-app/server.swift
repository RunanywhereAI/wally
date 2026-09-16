// Minimal, single-threaded, blocking HTTP/1.1 server: GET /health, POST
// /v1/chat/completions. Not for production concurrency -- this is a
// dev-serve helper for one local model, matching rac_server's own "ONE model
// per server process" scope.

import Darwin
import Foundation
import RunAnywhere

enum ServerError: Error {
    case socketFailed
    case bindFailed(Int32)
    case listenFailed
}

func runServer(port: UInt16, modelID: String) throws -> Never {
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    guard fd >= 0 else { throw ServerError.socketFailed }

    var reuse: Int32 = 1
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &reuse, socklen_t(MemoryLayout<Int32>.size))

    var addr = sockaddr_in()
    addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port = port.bigEndian
    addr.sin_addr.s_addr = inet_addr("127.0.0.1")

    let bindResult = withUnsafePointer(to: &addr) { ptr -> Int32 in
        ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPtr in
            bind(fd, sockaddrPtr, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    guard bindResult == 0 else { throw ServerError.bindFailed(errno) }
    guard listen(fd, 16) == 0 else { throw ServerError.listenFailed }

    FileHandle.standardError.write(Data("wally-mlx: listening on 127.0.0.1:\(port)\n".utf8))

    while true {
        let clientFd = accept(fd, nil, nil)
        guard clientFd >= 0 else { continue }
        handleConnection(clientFd, modelID: modelID)
    }
}

private struct HTTPRequest {
    let method: String
    let path: String
    let body: Data
}

private func readHTTPRequest(_ fd: Int32) -> HTTPRequest? {
    var buffer = Data()
    var chunk = [UInt8](repeating: 0, count: 8192)
    let headerTerminator = Data("\r\n\r\n".utf8)

    while buffer.range(of: headerTerminator) == nil {
        let n = chunk.withUnsafeMutableBytes { ptr in read(fd, ptr.baseAddress, ptr.count) }
        guard n > 0 else { return nil }
        buffer.append(contentsOf: chunk[0..<n])
        if buffer.count > 1_000_000 { return nil }
    }

    guard let headerRange = buffer.range(of: headerTerminator) else { return nil }
    let headerData = buffer.subdata(in: buffer.startIndex..<headerRange.lowerBound)
    var bodyData = buffer.subdata(in: headerRange.upperBound..<buffer.endIndex)

    guard let headerText = String(data: headerData, encoding: .utf8) else { return nil }
    let lines = headerText.components(separatedBy: "\r\n")
    guard let requestLine = lines.first else { return nil }
    let parts = requestLine.split(separator: " ")
    guard parts.count >= 2 else { return nil }
    let method = String(parts[0])
    let path = String(parts[1])

    var contentLength = 0
    for line in lines.dropFirst() {
        guard let colon = line.firstIndex(of: ":") else { continue }
        let key = line[line.startIndex..<colon].trimmingCharacters(in: .whitespaces).lowercased()
        let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
        if key == "content-length" { contentLength = Int(value) ?? 0 }
    }

    while bodyData.count < contentLength {
        let want = min(chunk.count, contentLength - bodyData.count)
        let n = chunk.withUnsafeMutableBytes { ptr in read(fd, ptr.baseAddress, want) }
        guard n > 0 else { break }
        bodyData.append(contentsOf: chunk[0..<n])
    }

    return HTTPRequest(method: method, path: path, body: bodyData)
}

private func writeHTTPResponse(_ fd: Int32, status: Int, body: Data, contentType: String) {
    let statusText: String
    switch status {
    case 200: statusText = "OK"
    case 400: statusText = "Bad Request"
    case 404: statusText = "Not Found"
    default: statusText = "Internal Server Error"
    }
    var head = "HTTP/1.1 \(status) \(statusText)\r\n"
    head += "Content-Type: \(contentType)\r\n"
    head += "Content-Length: \(body.count)\r\n"
    head += "Connection: close\r\n\r\n"
    var data = Data(head.utf8)
    data.append(body)
    data.withUnsafeBytes { ptr in
        _ = write(fd, ptr.baseAddress, ptr.count)
    }
}

// writeRaw writes every byte, looping on short writes. The buffered
// writeHTTPResponse above cannot serve SSE (it sends one body with a
// Content-Length); a stream writes frames as tokens arrive and closes the
// socket to signal the end.
private func writeRaw(_ fd: Int32, _ data: Data) {
    data.withUnsafeBytes { (ptr: UnsafeRawBufferPointer) in
        guard var base = ptr.baseAddress else { return }
        var remaining = ptr.count
        while remaining > 0 {
            let n = write(fd, base, remaining)
            if n <= 0 { break }
            base = base.advanced(by: n)
            remaining -= n
        }
    }
}

private func handleConnection(_ fd: Int32, modelID: String) {
    defer { close(fd) }
    guard let request = readHTTPRequest(fd) else { return }
    switch (request.method, request.path) {
    case ("GET", "/health"):
        writeHTTPResponse(fd, status: 200, body: Data(#"{"status":"ok"}"#.utf8), contentType: "application/json")
    case ("POST", "/v1/chat/completions"):
        handleChatCompletions(fd, modelID: modelID, body: request.body)
    default:
        writeHTTPResponse(fd, status: 404, body: Data(#"{"error":{"message":"not found"}}"#.utf8), contentType: "application/json")
    }
}

// MARK: - /v1/chat/completions

private struct ChatRequestIn: Decodable {
    struct Msg: Decodable { let role: String; let content: String }
    let messages: [Msg]
    let temperature: Double?
    let max_tokens: Int?
    let stream: Bool?
}

private struct ChatResponseOut: Encodable {
    struct MsgOut: Encodable { let role: String; let content: String }
    struct ChoiceOut: Encodable { let index: Int; let message: MsgOut; let finish_reason: String }
    struct UsageOut: Encodable { let prompt_tokens: Int; let completion_tokens: Int; let total_tokens: Int }
    let id: String
    let object: String
    let created: Int
    let model: String
    let choices: [ChoiceOut]
    let usage: UsageOut
}

// StreamChunk is one OpenAI chat.completion.chunk frame. wally chat and the
// coding harnesses read the daemon's stream as SSE (data: <json>\n\n lines with
// choices[].delta.content), the same shape rac_server emits for GGUF, so an MLX
// model has to speak it too or the client renders an empty reply.
private struct StreamChunk: Encodable {
    struct Choice: Encodable {
        struct Delta: Encodable { let role: String?; let content: String? }
        let index: Int
        let delta: Delta
        let finish_reason: String?
    }
    let id: String
    let object: String
    let created: Int
    let model: String
    let choices: [Choice]
}

private struct StreamError: Encodable {
    struct E: Encodable { let message: String }
    let error: E
}

private func roleFromString(_ s: String) -> RAMessageRole {
    switch s.lowercased() {
    case "system": return .system
    case "assistant": return .assistant
    case "tool": return .tool
    default: return .user
    }
}

private func jsonErrorBody(_ message: String) -> Data {
    (try? JSONEncoder().encode(["error": ["message": message]])) ?? Data(#"{"error":{"message":"internal error"}}"#.utf8)
}

private func buildGenerateRequest(modelID: String, req: ChatRequestIn) -> RALLMGenerateRequest {
    var generateRequest = RALLMGenerateRequest()
    generateRequest.modelID = modelID
    generateRequest.messages = req.messages.map { m in
        var msg = RAChatMessage()
        msg.role = roleFromString(m.role)
        msg.content = m.content
        return msg
    }
    var options = RALLMGenerationOptions.defaults()
    if let maxTokens = req.max_tokens { options.maxOutputTokens = Int32(maxTokens) }
    if let temperature = req.temperature { options.temperature = Float(temperature) }
    generateRequest.options = options
    return generateRequest
}

private func handleChatCompletions(_ fd: Int32, modelID: String, body: Data) {
    guard let req = try? JSONDecoder().decode(ChatRequestIn.self, from: body), !req.messages.isEmpty else {
        writeHTTPResponse(fd, status: 400, body: jsonErrorBody("invalid or empty chat request"), contentType: "application/json")
        return
    }

    if req.stream == true {
        handleChatStream(fd, modelID: modelID, req: req)
        return
    }

    let requestToSend = buildGenerateRequest(modelID: modelID, req: req)

    do {
        let result = try runBlocking { try await CppBridge.LLM.shared.generate(requestToSend) }
        if result.hasError {
            writeHTTPResponse(fd, status: 500, body: jsonErrorBody(result.error.message), contentType: "application/json")
            return
        }
        let response = ChatResponseOut(
            id: "wally-mlx-\(UUID().uuidString)",
            object: "chat.completion",
            created: Int(Date().timeIntervalSince1970),
            model: modelID,
            choices: [
                ChatResponseOut.ChoiceOut(
                    index: 0,
                    message: .init(role: "assistant", content: result.text),
                    finish_reason: "stop"
                )
            ],
            usage: ChatResponseOut.UsageOut(
                prompt_tokens: Int(result.usage.inputTokens),
                completion_tokens: Int(result.usage.outputTokens),
                total_tokens: Int(result.usage.totalTokens)
            )
        )
        let data = try JSONEncoder().encode(response)
        writeHTTPResponse(fd, status: 200, body: data, contentType: "application/json")
    } catch {
        writeHTTPResponse(fd, status: 500, body: jsonErrorBody(String(describing: error)), contentType: "application/json")
    }
}

// handleChatStream serves stream:true by driving the SDK's token stream
// (CppBridge.LLM.generateStream -> AsyncStream<RALLMStreamEvent>) and writing
// each token as an OpenAI chat.completion.chunk. Only .token events carry the
// visible answer; .thinking (reasoning) is skipped, matching how result.text
// excludes it on the non-stream path. If the runtime returns the whole answer
// on .completed without token events, that text is emitted as one chunk so the
// reply is never empty.
private func handleChatStream(_ fd: Int32, modelID: String, req: ChatRequestIn) {
    let requestToSend = buildGenerateRequest(modelID: modelID, req: req)
    let id = "wally-mlx-\(UUID().uuidString)"
    let created = Int(Date().timeIntervalSince1970)

    var head = "HTTP/1.1 200 OK\r\n"
    head += "Content-Type: text/event-stream\r\n"
    head += "Cache-Control: no-cache\r\n"
    head += "Connection: close\r\n\r\n"
    writeRaw(fd, Data(head.utf8))

    do {
        try runBlocking { () async throws -> Void in
            let encoder = JSONEncoder()
            func send(role: String?, content: String?, finish: String?) {
                let chunk = StreamChunk(
                    id: id, object: "chat.completion.chunk", created: created, model: modelID,
                    choices: [.init(index: 0, delta: .init(role: role, content: content), finish_reason: finish)]
                )
                if let data = try? encoder.encode(chunk), let json = String(data: data, encoding: .utf8) {
                    writeRaw(fd, Data("data: \(json)\n\n".utf8))
                }
            }
            func sendError(_ message: String) {
                if let data = try? encoder.encode(StreamError(error: .init(message: message))),
                    let json = String(data: data, encoding: .utf8) {
                    writeRaw(fd, Data("data: \(json)\n\n".utf8))
                }
            }

            send(role: "assistant", content: nil, finish: nil)
            var streamed = ""
            let stream = try await CppBridge.LLM.shared.generateStream(requestToSend)
            for await event in stream {
                switch event.eventKind {
                case .token where !event.token.isEmpty:
                    streamed += event.token
                    send(role: nil, content: event.token, finish: nil)
                case .completed:
                    if streamed.isEmpty, event.hasResult, !event.result.text.isEmpty {
                        send(role: nil, content: event.result.text, finish: nil)
                    }
                case .error:
                    sendError(event.hasError ? event.error.message : "generation error")
                default:
                    break
                }
            }
            send(role: nil, content: nil, finish: "stop")
            writeRaw(fd, Data("data: [DONE]\n\n".utf8))
        }
    } catch {
        let encoder = JSONEncoder()
        if let data = try? encoder.encode(StreamError(error: .init(message: String(describing: error)))),
            let json = String(data: data, encoding: .utf8) {
            writeRaw(fd, Data("data: \(json)\n\n".utf8))
        }
    }
}

// MARK: - async -> sync bridge
//
// The accept loop above is a dedicated OS thread running blocking POSIX I/O,
// not a Swift-concurrency task, so it can safely block on a semaphore while
// the actual generate() call runs on the cooperative pool via Task {}.
func runBlocking<T: Sendable>(_ op: @escaping @Sendable () async throws -> T) throws -> T {
    let semaphore = DispatchSemaphore(value: 0)
    let box = ResultBox<T>()
    Task {
        do {
            box.result = .success(try await op())
        } catch {
            box.result = .failure(error)
        }
        semaphore.signal()
    }
    semaphore.wait()
    return try box.result!.get()
}

private final class ResultBox<T>: @unchecked Sendable {
    var result: Result<T, Error>?
}
