#include "cli_formatter.h"

#include <algorithm>
#include <cctype>
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
constexpr const char* kRedCode = "\033[1;31m";
constexpr const char* kGreenCode = "\033[32m";
constexpr const char* kResetCode = "\033[0m";
}  // namespace

Palette make_palette(bool enabled) {
    if (!enabled) return Palette{};
    return Palette{kBoldCode, kBoldCyanCode, kBlueCode, kRedCode, kGreenCode, kResetCode};
}

}  // namespace cli_color

bool color_output_enabled(bool no_color_flag) {
    if (no_color_flag) return false;
    if (std::getenv("NO_COLOR") != nullptr) return false;
#ifdef _WIN32
    return _isatty(_fileno(stdout)) != 0;
#else
    return isatty(fileno(stdout)) != 0;
#endif
}

namespace {

// Where an example's note starts: two spaces of indent plus the longest
// command the help carries, with a gap after it.
constexpr std::size_t kExampleNoteColumn = 48;

// Shortest gap between a row's left column and its description.
constexpr std::size_t kColumnGap = 2;

// Sentence-case headings for CLI11's upper-case defaults. Groups a command
// registers itself pass through unchanged.
std::string heading_for(const std::string& group) {
    if (group == "OPTIONS") return "Options";
    if (group == "SUBCOMMANDS") return "Commands";
    return group;
}

// CLI11 renders a `!--hide-thinking` flag as `--hide-thinking{false}`; the
// brace suffix is noise to a reader.
std::string strip_flag_default(const std::string& name) {
    const std::size_t brace = name.find('{');
    return brace == std::string::npos ? name : name.substr(0, brace);
}

// `TEXT:FILE` -> `FILE`, `TEXT:{on,off}` -> `{on,off}`, and a validator
// description such as `INT:INT in [1 - 2147483647]` -> `INT`.
std::string simplify_type_name(const std::string& type_name) {
    const std::size_t colon = type_name.find(':');
    if (colon == std::string::npos) return type_name;
    const std::string suffix = type_name.substr(colon + 1);
    if (!suffix.empty() && suffix.front() == '{') return suffix;
    const bool one_word = std::all_of(suffix.begin(), suffix.end(), [](unsigned char c) {
        return std::isupper(c) != 0;
    });
    return one_word && !suffix.empty() ? suffix : type_name.substr(0, colon);
}

// Strips trailing whitespace from every line, collapses runs of blank lines to
// one, drops leading blank lines and ends with exactly one newline.
std::string tidy(const std::string& text) {
    std::string out;
    std::istringstream in(text);
    std::string line;
    bool previous_blank = true;  // suppresses blank lines at the top
    while (std::getline(in, line)) {
        const std::size_t end = line.find_last_not_of(" \t\r");
        line = end == std::string::npos ? std::string() : line.substr(0, end + 1);
        if (line.empty()) {
            if (previous_blank) continue;
            previous_blank = true;
        } else {
            previous_blank = false;
        }
        out += line;
        out += '\n';
    }
    while (out.size() >= 2 && out[out.size() - 1] == '\n' && out[out.size() - 2] == '\n') {
        out.pop_back();
    }
    return out;
}

// Left column padded to the description column, or the description dropped to
// the next line when the left column is too wide to leave a gap.
void stream_row(std::stringstream& out, const std::string& left, const std::string& desc,
                std::size_t column_width, std::size_t right_width) {
    out << left;
    if (desc.empty()) {
        out << '\n';
        return;
    }
    bool skip_first_line_prefix = true;
    if (left.length() + kColumnGap <= column_width) {
        out << std::string(column_width - left.length(), ' ');
    } else {
        out << '\n';
        skip_first_line_prefix = false;
    }
    CLI::detail::streamOutAsParagraph(out, desc, right_width, std::string(column_width, ' '),
                                      skip_first_line_prefix);
    out << '\n';
}

}  // namespace

std::string examples_footer(const std::vector<Example>& rows) {
    std::string out = "Examples:";
    for (const Example& row : rows) {
        std::string line = "  " + row.command;
        if (!row.note.empty()) {
            line.append(line.size() + kColumnGap <= kExampleNoteColumn ? kExampleNoteColumn - line.size()
                                                                       : kColumnGap,
                        ' ');
            line += row.note;
        }
        out += '\n' + line;
    }
    return out;
}

CliFormatter::CliFormatter(bool color_enabled) {
    // Help is plain by decision; the flag is kept so the call site reads the
    // same as the palette helpers above.
    static_cast<void>(color_enabled);
    label("POSITIONALS", "Arguments");
    label("SUBCOMMAND", "COMMAND");
    label("SUBCOMMANDS", "COMMANDS");
}

std::string CliFormatter::make_help(const CLI::App* app, std::string name,
                                    CLI::AppFormatMode mode) const {
    if (mode == CLI::AppFormatMode::Sub) {
        // `--help-all` renders a child command through this Sub path (see
        // make_subcommands below), which lands in CLI11's own make_expanded.
        // That ends with make_footer(app), and make_footer is suppressed
        // below so the *top-level* render can print the footer verbatim
        // instead of reflowed as a paragraph -- so append it here too, or a
        // child's Examples: block never shows up under --help-all.
        std::string help = CLI::Formatter::make_help(app, name, mode);
        const std::string footer = app->get_footer();
        if (!footer.empty()) {
            help += '\n' + footer + '\n';
        }
        return help;
    }
    std::stringstream out;
    out << make_description(app);
    out << make_usage(app, name);
    out << make_positionals(app);
    out << make_groups(app, mode);
    out << make_subcommands(app, mode);
    const std::string footer = app->get_footer();
    if (!footer.empty()) {
        out << '\n' << footer << '\n';
    }
    return tidy(out.str());
}

std::string CliFormatter::make_usage(const CLI::App* app, std::string name) const {
    std::string usage = CLI::Formatter::make_usage(app, name);
    usage.erase(0, usage.find_first_not_of('\n'));
    return "Usage: " + usage;
}

std::string CliFormatter::make_group(std::string group, bool is_positional,
                                      std::vector<const CLI::Option*> opts) const {
    std::stringstream out;
    out << "\n" << heading_for(group) << ":\n";
    for (const CLI::Option* opt : opts) {
        out << make_option(opt, is_positional);
    }
    return out.str();
}

std::string CliFormatter::make_subcommands(const CLI::App* app, CLI::AppFormatMode mode) const {
    std::stringstream out;
    std::vector<const CLI::App*> subcommands = app->get_subcommands({});

    // Groups in definition order, as CLI::Formatter::make_subcommands does.
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
        out << '\n' << heading_for(group) << ":\n";
        std::vector<const CLI::App*> subcommands_group = app->get_subcommands([&group](const CLI::App* sub_app) {
            return CLI::detail::to_lower(sub_app->get_group()) == CLI::detail::to_lower(group);
        });
        for (const CLI::App* new_com : subcommands_group) {
            if (new_com->get_name().empty()) continue;
            if (mode == CLI::AppFormatMode::All) {
                out << new_com->help(new_com->get_name(), CLI::AppFormatMode::Sub);
                out << '\n';
                continue;
            }
            out << make_subcommand_indented(new_com, "  ");
            // One level of nesting: a command's own subcommands print as
            // branches beneath it, marked with "> " so the nesting is obvious.
            // A subcommand hidden with an empty group (models load/unload/...)
            // is left out, same as at the top.
            for (const CLI::App* child : new_com->get_subcommands({})) {
                if (child->get_name().empty() || child->get_group().empty()) continue;
                out << make_subcommand_indented(child, "    > ");
            }
        }
    }
    return out.str();
}

std::string CliFormatter::make_footer(const CLI::App* /*app*/) const {
    return "";
}

std::string CliFormatter::make_subcommand_indented(const CLI::App* sub, const std::string& indent) const {
    std::stringstream out;
    // Primary name only (no ", alias" tail): the tree reads cleaner, and the
    // aliases still resolve on the command line.
    stream_row(out, indent + sub->get_display_name(false), sub->get_description(), get_column_width(),
               get_right_column_width());
    return out.str();
}

std::string CliFormatter::make_option_opts(const CLI::Option* opt) const {
    std::string out;
    if (opt->get_type_size() != 0) {
        const std::string type = simplify_type_name(opt->get_type_name());
        if (!type.empty()) out += " " + type;
        if (opt->get_expected_max() == CLI::detail::expected_max_vector_size) out += " ...";
        // CLI11's own Formatter::make_option_opts appends this too; drop it
        // here and a required() option (e.g. -m, --model TEXT) reads as
        // optional in --help.
        if (opt->get_required()) out += " " + get_label("REQUIRED");
    }
    return out;
}

std::string CliFormatter::make_option(const CLI::Option* opt, bool is_positional) const {
    std::stringstream out;
    std::string left;
    if (is_positional) {
        // The usage line already says which arguments are required and which
        // are repeatable, so a positional row is just its name.
        left = "  " + make_option_name(opt, true);
    } else {
        // "-m, --model TEXT". A long-only option sits under the long column so
        // every long name lines up: "      --json".
        std::vector<std::string> short_names;
        std::vector<std::string> long_names;
        for (const std::string& name : CLI::detail::split(make_option_name(opt, false), ',')) {
            (name.rfind("--", 0) == 0 ? long_names : short_names).push_back(strip_flag_default(name));
        }
        left = short_names.empty() ? "      " : "  " + CLI::detail::join(short_names, ", ");
        if (!short_names.empty() && !long_names.empty()) left += ", ";
        left += CLI::detail::join(long_names, ", ");
        left += make_option_opts(opt);
    }
    stream_row(out, left, make_option_desc(opt), get_column_width(), get_right_column_width());
    return out.str();
}

}  // namespace wally
