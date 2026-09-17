/**
 * @file cli_formatter.h
 * @brief Minimal, tasteful color formatter for `--help` (bold headings,
 * bold+cyan command/option names, plain descriptions) -- think `gh`.
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

// The same tasteful, minimal palette CliFormatter uses for `--help`, shared so
// other output (`wally about`) styles itself identically instead of picking
// its own ANSI codes.
namespace cli_color {

// Every field is "" when color is disabled, so a caller can always splice
// these in (`pal.bold + text + pal.reset`) without an if/else at the call
// site -- pasting empty strings is a no-op.
struct Palette {
    const char* bold = "";
    const char* bold_cyan = "";
    const char* blue = "";
    const char* reset = "";
};

Palette make_palette(bool enabled);

}  // namespace cli_color

// Overrides just enough of CLI::Formatter to color section headings and
// command/option names, computing column padding from the visible (plain)
// text rather than the ANSI-decorated one -- CLI11's own setw-based padding
// would otherwise miscount escape bytes as visible characters and misalign
// the description column. Everything else (usage, footer, description
// wrapping) is untouched.
class CliFormatter : public CLI::Formatter {
  public:
    explicit CliFormatter(bool color_enabled);

    CLI11_NODISCARD std::string make_group(std::string group, bool is_positional,
                                            std::vector<const CLI::Option*> opts) const override;
    std::string make_subcommands(const CLI::App* app, CLI::AppFormatMode mode) const override;
    std::string make_subcommand(const CLI::App* sub) const override;
    std::string make_option(const CLI::Option* opt, bool is_positional) const override;

  private:
    bool color_enabled_;
};

}  // namespace wally

#endif  // WALLY_CLI_FORMATTER_H
