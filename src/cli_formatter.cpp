#include "cli_formatter.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <sstream>

#ifdef _WIN32
#include <io.h>
#else
#include <unistd.h>
#endif

namespace wally {

namespace cli_color {

namespace {
constexpr const char* kBoldCode = "\033[1m";
constexpr const char* kBoldCyanCode = "\033[1;36m";
constexpr const char* kBlueCode = "\033[34m";
constexpr const char* kResetCode = "\033[0m";
}  // namespace

Palette make_palette(bool enabled) {
    if (!enabled) return Palette{};
    return Palette{kBoldCode, kBoldCyanCode, kBlueCode, kResetCode};
}

}  // namespace cli_color

namespace {

std::string colorize(const std::string& text, const char* code, bool enabled) {
    if (!enabled || text.empty()) return text;
    return std::string(code) + text + cli_color::kResetCode;
}

}  // namespace

bool color_output_enabled(bool no_color_flag) {
    if (no_color_flag) return false;
    if (std::getenv("NO_COLOR") != nullptr) return false;
#ifdef _WIN32
    return _isatty(_fileno(stdout)) != 0;
#else
    return isatty(fileno(stdout)) != 0;
#endif
}

CliFormatter::CliFormatter(bool color_enabled) : color_enabled_(color_enabled) {}

std::string CliFormatter::make_group(std::string group, bool is_positional,
                                      std::vector<const CLI::Option*> opts) const {
    std::stringstream out;
    out << "\n" << colorize(group, cli_color::kBoldCode, color_enabled_) << ":\n";
    for (const CLI::Option* opt : opts) {
        out << make_option(opt, is_positional);
    }
    return out.str();
}

std::string CliFormatter::make_subcommands(const CLI::App* app, CLI::AppFormatMode mode) const {
    std::stringstream out;
    std::vector<const CLI::App*> subcommands = app->get_subcommands({});

    // Make a list in definition order of the groups seen (mirrors
    // CLI::Formatter::make_subcommands -- only the heading gets color here).
    std::vector<std::string> subcmd_groups_seen;
    for (const CLI::App* com : subcommands) {
        if (com->get_name().empty()) {
            if (!com->get_group().empty() && com->get_group().front() != '+') {
                out << make_expanded(com, mode);
            }
            continue;
        }
        std::string group_key = com->get_group();
        if (!group_key.empty() &&
            std::find_if(subcmd_groups_seen.begin(), subcmd_groups_seen.end(),
                         [&group_key](const std::string& a) {
                             return CLI::detail::to_lower(a) == CLI::detail::to_lower(group_key);
                         }) == subcmd_groups_seen.end()) {
            subcmd_groups_seen.push_back(group_key);
        }
    }

    for (const std::string& group : subcmd_groups_seen) {
        out << '\n' << colorize(group, cli_color::kBoldCode, color_enabled_) << ":\n";
        std::vector<const CLI::App*> subcommands_group = app->get_subcommands([&group](const CLI::App* sub_app) {
            return CLI::detail::to_lower(sub_app->get_group()) == CLI::detail::to_lower(group);
        });
        for (const CLI::App* new_com : subcommands_group) {
            if (new_com->get_name().empty()) continue;
            if (mode != CLI::AppFormatMode::All) {
                out << make_subcommand(new_com);
            } else {
                out << new_com->help(new_com->get_name(), CLI::AppFormatMode::Sub);
                out << '\n';
            }
        }
    }

    return out.str();
}

std::string CliFormatter::make_subcommand(const CLI::App* sub) const {
    std::stringstream out;
    const std::string suffix = sub->get_required() ? " " + get_label("REQUIRED") : "";
    const std::string plain_name = "  " + sub->get_display_name(true) + suffix;

    out << colorize("  " + sub->get_display_name(true), cli_color::kBoldCyanCode, color_enabled_) << suffix;
    if (plain_name.length() < get_column_width()) {
        out << std::string(get_column_width() - plain_name.length(), ' ');
    }
    CLI::detail::streamOutAsParagraph(out, sub->get_description(), get_right_column_width(),
                                      std::string(get_column_width(), ' '), true);
    out << '\n';
    return out.str();
}

std::string CliFormatter::make_option(const CLI::Option* opt, bool is_positional) const {
    std::stringstream out;
    const std::size_t column_width = get_column_width();

    if (is_positional) {
        const std::string plain_left = "  " + make_option_name(opt, true) + make_option_opts(opt);
        const std::string desc = make_option_desc(opt);

        out << colorize("  " + make_option_name(opt, true), cli_color::kBoldCyanCode, color_enabled_) << make_option_opts(opt);
        if (plain_left.length() < column_width) {
            out << std::string(column_width - plain_left.length(), ' ');
        }

        if (!desc.empty()) {
            bool skip_first_line_prefix = true;
            if (plain_left.length() >= column_width) {
                out << '\n';
                skip_first_line_prefix = false;
            }
            CLI::detail::streamOutAsParagraph(out, desc, get_right_column_width(), std::string(column_width, ' '),
                                              skip_first_line_prefix);
        }
        out << '\n';
        return out.str();
    }

    // Non-positional: same short-name / long-name column split as
    // CLI::Formatter::make_option, reproduced here because coloring the name
    // and padding it with std::setw don't mix -- setw counts the ANSI escape
    // bytes as visible characters and under-pads the description column.
    // `visible_length` tracks what setw would have measured on the plain
    // (uncolored) text so the layout stays identical either way.
    const std::string names_combined = make_option_name(opt, false);
    const std::string opts_text = make_option_opts(opt);
    const std::string desc = make_option_desc(opt);

    const auto names = CLI::detail::split(names_combined, ',');
    std::vector<std::string> short_names_v;
    std::vector<std::string> long_names_v;
    std::for_each(names.begin(), names.end(), [&short_names_v, &long_names_v](const std::string& name) {
        if (name.find("--", 0) != std::string::npos)
            long_names_v.push_back(name);
        else
            short_names_v.push_back(name);
    });

    const std::string short_names = CLI::detail::join(short_names_v, ", ");
    const std::string long_names = CLI::detail::join(long_names_v, ", ");

    const auto short_column_width = static_cast<int>(column_width / 3);
    const auto long_column_width =
        static_cast<int>(std::ceil(static_cast<float>(column_width) / 3.0f * 2.0f));
    int short_over_size = 0;
    std::size_t visible_length = 0;

    if (!short_names.empty()) {
        std::string plain_short = "  " + short_names;
        if (long_names.empty() && !opts_text.empty()) plain_short += opts_text;
        if (!long_names.empty()) plain_short += ",";
        if (static_cast<int>(plain_short.length()) >= short_column_width) {
            plain_short += " ";
            short_over_size = static_cast<int>(plain_short.length()) - short_column_width;
        }

        const std::string colored_name = colorize("  " + short_names, cli_color::kBoldCyanCode, color_enabled_);
        const std::string trailer = plain_short.substr(2 + short_names.length());
        out << colored_name << trailer;
        visible_length += plain_short.length();
        if (static_cast<int>(plain_short.length()) < short_column_width) {
            const std::size_t pad = static_cast<std::size_t>(short_column_width) - plain_short.length();
            out << std::string(pad, ' ');
            visible_length += pad;
        }
    } else {
        out << std::string(static_cast<std::size_t>(short_column_width), ' ');
        visible_length += static_cast<std::size_t>(short_column_width);
    }

    short_over_size = (std::min)(short_over_size, long_column_width);
    const auto adjusted_long_width = long_column_width - short_over_size;

    if (!long_names.empty()) {
        std::string plain_long = long_names;
        if (!opts_text.empty()) plain_long += opts_text;
        if (static_cast<int>(plain_long.length()) >= adjusted_long_width) plain_long += " ";

        const std::string colored_name = colorize(long_names, cli_color::kBoldCyanCode, color_enabled_);
        const std::string trailer = plain_long.substr(long_names.length());
        out << colored_name << trailer;
        visible_length += plain_long.length();
        if (static_cast<int>(plain_long.length()) < adjusted_long_width) {
            const std::size_t pad = static_cast<std::size_t>(adjusted_long_width) - plain_long.length();
            out << std::string(pad, ' ');
            visible_length += pad;
        }
    } else {
        out << std::string(static_cast<std::size_t>(adjusted_long_width), ' ');
        visible_length += static_cast<std::size_t>(adjusted_long_width);
    }

    if (!desc.empty()) {
        bool skip_first_line_prefix = true;
        if (visible_length > column_width) {
            out << '\n';
            skip_first_line_prefix = false;
        }
        CLI::detail::streamOutAsParagraph(out, desc, get_right_column_width(), std::string(column_width, ' '),
                                          skip_first_line_prefix);
    }

    out << '\n';
    return out.str();
}

}  // namespace wally
