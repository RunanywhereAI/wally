#ifndef WALLY_ANTHROPIC_TRANSLATE_H
#define WALLY_ANTHROPIC_TRANSLATE_H

#include <map>
#include <string>
#include <vector>

#include <nlohmann/json.hpp>

/// The wire-format translation, with no sockets in it.
///
/// Separated from the server so the mapping can be tested by handing it JSON
/// and reading JSON back, which is the only part of this worth testing: the
/// HTTP plumbing is cpp-httplib's, and the interesting bugs are all in the
/// shapes.
namespace wally::anthropic::translate {

using Json = nlohmann::json;

/// Anthropic Messages request -> OpenAI chat completion request.
///
/// `model` replaces whatever model the caller named, because Claude Code sends
/// its own model ids and the endpoint behind us has never heard of them.
Json RequestToOpenAI(const Json& anthropic, const std::string& model);

/// OpenAI chat completion -> a whole Anthropic message.
Json ResponseToAnthropic(const Json& openai, const std::string& model);

/// One OpenAI stream chunk, turned into the Anthropic SSE events it implies.
///
/// Anthropic's stream is a state machine — message_start, content_block_start,
/// deltas, content_block_stop, message_delta, message_stop — where OpenAI's is
/// a flat run of deltas. `state` carries what has already been emitted so the
/// opening events fire exactly once.
struct StreamState {
    /// A call being assembled from the stream. OpenAI spreads one across as
    /// many chunks as it likes, naming it once and then sending its arguments
    /// a few characters at a time.
    struct ToolCall {
        std::string id;
        std::string name;
        std::string arguments;
    };

    bool opened = false;
    bool block_open = false;
    /// Set once the endpoint has reported a failure, after which the closing
    /// events would be describing a turn that never happened.
    bool failed = false;
    bool closed = false;
    std::string message_id;
    std::string model;
    std::string stop_reason;
    int input_tokens = 0;
    int output_tokens = 0;

    /// The calls so far, in the order they were first seen, which is the order
    /// they are written out in.
    ///
    /// Endpoints identify a call in two different ways and the slot is what
    /// reconciles them: OpenAI numbers its calls and dribbles the arguments of
    /// each across chunks, while others send a call whole and number nothing,
    /// leaning on the id instead. Keying on either alone merges calls that are
    /// separate or splits one that is not.
    std::map<int, ToolCall> tool_calls;
    std::map<int, int> tool_slot_by_index;
    std::map<std::string, int> tool_slot_by_id;
    int next_tool_slot = 0;
};

/// Returns the SSE text to write for `chunk`, or empty when it implies nothing.
std::string StreamChunkToAnthropic(const Json& chunk, StreamState* state);

/// The closing events, written once the upstream stream ends.
std::string StreamCloseToAnthropic(StreamState* state);

/// Report one protocol failure; never emit successful terminal/tool events afterward.
std::string StreamErrorToAnthropic(StreamState* state, const std::string& message);

/// An Anthropic-shaped error body, so a failure reads as one to the client
/// rather than as a malformed message.
std::string ErrorBody(const std::string& type, const std::string& message);

/// Reads the failure out of a body the endpoint sent with a success status.
///
/// Returns false when `payload` carries no error, which is the ordinary case.
bool PayloadError(const Json& payload, std::string* type, std::string* message);

/// Maps a failed upstream reply to the (type, message) an Anthropic-shaped error
/// should carry. `status` 0 means the endpoint never answered.
///
/// The HTTP status picks the error type, so a 403 reads as `permission_error`
/// and a 429 as `rate_limit_error` rather than the generic `api_error` a tool
/// will retry forever; the message is pulled from an OpenAI-style error body
/// when there is one, otherwise the raw body, otherwise a plain status line.
void UpstreamFailure(int status, const std::string& body, std::string* type,
                     std::string* message);

}  // namespace wally::anthropic::translate

#endif  // WALLY_ANTHROPIC_TRANSLATE_H
