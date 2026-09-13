// AI Assistant's settings file is the reader's, not ours: it holds their chat
// mode, their prompt preferences, and settings a future IDE will add that this
// build has never heard of. We rewrite two options inside it and must leave
// every other byte alone, on files we did not write and cannot predict.
//
// These are the cases that would eat somebody's configuration.

#include "test_common.h"

#include <string>

#include "ide/jetbrains_profile.h"

namespace {

using wally::ide::WithChatModel;

constexpr const char* kModel = "glm-5.3-flash";
constexpr const char* kId = "OpenAIAPI/glm-5.3-flash";

bool Contains(const std::string& haystack, const std::string& needle) {
    return haystack.find(needle) != std::string::npos;
}

int Occurrences(const std::string& haystack, const std::string& needle) {
    int count = 0;
    for (size_t at = haystack.find(needle); at != std::string::npos;
         at = haystack.find(needle, at + needle.size())) {
        ++count;
    }
    return count;
}

/// A real file, as RustRover wrote it: our two options plus settings of theirs,
/// and a block option whose children end in "/>" — the shape that fooled an
/// earlier version of the parser into cutting the block in half.
std::string RealWorldDocument() {
    return R"(<application>
  <component name="LLMSettings">
    <option name="chat_forced_llm_per_mode">
      <map>
        <entry key="chat" value="6" />
      </map>
    </option>
    <option name="chat_mode" value="CHAT" />
    <option name="chat_preferred_llm" value="auto_9f9c7f49" />
    <option name="chat_preferred_llm_per_mode">
      <map>
        <entry key="chat" value="auto_9f9c7f49" />
      </map>
    </option>
    <option name="custom_chat_instruction_was_shown" value="true" />
  </component>
</application>
)";
}

TestResult test_rewrites_the_model_and_keeps_everything_else() {
    TestResult result;
    result.test_name = "rewrites_the_model_and_keeps_everything_else";
    const std::string out = WithChatModel(RealWorldDocument(), kModel);
    result.actual = out;

    if (!Contains(out, std::string("<option name=\"chat_preferred_llm\" value=\"") + kId + "\" />")) {
        result.details = "the preferred model was not set";
        return result;
    }
    if (Contains(out, "auto_9f9c7f49")) {
        result.details = "the previous model id survived";
        return result;
    }
    // Theirs, all of it.
    if (!Contains(out, "<option name=\"chat_mode\" value=\"CHAT\" />") ||
        !Contains(out, "<option name=\"custom_chat_instruction_was_shown\" value=\"true\" />") ||
        !Contains(out, "<entry key=\"chat\" value=\"6\" />")) {
        result.details = "a setting belonging to the reader was lost";
        return result;
    }
    // The forced-mode block must survive whole, not be cut at its child's "/>".
    if (Occurrences(out, "</map>") != 2 || Occurrences(out, "</option>") != 2) {
        result.details = "a block option was left unbalanced";
        return result;
    }
    result.passed = true;
    return result;
}

// Running the same command twice is ordinary: relaunching an editor reconfigures
// it. The second run must produce exactly the first run's file.
TestResult test_is_idempotent() {
    TestResult result;
    result.test_name = "is_idempotent";
    const std::string once = WithChatModel(RealWorldDocument(), kModel);
    const std::string twice = WithChatModel(once, kModel);
    result.expected = "the second run changes nothing";
    if (once != twice) {
        result.details = "a second run rewrote the file";
        result.actual = twice;
        return result;
    }
    if (Occurrences(once, "name=\"chat_preferred_llm\"") != 1 ||
        Occurrences(once, "name=\"chat_preferred_llm_per_mode\"") != 1) {
        result.details = "the option was written more than once";
        result.actual = once;
        return result;
    }
    result.passed = true;
    return result;
}

// Switching model is the other ordinary case, and the old id must not linger.
TestResult test_switching_model_replaces_the_previous_one() {
    TestResult result;
    result.test_name = "switching_model_replaces_the_previous_one";
    const std::string first = WithChatModel(RealWorldDocument(), "qwen3.8-27b");
    const std::string second = WithChatModel(first, kModel);
    result.actual = second;
    if (Contains(second, "qwen3.8-27b")) {
        result.details = "the previous model id was left behind";
        return result;
    }
    if (Occurrences(second, "name=\"chat_preferred_llm\"") != 1) {
        result.details = "switching model duplicated the option";
        return result;
    }
    result.passed = true;
    return result;
}

// An IDE where nobody has opened AI Assistant has no such file at all.
TestResult test_creates_the_component_when_the_file_is_empty() {
    TestResult result;
    result.test_name = "creates_the_component_when_the_file_is_empty";
    const std::string out = WithChatModel("", kModel);
    result.actual = out;
    if (!Contains(out, "<application>") || !Contains(out, "</application>") ||
        !Contains(out, "<component name=\"LLMSettings\">") || !Contains(out, kId)) {
        result.details = "an empty file did not produce a usable document";
        return result;
    }
    // And the result must survive its own second run.
    if (WithChatModel(out, kModel) != out) {
        result.details = "the created document is not stable under a second run";
        return result;
    }
    result.passed = true;
    return result;
}

// The file exists with other components in it, which is the common case: the
// IDE keeps unrelated settings in the same tree.
TestResult test_adds_the_component_beside_existing_ones() {
    TestResult result;
    result.test_name = "adds_the_component_beside_existing_ones";
    const std::string document = R"(<application>
  <component name="SomethingElse">
    <option name="chat_preferred_llm" value="not-ours" />
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (!Contains(out, "<component name=\"SomethingElse\">")) {
        result.details = "another component was dropped";
        return result;
    }
    // An option of the same name in a foreign component is not ours to touch.
    if (!Contains(out, "<option name=\"chat_preferred_llm\" value=\"not-ours\" />")) {
        result.details = "an identically named option in another component was rewritten";
        return result;
    }
    if (!Contains(out, kId)) {
        result.details = "our own component was not added";
        return result;
    }
    result.passed = true;
    return result;
}

// A component that never closes is a file we do not understand. Guessing at
// where it ended would rewrite settings we cannot see.
TestResult test_refuses_a_document_it_cannot_parse() {
    TestResult result;
    result.test_name = "refuses_a_document_it_cannot_parse";
    const std::string truncated = "<application>\n  <component name=\"LLMSettings\">\n";
    const std::string out = WithChatModel(truncated, kModel);
    result.expected = "empty, meaning refused";
    result.actual = out;
    if (!out.empty()) {
        result.details = "a malformed document was rewritten anyway";
        return result;
    }
    result.passed = true;
    return result;
}

// An option whose name merely starts with ours must survive: a prefix match
// would delete a setting that has nothing to do with this.
TestResult test_leaves_options_with_a_longer_name_alone() {
    TestResult result;
    result.test_name = "leaves_options_with_a_longer_name_alone";
    const std::string document = R"(<application>
  <component name="LLMSettings">
    <option name="chat_preferred_llm_fallback" value="keep-me" />
    <option name="chat_preferred_llm" value="replace-me" />
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (!Contains(out, "<option name=\"chat_preferred_llm_fallback\" value=\"keep-me\" />")) {
        result.details = "an option with a longer name was removed";
        return result;
    }
    if (Contains(out, "replace-me")) {
        result.details = "the option we own was not replaced";
        return result;
    }
    result.passed = true;
    return result;
}

// The component appearing twice is not something the IDE writes, but a file
// edited by hand can carry it. Only the first is ours to touch, and nothing may
// be lost from the second.
TestResult test_handles_a_repeated_component() {
    TestResult result;
    result.test_name = "handles_a_repeated_component";
    const std::string document = R"(<application>
  <component name="LLMSettings">
    <option name="chat_preferred_llm" value="first" />
  </component>
  <component name="LLMSettings">
    <option name="chat_mode" value="EDIT" />
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (!Contains(out, "<option name=\"chat_mode\" value=\"EDIT\" />")) {
        result.details = "the second component was damaged";
        return result;
    }
    if (Contains(out, "\"first\"")) {
        result.details = "the first component's model was not replaced";
        return result;
    }
    if (Occurrences(out, "<component name=\"LLMSettings\">") != 2 ||
        Occurrences(out, "</component>") != 2) {
        result.details = "the document's component structure changed";
        return result;
    }
    result.passed = true;
    return result;
}

// Windows line endings, and a file with no trailing newline. Both are files the
// reader may hand us, and neither is a reason to mangle the document.
TestResult test_survives_crlf_and_a_missing_trailing_newline() {
    TestResult result;
    result.test_name = "survives_crlf_and_a_missing_trailing_newline";
    const std::string crlf =
        "<application>\r\n  <component name=\"LLMSettings\">\r\n"
        "    <option name=\"chat_mode\" value=\"CHAT\" />\r\n  </component>\r\n</application>";
    const std::string out = WithChatModel(crlf, kModel);
    result.actual = out;
    if (!Contains(out, kId)) {
        result.details = "a CRLF document was not updated";
        return result;
    }
    if (!Contains(out, "chat_mode")) {
        result.details = "a CRLF document lost a setting";
        return result;
    }
    if (Occurrences(out, "</component>") != 1 || Occurrences(out, "</application>") != 1) {
        result.details = "a CRLF document came out structurally wrong";
        return result;
    }
    result.passed = true;
    return result;
}

// Every model id reaching this has been through ModelIdIsSafe, which refuses
// quotes and angle brackets. This pins that the id is placed verbatim, so a
// future caller that skips that check is a visible break rather than a silent
// one.
TestResult test_writes_the_model_id_verbatim() {
    TestResult result;
    result.test_name = "writes_the_model_id_verbatim";
    const std::string out = WithChatModel(RealWorldDocument(), "qwen3.8-27b-1bit-npu");
    result.actual = out;
    if (Occurrences(out, "OpenAIAPI/qwen3.8-27b-1bit-npu") != 2) {
        result.details = "the id was not written to both the preference and the per-mode map";
        return result;
    }
    result.passed = true;
    return result;
}

// A document cut short mid-write — a crash, a full disk, an editor mid-save —
// still holds the reader's settings. Replacing it with a fresh file loses them
// silently while reporting success, which is worse than any error.
TestResult test_refuses_a_document_with_no_closing_root() {
    TestResult result;
    result.test_name = "refuses_a_document_with_no_closing_root";
    const std::string truncated = R"(<application>
  <component name="SomethingElse">
    <option name="precious" value="do-not-lose-me" />
  </component>
)";
    const std::string out = WithChatModel(truncated, kModel);
    result.expected = "empty, meaning refused";
    result.actual = out;
    if (out.empty()) {
        result.passed = true;
        return result;
    }
    result.details = Contains(out, "do-not-lose-me")
                         ? "the document was rewritten rather than refused"
                         : "a setting belonging to the reader was destroyed";
    return result;
}

// Markup quoted inside CDATA is text, not markup. Deleting through it destroys
// the reader's content and can leave the file unparseable.
TestResult test_ignores_options_quoted_inside_cdata() {
    TestResult result;
    result.test_name = "ignores_options_quoted_inside_cdata";
    const std::string document =
        "<application>\n  <component name=\"LLMSettings\">\n"
        "    <option name=\"note\"><![CDATA[<option name=\"chat_preferred_llm\" value=\"x\"/>]]>"
        "</option>\n    <option name=\"chat_preferred_llm\" value=\"real\" />\n"
        "  </component>\n</application>\n";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (out.empty()) {
        result.details = "a legal document was refused";
        return result;
    }
    if (!Contains(out, "<![CDATA[") || !Contains(out, "]]>")) {
        result.details = "the CDATA section was damaged";
        return result;
    }
    if (Contains(out, "value=\"real\"")) {
        result.details = "the real option was not replaced";
        return result;
    }
    result.passed = true;
    return result;
}

// The same, for a comment. A commented-out setting is the reader's note to
// themselves and has to survive untouched.
TestResult test_ignores_options_inside_comments() {
    TestResult result;
    result.test_name = "ignores_options_inside_comments";
    const std::string document = R"(<application>
  <component name="LLMSettings">
    <!-- reference: <option name="chat_preferred_llm" value="decoy" /> -->
    <option name="chat_mode" value="CHAT" />
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (out.empty()) {
        result.details = "a legal document was refused";
        return result;
    }
    if (!Contains(out, "<!-- reference: <option name=\"chat_preferred_llm\" value=\"decoy\" /> -->")) {
        result.details = "the comment was damaged";
        return result;
    }
    if (!Contains(out, kId) || !Contains(out, "chat_mode")) {
        result.details = "the real component was not updated";
        return result;
    }
    result.passed = true;
    return result;
}

// A commented-out copy of the component itself must not be mistaken for the
// real one, or the model is written into a comment and nothing takes effect.
TestResult test_is_not_fooled_by_a_commented_out_component() {
    TestResult result;
    result.test_name = "is_not_fooled_by_a_commented_out_component";
    const std::string document = R"(<application>
  <!-- was: <component name="LLMSettings"> old stuff </component> -->
  <component name="LLMSettings">
    <option name="chat_mode" value="CHAT" />
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (out.empty()) {
        result.details = "a legal document was refused";
        return result;
    }
    const size_t comment_end = out.find("-->");
    const size_t written = out.find(kId);
    if (written == std::string::npos) {
        result.details = "the model was never written";
        return result;
    }
    if (comment_end == std::string::npos || written < comment_end) {
        result.details = "the model was written inside the comment";
        return result;
    }
    result.passed = true;
    return result;
}

// Attribute order, single quotes and loose whitespace are all legal XML, and
// an option written that way is still the option we own.
TestResult test_matches_options_however_they_are_written() {
    TestResult result;
    result.test_name = "matches_options_however_they_are_written";
    const std::string document =
        "<application>\n  <component name='LLMSettings'>\n"
        "    <option value=\"swapped\" name=\"chat_preferred_llm\" />\n"
        "    <option   name = 'chat_preferred_llm_per_mode' >\n      <map/>\n    </option>\n"
        "  </component>\n</application>\n";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (out.empty()) {
        result.details = "a legal document was refused";
        return result;
    }
    if (Contains(out, "swapped")) {
        result.details = "an option written with its attributes swapped was not replaced";
        return result;
    }
    if (Occurrences(out, "name=\"chat_preferred_llm\"") != 1 ||
        Occurrences(out, "<component") != 1) {
        result.details = "a duplicate option or a second component was created";
        return result;
    }
    result.passed = true;
    return result;
}

// A `>` inside an attribute value is legal and must not be read as the end of
// the tag.
TestResult test_handles_a_greater_than_inside_an_attribute() {
    TestResult result;
    result.test_name = "handles_a_greater_than_inside_an_attribute";
    const std::string document = R"(<application>
  <component name="LLMSettings">
    <option name="prompt" value="if a > b then" />
    <option name="chat_preferred_llm" value="stale" />
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (out.empty()) {
        result.details = "a legal document was refused";
        return result;
    }
    if (Contains(out, "stale")) {
        result.details = "the option after a '>' in an attribute was not replaced";
        return result;
    }
    if (!Contains(out, "if a > b then")) {
        result.details = "the attribute holding '>' was damaged";
        return result;
    }
    result.passed = true;
    return result;
}

// An option of the same name nested inside another option's children belongs to
// that option, not to us.
TestResult test_leaves_a_nested_option_of_the_same_name_alone() {
    TestResult result;
    result.test_name = "leaves_a_nested_option_of_the_same_name_alone";
    const std::string document = R"(<application>
  <component name="LLMSettings">
    <option name="history">
      <option name="chat_preferred_llm" value="an-old-entry" />
    </option>
    <option name="chat_preferred_llm" value="the-real-one" />
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (out.empty()) {
        result.details = "a legal document was refused";
        return result;
    }
    if (!Contains(out, "an-old-entry")) {
        result.details = "a nested option belonging to another setting was removed";
        return result;
    }
    if (Contains(out, "the-real-one")) {
        result.details = "the direct child was not replaced";
        return result;
    }
    result.passed = true;
    return result;
}

// A nested component must not steal the closing tag: the new options belong to
// LLMSettings, as siblings of the nested element rather than children of it.
TestResult test_places_options_in_the_right_component_when_nested() {
    TestResult result;
    result.test_name = "places_options_in_the_right_component_when_nested";
    const std::string document = R"(<application>
  <component name="LLMSettings">
    <component name="Inner">
      <option name="inner_setting" value="keep" />
    </component>
  </component>
</application>
)";
    const std::string out = WithChatModel(document, kModel);
    result.actual = out;
    if (out.empty()) {
        result.details = "a legal document was refused";
        return result;
    }
    if (!Contains(out, "inner_setting")) {
        result.details = "the nested component was damaged";
        return result;
    }
    const size_t inner_close = out.find("</component>");
    const size_t written = out.find(kId);
    if (written == std::string::npos || written < inner_close) {
        result.details = "the model was written inside the nested component";
        return result;
    }
    result.passed = true;
    return result;
}

// An unterminated comment is a document we cannot read the shape of.
TestResult test_refuses_an_unterminated_comment() {
    TestResult result;
    result.test_name = "refuses_an_unterminated_comment";
    const std::string document =
        "<application>\n  <component name=\"LLMSettings\">\n    <!-- never closed\n";
    const std::string out = WithChatModel(document, kModel);
    result.expected = "empty, meaning refused";
    result.actual = out;
    if (!out.empty()) {
        result.details = "a document with an unterminated comment was rewritten";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_jetbrains_profile");
    suite.add("rewrites_the_model_and_keeps_everything_else",
              test_rewrites_the_model_and_keeps_everything_else);
    suite.add("is_idempotent", test_is_idempotent);
    suite.add("switching_model_replaces_the_previous_one",
              test_switching_model_replaces_the_previous_one);
    suite.add("creates_the_component_when_the_file_is_empty",
              test_creates_the_component_when_the_file_is_empty);
    suite.add("adds_the_component_beside_existing_ones",
              test_adds_the_component_beside_existing_ones);
    suite.add("refuses_a_document_it_cannot_parse", test_refuses_a_document_it_cannot_parse);
    suite.add("leaves_options_with_a_longer_name_alone",
              test_leaves_options_with_a_longer_name_alone);
    suite.add("handles_a_repeated_component", test_handles_a_repeated_component);
    suite.add("survives_crlf_and_a_missing_trailing_newline",
              test_survives_crlf_and_a_missing_trailing_newline);
    suite.add("writes_the_model_id_verbatim", test_writes_the_model_id_verbatim);
    suite.add("refuses_a_document_with_no_closing_root",
              test_refuses_a_document_with_no_closing_root);
    suite.add("ignores_options_quoted_inside_cdata", test_ignores_options_quoted_inside_cdata);
    suite.add("ignores_options_inside_comments", test_ignores_options_inside_comments);
    suite.add("is_not_fooled_by_a_commented_out_component",
              test_is_not_fooled_by_a_commented_out_component);
    suite.add("matches_options_however_they_are_written",
              test_matches_options_however_they_are_written);
    suite.add("handles_a_greater_than_inside_an_attribute",
              test_handles_a_greater_than_inside_an_attribute);
    suite.add("leaves_a_nested_option_of_the_same_name_alone",
              test_leaves_a_nested_option_of_the_same_name_alone);
    suite.add("places_options_in_the_right_component_when_nested",
              test_places_options_in_the_right_component_when_nested);
    suite.add("refuses_an_unterminated_comment", test_refuses_an_unterminated_comment);
    return suite.run(argc, argv);
}
