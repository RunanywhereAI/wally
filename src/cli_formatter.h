/**
 * @file cli_formatter.h
 * @brief Plain-text `--help` layout for wally: a `Usage:` line, sentence-case
 * section headings, one `-m, --model TEXT` column for options, a nested
 * command tree, and a verbatim `Examples:` footer. No color.
 */

#ifndef WALLY_CLI_FORMATTER_H
#define WALLY_CLI_FORMATTER_H

#include <string>
#include <vector>

#include <CLI11.hpp>

namespace wally {

// True when ANSI color is safe to emit on stdout: not forced off by
// --no-color or NO_COLOR, and stdout is actually a terminal. Piped or
// redirected output (CI logs, `| cat`, a file) always gets plain text.
bool color_output_enabled(bool no_color_flag);

// The palette other output (`wally about`, the default-model notice) styles
// itself with. `--help` itself is always plain.
namespace cli_color {

// Every field is "" when color is disabled, so a caller can always splice
// these in (`pal.bold + text + pal.reset`) without an if/else at the call
// site -- pasting empty strings is a no-op.
struct Palette {
    const char* bold = "";
    const char* bold_cyan = "";
    const char* blue = "";
    const char* red = "";
    const char* green = "";
    const char* reset = "";
};

Palette make_palette(bool enabled);

}  // namespace cli_color

// One line of an `Examples:` block: the command, and an optional note that
// says what it does.
struct Example {
    std::string command;
    std::string note;
};

// The `Examples:` footer a command's help ends with. Commands sit at a
// two-space indent and every note starts at the same column, so the block
// reads as a table whichever command it is under. No trailing newline.
std::string examples_footer(const std::vector<Example>& rows);

// Overrides CLI::Formatter to lay help out the way every wally command shows
// it: description, `Usage: wally ...`, Arguments, Options, Commands (with one
// level of nested subcommands), Examples. Padding is computed from plain text,
// trailing whitespace is stripped and blank lines are collapsed, so the output
// is the same whether it lands on a terminal or in a file.
class CliFormatter : public CLI::Formatter {
  public:
    explicit CliFormatter(bool color_enabled);

    std::string make_help(const CLI::App* app, std::string name, CLI::AppFormatMode mode) const override;
    std::string make_usage(const CLI::App* app, std::string name) const override;
    CLI11_NODISCARD std::string make_group(std::string group, bool is_positional,
                                            std::vector<const CLI::Option*> opts) const override;
    std::string make_subcommands(const CLI::App* app, CLI::AppFormatMode mode) const override;
    // The footer is printed verbatim by make_help; CLI11's own make_footer
    // would reflow it as a paragraph and collapse the example columns.
    std::string make_footer(const CLI::App* app) const override;
    std::string make_option(const CLI::Option* opt, bool is_positional) const override;
    // Just the value type (`TEXT`, `INT`, `FILE`, `{on,off}`), without the
    // validator text CLI11 appends (`INT:INT in [1 - 2147483647]`).
    std::string make_option_opts(const CLI::Option* opt) const override;

  private:
    // Renders one command row at a chosen left indent, so a parent prints at
    // "  " and its subcommands print nested beneath at a deeper indent. The
    // description column is the same for every row regardless of indent, which
    // is what keeps the two levels aligned.
    std::string make_subcommand_indented(const CLI::App* sub, const std::string& indent) const;
};

}  // namespace wally

#endif  // WALLY_CLI_FORMATTER_H
