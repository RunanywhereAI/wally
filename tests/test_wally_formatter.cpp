#include "test_common.h"

#include <memory>
#include <regex>
#include <sstream>

#include "cli_formatter.h"

namespace {

std::string StripColor(const std::string& value) {
    return std::regex_replace(value, std::regex("\033\\[[0-9;]*m"), "");
}

std::string Render(bool color, CLI::AppFormatMode mode = CLI::AppFormatMode::Normal) {
    CLI::App app{"Run local and cloud models", "wally"};
    app.formatter(std::make_shared<wally::CliFormatter>(color));
    bool flag = false;
    app.add_flag("--flag", flag,
                 "A long explanation of an option that should wrap within an ordinary terminal "
                 "without moving the next line back to the left margin.");
    auto* command = app.add_subcommand("opencode", "Open a coding tool");
    command->footer(wally::examples_footer({
        {"wally opencode -m qwen3-0.6b", "Use a downloaded model"},
        {"wally opencode --cloud -m glm-5.3-flash", "Use a cloud model"},
    }));
    app.footer(wally::examples_footer({{"wally models list", "Browse models"}}, "Get started"));
    return mode == CLI::AppFormatMode::Sub ? command->help("wally opencode", mode) : app.help();
}

TestResult test_examples_are_copyable() {
    TestResult result;
    result.test_name = "examples_are_copyable";
    const std::string command = "wally opencode -m \"/a path containing spaces/model.gguf\" -- run \"explain this project\"";
    const std::string footer = wally::examples_footer({
        {command, "A long explanation that wraps onto a second line without adding prose to the "
                  "command or changing any of its arguments when the example is pasted into a shell"},
        {"wally account login", ""},
    });
    std::istringstream lines(footer);
    std::string line;
    int notes = 0;
    int commands = 0;
    while (std::getline(lines, line)) {
        if (!line.empty() && (line.back() == ' ' || line.back() == '\t')) {
            result.details = "footer has trailing whitespace";
            return result;
        }
        if (line.rfind("  # ", 0) == 0) {
            ++notes;
            if (line.size() > 80) {
                result.details = "note exceeds the terminal width";
                return result;
            }
        } else if (line.rfind("  ", 0) == 0) {
            if (line != "  " + command && line != "  wally account login") {
                result.details = "command was wrapped or its arguments changed";
                return result;
            }
            ++commands;
        }
    }
    result.passed = notes >= 2 && commands == 2 && footer.back() != '\n';
    if (!result.passed) result.details = "expected wrapped comments and two intact commands";
    return result;
}

TestResult test_color_preserves_readable_text() {
    TestResult result;
    result.test_name = "color_preserves_readable_text";
    for (const auto mode : {CLI::AppFormatMode::Normal, CLI::AppFormatMode::Sub}) {
        const std::string plain = Render(false, mode);
        const std::string colored = Render(true, mode);
        if (plain.find('\033') != std::string::npos || StripColor(colored) != plain ||
            colored.find("\033[1;36m  wally ") == std::string::npos ||
            colored.find("\033[1m" + std::string(mode == CLI::AppFormatMode::Sub ? "Examples:" : "Get started:")) ==
                std::string::npos) {
            result.details = "color changed text/layout or omitted the footer palette";
            return result;
        }
    }
    result.passed = true;
    return result;
}

TestResult test_help_fits_standard_terminal() {
    TestResult result;
    result.test_name = "help_fits_standard_terminal";
    const std::string help = Render(false);
    std::istringstream lines(help);
    std::string line;
    while (std::getline(lines, line)) {
        if (line.size() > 80) {
            result.details = "line exceeds 80 columns: " + line;
            return result;
        }
        if (!line.empty() && (line.back() == ' ' || line.back() == '\t')) {
            result.details = "help has trailing whitespace";
            return result;
        }
    }
    result.passed = help.find("\n\n\n") == std::string::npos &&
                    help.back() == '\n' && help[help.size() - 2] != '\n';
    if (!result.passed) result.details = "help has excess blank lines";
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_formatter");
    suite.add("examples_are_copyable", test_examples_are_copyable);
    suite.add("color_preserves_readable_text", test_color_preserves_readable_text);
    suite.add("help_fits_standard_terminal", test_help_fits_standard_terminal);
    return suite.run(argc, argv);
}
