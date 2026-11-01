#!/bin/sh
# Install the Switchboard client on this machine (macOS or Linux). No sudo:
# everything goes under your home directory.
#
#   curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh -s -- --invite 'sbi1...'
#
# It downloads the release binary for this OS and architecture with the
# release's SHA256SUMS, checks the checksum, and runs `switchboard install`,
# which puts the binary at ~/.local/bin/switchboard (the command itself) and
# installs the bridge's user service (launchd on macOS, systemd on Linux)
# without starting it. Running it again upgrades in place, and moves an
# installation from before 0.3.1 (which had the command in ~/bin); once
# installed, `switchboard upgrade` does the same.
#
# Then it starts `switchboard init` on the terminal (read from /dev/tty, so
# `curl | sh` works), which joins this machine to a server with an invite,
# sets up the server here, or both. Options after `sh -s --` go to init (see
# `switchboard init --help`): `--invite INVITE` joins with that invite, and
# `--invite` alone asks for it. Without a terminal, init runs only when given
# options or SWITCHBOARD_INVITE, and then asks nothing.
#
# On a machine that is already set up, a run without options or an invite
# only upgrades: it restarts the bridge if it was running, and leaves it
# stopped if it was not.
#
# Environment:
#   SWITCHBOARD_VERSION  the release to install, such as 0.2.0 (default: the latest)
#   SWITCHBOARD_INVITE   an invite to set this machine up with after installing
#   GITHUB_TOKEN         a token that can read the repository, while it is private
#   SWITCHBOARD_REPO     owner/name on GitHub (default: krish-k7/switchboard)
#   SWITCHBOARD_GITHUB, SWITCHBOARD_GITHUB_API
#                        the GitHub web and API addresses (for mirrors and tests)
set -eu

repo=${SWITCHBOARD_REPO:-krish-k7/switchboard}
web=${SWITCHBOARD_GITHUB:-https://github.com}
api=${SWITCHBOARD_GITHUB_API:-https://api.github.com}
version=${SWITCHBOARD_VERSION:-}
token=${GITHUB_TOKEN:-}
invite=${SWITCHBOARD_INVITE:-}
tmp=
spinner=

# The visual language of the switchboard command. Every line starts with a
# two-cell mark column (a check mark in the accent color for something
# finished, a red cross for a failure, a yellow ! for a warning, blanks
# otherwise), so text starts in one column; text is the terminal's own
# color, commands bold and on lines of their own. A terminal gets the title
# and progress; color unless NO_COLOR is set or TERM is dumb; Unicode marks
# unless the locale says another encoding.
tty= color= color_err=
if [ -t 1 ]; then
    tty=1
    if [ -z "${NO_COLOR+set}" ] && [ "${TERM:-}" != dumb ]; then color=1; fi
fi
if [ -t 2 ] && [ -z "${NO_COLOR+set}" ] && [ "${TERM:-}" != dumb ]; then color_err=1; fi
done_mark=+ fail_mark=x frames='| / - \'
case ${LC_ALL:-${LC_CTYPE:-${LANG:-UTF-8}}} in
    *[Uu][Tt][Ff]-8* | *[Uu][Tt][Ff]8*) done_mark=✓ fail_mark=✗ frames='⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏' ;;
esac

say() { printf '%s\n' "$*"; }

# text COLOR TONE TEXT: TEXT in the text column, in the SGR tone TONE (0
# normal, 1 bold); commands quoted in backticks are bold. COLOR is empty for
# plain output.
text() {
    if [ -n "$1" ]; then
        esc=$(printf '\033')
        printf '  %s\n' "$3" |
            sed "s/\`\([^\`]*\)\`/$esc[1m\1$esc[0;$2m/g; s/^  /  $esc[$2m/; s/\$/$esc[0m/"
    else
        printf '  %s\n' "$3" | sed 's/`//g'
    fi
}

# command_line COLOR COMMAND: a command to copy, alone on its line, four
# cells in from the text, in bold.
command_line() {
    if [ -n "$1" ]; then
        printf '      \033[1m%s\033[0m\n' "$2"
    else
        printf '      %s\n' "$2"
    fi
}

# lead COLOR TEXT: TEXT, with a command it ends with (`cmd`.) moved to a
# line of its own.
lead() {
    last=$(printf '%s\n' "$2" | sed -n 's/^.* `\([^`]*\)`\.$/\1/p')
    if [ -n "$last" ]; then
        text "$1" 0 "$(printf '%s\n' "$2" | sed 's/ `[^`]*`\.$//')"
        command_line "$1" "$last"
    else
        text "$1" 0 "$2"
    fi
}

# done_line TEXT: a finished step.
done_line() {
    if [ -n "$color" ]; then
        printf '\033[36m%s\033[0m %s\n' "$done_mark" "$*"
    else
        printf '%s %s\n' "$done_mark" "$*"
    fi
}

# warn_line TEXT: something to look at.
warn_line() {
    if [ -n "$color" ]; then
        printf '\033[1;33m!\033[0m %s\n' "$*"
    else
        printf '! %s\n' "$*"
    fi
}

# fail SENTENCE [FIX]: stop with the command's error shape on stderr: the
# cross and what went wrong, then what to do about it.
fail() {
    settle
    if [ -n "$color_err" ]; then
        printf '\033[1;31m%s\033[0m \033[1m%s\033[0m\n' "$fail_mark" "$1" >&2
    else
        printf '%s %s\n' "$fail_mark" "$1" >&2
    fi
    [ $# -lt 2 ] || lead "$color_err" "To fix, $2" >&2
    exit 1
}

# spin TEXT: show TEXT with a spinner until settle; on a terminal without
# color, TEXT as a plain line instead; nothing without a terminal.
spin() {
    if [ -n "$color" ]; then
        printf '\033[?25l'
        (
            trap - EXIT
            set -f
            while :; do
                for frame in $frames; do
                    # Stop if the installer is gone, so no orphan draws on.
                    kill -0 $$ 2>/dev/null || exit 0
                    printf '\r\033[36m%s\033[0m %s\033[K' "$frame" "$1"
                    sleep 0.08 2>/dev/null || sleep 1
                done
            done
        ) &
        spinner=$!
    elif [ -n "$tty" ]; then
        printf '  %s ...\n' "$1"
    fi
}

# settle: stop the spinner, clear its line, and show the cursor again.
settle() {
    [ -n "$spinner" ] || return 0
    kill "$spinner" 2>/dev/null || true
    wait "$spinner" 2>/dev/null || true
    spinner=
    printf '\r\033[K\033[?25h'
}

cleanup() {
    settle
    [ -z "$tmp" ] || rm -rf "$tmp"
}

# fetch URL FILE [HEADER...]: download URL to FILE, failing on HTTP errors;
# the downloader's message goes to $tmp/fetch-error. With a token, curl reads
# the Authorization header from the private config file $auth (never from
# its command line, which other users can see in ps) and sends it only to the
# first host, never to a host it is redirected to.
fetch() {
    url=$1 file=$2
    shift 2
    count=$#
    if [ "$downloader" = curl ]; then
        for header; do set -- "$@" -H "$header"; done
        shift "$count"
        [ -z "$auth" ] || set -- "$@" --config "$auth"
        curl -fsSL --proto '=https,http' --proto-redir '=https' --retry 2 "$@" -o "$file" "$url" 2> "$tmp/fetch-error"
    else
        for header; do set -- "$@" "--header=$header"; done
        shift "$count"
        wget -q "$@" -O "$file" "$url" 2> "$tmp/fetch-error"
    fi
}

# fetch_failed STATUS URL SENTENCE FIX: stop after a failed fetch. When the
# server answered with an error, SENTENCE (with the HTTP status) and FIX say
# what it means; anything else is a network failure.
fetch_failed() {
    # Interrupted: the trap has the last word.
    [ "$1" -le 128 ] || { cleanup; exit 130; }
    if [ "$downloader-$1" = curl-22 ] || [ "$downloader-$1" = wget-8 ]; then
        status=$(sed -n 's/.*returned error: *\([0-9][0-9]*\).*/\1/p' "$tmp/fetch-error" | tail -n 1)
        fail "$3${status:+ (the server answered $status)}." "$4"
    fi
    host=$(printf '%s\n' "$2" | sed 's|^[A-Za-z]*://||; s|[/?].*||')
    reason=$(sed -n 's/^curl: ([0-9]*) //p' "$tmp/fetch-error" | tail -n 1 |
        awk '{ print tolower(substr($0, 1, 1)) substr($0, 2) }')
    fail "Could not reach $host${reason:+ ($reason)}." \
        "check this machine's internet connection, then run this again."
}

# asset_url RELEASE_JSON NAME: the API download address of one release asset.
# The asset object lists "url" (.../releases/assets/ID) before "name" and
# before its nested uploader object, so it stays on one line once braces
# become newlines.
asset_url() {
    tr -d '\r\n' < "$1" | tr '{}' '\n\n' | grep "\"name\": *\"$2\"" |
        sed -n 's|.*"url": *"\([^"]*/releases/assets/[0-9][0-9]*\)".*|\1|p' | head -n 1
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        openssl dgst -sha256 -r "$1" | awk '{print $1}'
    fi
}

# reported FILE: the error a switchboard command printed as JSON in FILE, as
# " (error)", or nothing.
reported() {
    reason=$(sed -n 's/.*"error": *"\(.*\)"[^"]*$/\1/p' "$1" | head -n 1 | sed 's/\\"/"/g; s/\\\\/\\/g')
    [ -z "$reason" ] || printf ' (%s)' "${reason%.}"
}

# has_up VERSION_LINE: whether a `switchboard --version` line ("switchboard
# 0.3.0") names 0.3 or later, which has `up` and `down`. Asked of the
# version, not of `up --help`, which 0.2 also answers (with its general help).
has_up() {
    up_version=${1#switchboard }
    up_version=${up_version#v}
    up_major=${up_version%%.*}
    up_minor=${up_version#*.}
    up_minor=${up_minor%%[!0-9]*}
    case $up_major in '' | *[!0-9]*) return 1 ;; esac
    case $up_minor in '' | *[!0-9]*) return 1 ;; esac
    [ "$up_major" -gt 0 ] || [ "$up_minor" -ge 3 ]
}

# at_least VERSION_LINE MAJOR MINOR PATCH: whether a `switchboard --version`
# line names that version or a later one (a pre-release of it counts as
# earlier only when it is the same version, which is close enough here).
at_least() {
    v=${1#switchboard }
    v=${v#v}
    v=${v%%[-+]*}
    v_major=${v%%.*}
    v_rest=${v#*.}
    v_minor=${v_rest%%.*}
    v_patch=${v_rest#*.}
    for part in "$v_major" "$v_minor" "$v_patch"; do
        case $part in '' | *[!0-9]*) return 1 ;; esac
    done
    [ "$v_major" -ne "$2" ] && { [ "$v_major" -gt "$2" ]; return; }
    [ "$v_minor" -ne "$3" ] && { [ "$v_minor" -gt "$3" ]; return; }
    [ "$v_patch" -ge "$4" ]
}

# The text every Switchboard program from 0.3.1 on carries (install.rs,
# PROGRAM_SIGNATURE): what marks the file at ~/.local/bin/switchboard as one
# an installer put there.
signature='switchboard-program-signature:7f3b9c1e5a2d4f60'

# Whether this process can reach a terminal.
has_terminal() { (exec < /dev/tty) 2>/dev/null && [ -t 2 ]; }

main() {
    # Everything after `sh -s --` is for switchboard init; only check here
    # what must be known before downloading.
    asks=
    previous=
    for argument; do
        case $argument in
            -h | --help)
                say "Usage: curl -fsSL .../install.sh | sh [-s -- INIT-OPTIONS]"
                say ""
                say "Installs Switchboard for this user, then runs switchboard init with the"
                say "options given: it joins this machine to a server with an invite, sets up"
                say "the server here, or both (switchboard init --help lists its options once"
                say "installed)."
                exit 0 ;;
        esac
        [ "$previous" != --invite ] || case $argument in -*) [ "$argument" = - ] || asks=1 ;; esac
        previous=$argument
    done
    [ "$previous" != --invite ] || asks=1

    # The title, as switchboard init shows it; init is told not to repeat it.
    titled=
    if [ -n "$tty" ]; then
        titled=1
        if [ -n "$color" ]; then
            printf '\n  \033[1mSwitchboard\033[0m\n  Connects Claude Code chats across your machines.\n\n'
        else
            printf '\n  Switchboard\n  Connects Claude Code chats across your machines.\n\n'
        fi
    fi

    if [ -n "$asks" ] && [ -z "$invite" ] && ! has_terminal; then
        fail "The --invite option needs a terminal to ask for the invite on." \
            "put the invite after it (--invite sbi1...), or set SWITCHBOARD_INVITE, and run this again."
    fi
    # init's options, checked here so a typo fails before the download.
    unknown="leave it out, or use one of switchboard init's options (switchboard init --help lists them)."
    value=
    for argument; do
        if [ -n "$value" ]; then
            value=
            case $argument in
                -*) [ "$previous_option" = --invite ] ||
                        fail "$previous_option needs a value." "put the value after it, or leave it out." ;;
                *) continue ;;
            esac
        fi
        case ${argument%%=*} in
            --invite) previous_option=--invite
                case $argument in *=*) ;; *) value=1 ;; esac ;;
            --name | --machine | --owner | --tls | --domain | --email | --server-name | --port | --config)
                previous_option=$argument; case $argument in *=*) ;; *) value=1 ;; esac ;;
            --join | --plugin | --no-plugin | --no-start | --replace | --server | --yes | -y | --json) ;;
            -) [ "${previous_option:-}" = --invite ] || fail "Unknown option: -." "$unknown" ;;
            *) fail "Unknown option: $argument." "$unknown" ;;
        esac
    done
    [ -z "$value" ] || [ "$previous_option" = --invite ] ||
        fail "$previous_option needs a value." "put the value after it, or leave it out."

    case $(uname -s) in
        Darwin) os=darwin os_name=macOS ;;
        Linux) os=linux os_name=Linux ;;
        *) fail "Switchboard does not run on $(uname -s)." "install it on a macOS or Linux machine." ;;
    esac
    case $(uname -m) in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=arm64 ;;
        *) fail "Switchboard has no build for $(uname -m) processors." \
            "install it on a 64-bit Intel or ARM machine (x86_64 or arm64)." ;;
    esac
    # An x86_64 shell under Rosetta on Apple silicon gets the native binary.
    if [ "$os-$arch" = darwin-x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
        arch=arm64
    fi
    asset=switchboard-$os-$arch
    case $version in
        '') tag= ;;
        v*) tag=$version ;;
        *) tag=v$version ;;
    esac

    # The tools, checked before anything is downloaded.
    if command -v curl >/dev/null 2>&1; then
        downloader=curl
    elif command -v wget >/dev/null 2>&1; then
        downloader=wget
        # wget would resend the token to the storage host it is redirected to.
        [ -z "$token" ] || fail "Downloading from a private repository needs curl." \
            "install curl, then run this again."
    else
        fail "Downloading Switchboard needs curl or wget, and this machine has neither." \
            "install curl, then run this again."
    fi
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1 &&
        ! command -v openssl >/dev/null 2>&1; then
        fail "Checking the download needs sha256sum, shasum, or openssl, and this machine has none of them." \
            "install one of them (sha256sum comes with coreutils), then run this again."
    fi
    if [ -n "$token" ]; then
        # GitHub tokens are letters, digits, and underscores; anything that
        # could break out of the quoted line in curl's config is refused.
        case $token in
            *[!A-Za-z0-9_.-]*) fail "GITHUB_TOKEN has characters a GitHub token does not have." \
                "copy the token again, without quotes or spaces, then run this again." ;;
        esac
    fi

    # A set-up machine run without options or an invite is only upgraded.
    upgrade_only=
    if [ -f "$HOME/.config/switchboard/bridge.json" ] && [ $# -eq 0 ] && [ -z "$invite" ]; then
        upgrade_only=1
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t switchboard)
    trap cleanup EXIT
    trap 'cleanup; exit 130' HUP INT TERM

    spin "Downloading Switchboard${tag:+ ${tag#v}} for $os_name ($arch)"
    if [ -n "$tag" ]; then release_name="release $tag of $repo"; else release_name="latest release of $repo"; fi
    auth=
    if [ -n "$token" ]; then
        # A private repository: find the assets through the API, then download
        # them as octet streams (curl drops the token on the storage redirect).
        auth=$tmp/curl-auth
        # printf is a shell builtin, so the token is in no process's arguments.
        (umask 077 && printf 'header = "Authorization: Bearer %s"\n' "$token" > "$auth")
        if [ -n "$tag" ]; then
            release=$api/repos/$repo/releases/tags/$tag
            missing="There is no release $tag in $repo, or GITHUB_TOKEN cannot read it"
            check="check SWITCHBOARD_VERSION, and that the token can read $repo, then run this again."
        else
            release=$api/repos/$repo/releases/latest
            missing="There is no published release in $repo, or GITHUB_TOKEN cannot read it"
            check="check that the token can read $repo, then run this again."
        fi
        fetch "$release" "$tmp/release.json" "Accept: application/vnd.github+json" ||
            fetch_failed $? "$release" "$missing" "$check"
        binary_url=$(asset_url "$tmp/release.json" "$asset")
        sums_url=$(asset_url "$tmp/release.json" SHA256SUMS)
        other="install another release with SWITCHBOARD_VERSION, or leave it unset for the latest."
        [ -n "$binary_url" ] || fail "The $release_name has no build for this machine ($asset)." "$other"
        [ -n "$sums_url" ] || fail "The $release_name has no SHA256SUMS, so the download cannot be checked." "$other"
        fetch "$binary_url" "$tmp/$asset" "Accept: application/octet-stream" ||
            fetch_failed $? "$binary_url" "Could not download $asset from the $release_name" "run this again."
        fetch "$sums_url" "$tmp/SHA256SUMS" "Accept: application/octet-stream" ||
            fetch_failed $? "$sums_url" "Could not download SHA256SUMS from the $release_name" "run this again."
    else
        if [ -n "$tag" ]; then
            base=$web/$repo/releases/download/$tag
            check="check that release $tag exists; while the repository is private, set GITHUB_TOKEN to a token that can read it."
        else
            base=$web/$repo/releases/latest/download
            check="while the repository is private, set GITHUB_TOKEN to a token that can read it, then run this again."
        fi
        fetch "$base/$asset" "$tmp/$asset" ||
            fetch_failed $? "$base/$asset" "Could not download $asset from the $release_name" "$check"
        fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
            fetch_failed $? "$base/SHA256SUMS" "Could not download SHA256SUMS from the $release_name" \
                "run this again; if it fails again, install another release with SWITCHBOARD_VERSION."
    fi

    expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name {print $1; exit}' "$tmp/SHA256SUMS")
    [ -n "$expected" ] || fail "The release's SHA256SUMS lists no $asset, so the download cannot be checked; nothing was installed." \
        "install another release with SWITCHBOARD_VERSION, or leave it unset for the latest."
    actual=$(sha256 "$tmp/$asset")
    [ "$actual" = "$expected" ] || fail "The download of $asset does not match the release's checksum, so nothing was installed." \
        "run this again; if it fails the same way, the release itself is damaged."
    chmod 700 "$tmp/$asset"
    if ! installed=$("$tmp/$asset" --version 2> "$tmp/version-error"); then
        why=$(tail -n 1 "$tmp/version-error")
        fail "The downloaded program does not run on this machine${why:+ ($why)}."
    fi
    case $installed in
        "switchboard "*) shown="Switchboard ${installed#switchboard }" ;;
        *) shown=$installed ;;
    esac

    # Whether the bridge runs now, asked before the new service is installed
    # (`switchboard install` installs it without starting it).
    bridge=
    if [ -n "$upgrade_only" ]; then
        if "$tmp/$asset" status --json > "$tmp/status.json" 2>/dev/null; then
            if grep -q '^  "pid": *[0-9]' "$tmp/status.json"; then bridge=running
            elif grep -q '^  "down": *true' "$tmp/status.json"; then bridge=down
            else bridge=stopped; fi
        else
            bridge=unknown
        fi
    fi

    settle
    done_line "Downloaded $shown and checked it against the release's checksums"
    if at_least "$installed" 0 3 1; then
        # 0.3.1 and later: the command is the program, in ~/.local/bin.
        program=$HOME/.local/bin/switchboard
        "$tmp/$asset" install --json > "$tmp/install.out" ||
            fail "Could not install $shown$(reported "$tmp/install.out")." "fix that, then run this again."
        done_line "Installed the switchboard command in ~/.local/bin"
        if grep -q '^  "removed":' "$tmp/install.out"; then
            done_line "Removed what earlier versions installed in ~/bin and ~/.local/share/switchboard"
        fi
        if grep -q '^  "plugin_unknown":' "$tmp/install.out"; then
            warn_line "Kept ~/bin/switchboard as a link to the command: Claude Code couldn't say which Switchboard plugin it has, and one older than 0.3.1 runs that path"
            text "$color" 0 "If its plugin is current (\`claude plugin list\`), the next upgrade removes the link once Claude Code answers."
        elif grep -q '^  "plugin_link":' "$tmp/install.out"; then
            warn_line "Kept ~/bin/switchboard as a link to the command: this machine's Claude Code plugin is older than 0.3.1 and runs that path"
            text "$color" 0 "Update the plugin with \`claude plugin marketplace update switchboard\`; the next upgrade removes the link."
        fi
        if grep -q '^  "left":' "$tmp/install.out"; then
            warn_line "Left ~/bin/switchboard alone: Switchboard didn't write it"
            text "$color" 0 "If ~/bin comes before ~/.local/bin on your PATH, it runs instead of switchboard."
        fi
        if grep -q '^  "cleanup_failed":' "$tmp/install.out"; then
            warn_line "Couldn't remove everything earlier versions left; \`switchboard install\` says what"
        fi
        case ":$PATH:" in
            *":$HOME/.local/bin:"*) command=switchboard ;;
            *) command=$program ;;
        esac
    else
        # An earlier release, pinned: it writes the command to ~/bin and
        # refuses a program or link of a later release in its way, so those
        # (and only files an installer wrote) are moved aside first, and put
        # back if it fails.
        program=$HOME/bin/switchboard
        moved= linked=
        if [ -f "$HOME/.local/bin/switchboard" ] && [ ! -L "$HOME/.local/bin/switchboard" ] &&
            grep -q -F "$signature" "$HOME/.local/bin/switchboard" 2>/dev/null; then
            mv "$HOME/.local/bin/switchboard" "$tmp/moved-program" && moved=1
        fi
        if [ -L "$HOME/bin/switchboard" ] &&
            [ "$(readlink "$HOME/bin/switchboard")" = "$HOME/.local/bin/switchboard" ]; then
            rm -f "$HOME/bin/switchboard" && linked=1
        fi
        if ! "$tmp/$asset" install > "$tmp/install.out"; then
            if [ -n "$moved" ] && [ ! -e "$HOME/.local/bin/switchboard" ] && [ ! -L "$HOME/.local/bin/switchboard" ]; then
                mv "$tmp/moved-program" "$HOME/.local/bin/switchboard"
            fi
            if [ -n "$linked" ] && [ ! -e "$HOME/bin/switchboard" ] && [ ! -L "$HOME/bin/switchboard" ]; then
                ln -s "$HOME/.local/bin/switchboard" "$HOME/bin/switchboard"
            fi
            fail "Could not install $shown, so the installed version was left as it was." \
                "follow the message above, then run this again."
        fi
        done_line "Installed the switchboard command in ~/bin"
        case ":$PATH:" in
            *":$HOME/bin:"* | *":$HOME/.local/bin:"*) command=switchboard ;;
            *) command=$program ;;
        esac
    fi
    # Releases before 0.3 call up and down start and stop (0.3 and later
    # still take those names, so an unreadable version gets them too).
    up=start down=stop
    if has_up "$installed"; then up=up down=down; fi
    if [ -n "$upgrade_only" ]; then
        case $bridge in
            running)
                spin "Restarting the bridge"
                "$program" $down --json > "$tmp/restart.out" 2>&1 ||
                    fail "Could not stop the bridge to restart it on $shown$(reported "$tmp/restart.out")." \
                        "run \`$command doctor\` to see why, then \`$command $down\` and \`$command $up\`."
                "$program" $up --json > "$tmp/restart.out" 2>&1 ||
                    fail "The bridge stopped but did not start again$(reported "$tmp/restart.out")." \
                        "run \`$command doctor\` to see why, then \`$command $up\`."
                settle
                done_line "Restarted the bridge on $shown"
                ;;
            down)
                done_line "This machine is already set up"
                text "$color" 0 "This machine is down, so it stays down. To bring it back online, run"
                command_line "$color" "$command up"
                ;;
            stopped)
                done_line "This machine is already set up"
                text "$color" 0 "The bridge was not running, so it stays stopped. Start it with"
                command_line "$color" "$command $up"
                ;;
            *)
                warn_line "Could not tell whether the bridge is running"
                text "$color" 0 "To run the new version, restart it with"
                command_line "$color" "$command $down"
                command_line "$color" "$command $up"
                ;;
        esac
        say ""
        text "$color" 0 "Next time, this does all of it in one step:"
        command_line "$color" "$command upgrade"
        return 0
    fi
    [ -z "$titled" ] || export SWITCHBOARD_TITLE_SHOWN=1
    if has_terminal; then
        # init takes over the terminal; the download is no longer needed.
        cleanup
        trap - EXIT HUP INT TERM
        exec "$program" init "$@" < /dev/tty
    fi
    if [ $# -gt 0 ] || [ -n "$invite" ]; then
        # No terminal: init asks nothing, and stops on a missing answer,
        # naming the option. SWITCHBOARD_INVITE reaches it in the environment.
        cleanup
        trap - EXIT HUP INT TERM
        exec "$program" init "$@" < /dev/null
    fi
    say ""
    text "$color" 1 "Next"
    text "$color" 0 "In a terminal, run"
    command_line "$color" "$command init"
    text "$color" 0 "It joins this machine to a server with an invite, sets up the server here, or both."
}

main "$@"
