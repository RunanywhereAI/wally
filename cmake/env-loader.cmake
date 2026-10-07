# A repo-local .env (copy .env.example) feeds the configure environment, so a
# fresh clone can pick a channel with one file instead of exporting variables
# by hand. Values stay out of the repo and the command line; environment
# variables still win, which is how CI keeps passing its own. Included before
# CMakeLists.txt reads $ENV{WALLY_*}.
set(_wally_env_file "${CMAKE_SOURCE_DIR}/.env")
if(EXISTS "${_wally_env_file}")
    file(STRINGS "${_wally_env_file}" _wally_env_lines)
    foreach(_line IN LISTS _wally_env_lines)
        # Only KEY=value lines; comments and blanks are skipped.
        if(_line MATCHES "^[A-Za-z_][A-Za-z0-9_]*=")
            string(REGEX REPLACE "^([A-Za-z_][A-Za-z0-9_]*)=(.*)$" "\\1" _key "${_line}")
            string(REGEX REPLACE "^([A-Za-z_][A-Za-z0-9_]*)=(.*)$" "\\2" _value "${_line}")
            string(STRIP "${_value}" _value)
            # Tolerate KEY="value" as well as KEY=value.
            string(REGEX REPLACE "^\"(.*)\"$" "\\1" _value "${_value}")
            if(NOT DEFINED ENV{${_key}})
                set(ENV{${_key}} "${_value}")
            endif()
        endif()
    endforeach()
endif()