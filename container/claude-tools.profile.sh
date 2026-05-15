# /etc/profile.d/claude-tools.sh
#
# Prepend bind-mounted claude-watch CLI dirs to PATH when they exist.
#
# When the operator bind-mounts ~/repos/claude-watch into the container
# (as the example compose does), the Python CLIs under tools/ become
# discoverable on PATH without baking them into the image. Each tool dir
# is checked individually -- a missing dir is a silent no-op, not an
# error, so this also works in a stripped-down `docker run` without any
# bind mounts.
#
# Sourced by login shells via /etc/profile, and by interactive non-login
# bash shells via the /etc/bash.bashrc append in the Dockerfile.
#
# POSIX sh syntax (no arrays) so /bin/sh-mode profile loading works.

for _claude_tools_dir in \
    "${HOME}/repos/claude-watch/tools/session-task" \
    "${HOME}/repos/claude-watch/tools/claude-event" \
    "${HOME}/repos/claude-watch/tools/obligations" \
    "${HOME}/repos/claude-watch/tools/claude-watch-ack" \
    "${HOME}/repos/claude-watch/tools/claude-watch-dispatch"
do
    if [ -d "${_claude_tools_dir}" ]; then
        case ":${PATH}:" in
            *":${_claude_tools_dir}:"*) ;;
            *) PATH="${_claude_tools_dir}:${PATH}" ;;
        esac
    fi
done
unset _claude_tools_dir
export PATH
