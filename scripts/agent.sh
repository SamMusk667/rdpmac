#!/bin/sh
# Runs rdpmacd as a LaunchAgent in your login session.
#
# macOS checks Screen Recording and Accessibility against the process responsible for rdpmacd.
# Started from a shell that is the terminal app, or sshd over SSH; started by launchd as an agent
# it is rdpmacd itself, so the grants belong to rdpmacd alone and survive updates, because every
# install signs the binary with scripts/sign-dev.sh.
#
#   sh scripts/agent.sh install [-- ARGS]  install target/release/rdpmacd and start it; ARGS
#                                          replace the daemon's arguments, which are kept otherwise
#   sh scripts/agent.sh permissions        make macOS ask for both permissions for the agent
#   sh scripts/agent.sh start | stop | restart | status
#   sh scripts/agent.sh logs [-f]
#   sh scripts/agent.sh uninstall          remove the agent; logs and the TLS certificate stay
#
# Set RDPMAC_BIN to install another build, for example target/debug/rdpmacd.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
label="com.rdpmac.rdpmacd"
domain="gui/$(id -u)"
support="$HOME/Library/Application Support/rdpmac"
bin="$support/bin/rdpmacd"
plist="$HOME/Library/LaunchAgents/$label.plist"
logs_dir="$HOME/Library/Logs/rdpmac"
# rdpmacd writes daily files there itself; launchd.log only catches what it prints before that.
launchd_log="$logs_dir/launchd.log"
tab=$(printf '\t')

die() {
    echo "agent: $*" >&2
    exit 1
}

usage() {
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

loaded() {
    launchctl print "$domain/$1" >/dev/null 2>&1
}

# bootout can return before launchd has let go of the job, and bootstrap fails until it has.
unload() {
    loaded "$1" || return 0
    launchctl bootout "$domain/$1" 2>/dev/null || true
    i=0
    while loaded "$1"; do
        i=$((i + 1))
        [ "$i" -le 50 ] || die "launchd did not unload $1"
        sleep 0.2
    done
}

load() {
    launchctl bootstrap "$domain" "$1" ||
        die "launchd could not load $1; $(id -un) must be logged in at the Mac's screen, and" \
            "rdpmacd allowed under System Settings > General > Login Items"
}

job_field() {
    launchctl print "$domain/$1" 2>/dev/null | sed -n "s/^$tab$2 = //p"
}

# Writes the launchd job for label $2 to $1; the remaining arguments are the command line.
write_job() {
    out=$1
    job=$2
    shift 2
    rm -f "$out"
    plutil -create xml1 "$out"
    plutil -insert Label -string "$job" "$out"
    plutil -insert ProgramArguments -array "$out"
    for arg in "$@"; do
        plutil -insert ProgramArguments -string "$arg" -append "$out"
    done
    plutil -insert EnvironmentVariables -dictionary "$out"
    plutil -insert EnvironmentVariables.RDPMAC_LOG -string info "$out"
    plutil -insert EnvironmentVariables.RDPMAC_LOG_DIR -string "$logs_dir" "$out"
    plutil -insert RunAtLoad -bool YES "$out"
    plutil -insert ProcessType -string Interactive "$out"
    # Capture and input injection only work in the session at the Mac's screen.
    plutil -insert LimitLoadToSessionType -string Aqua "$out"
    plutil -insert StandardOutPath -string "$launchd_log" "$out"
    plutil -insert StandardErrorPath -string "$launchd_log" "$out"
}

newest_log() {
    ls -t "$logs_dir"/rdpmacd.*.log 2>/dev/null | head -n 1
}

# The installed agent's daemon arguments, one per line.
agent_args() {
    [ -f "$plist" ] || return 0
    n=$(plutil -extract ProgramArguments raw -o - "$plist" 2>/dev/null) || return 0
    i=1
    while [ "$i" -lt "$n" ]; do
        plutil -extract "ProgramArguments.$i" raw -o - "$plist"
        i=$((i + 1))
    done
}

install_agent() {
    src="${RDPMAC_BIN:-$root/target/release/rdpmacd}"
    [ -f "$src" ] || die "no $src; build it first: cargo build --release"
    if [ "$#" -eq 0 ]; then
        saved=$(agent_args)
        while IFS= read -r arg; do
            if [ -n "$arg" ]; then
                set -- "$@" "$arg"
            fi
        done <<EOF
$saved
EOF
    elif [ "$1" = "--" ]; then
        shift
    fi

    mkdir -p "$(dirname "$bin")" "$(dirname "$plist")" "$logs_dir"
    unload "$label"
    # Sign a copy so that a failed signature leaves the installed binary as it was.
    cp "$src" "$bin.new"
    sh "$root/scripts/sign-dev.sh" sign "$bin.new" || {
        rm -f "$bin.new"
        die "signing failed; the agent is stopped, start the old one with: sh scripts/agent.sh start"
    }
    mv -f "$bin.new" "$bin"

    write_job "$plist" "$label" "$bin" "$@"
    # Restart after a crash or a failed start, not after a clean exit.
    plutil -insert KeepAlive -dictionary "$plist"
    plutil -insert KeepAlive.SuccessfulExit -bool NO "$plist"
    plutil -insert ThrottleInterval -integer 10 "$plist"

    load "$plist"
    echo "installed $bin"
    sleep 2
    status_agent
    if tail -n 15 "$(newest_log)" 2>/dev/null | grep -q 'permission is missing'; then
        echo
        echo "rdpmacd lacks a permission; next: sh scripts/agent.sh permissions"
    fi
}

permissions() {
    [ -x "$bin" ] || die "install the agent first: sh scripts/agent.sh install"
    job="$label.permissions"
    job_plist="$support/$job.plist"
    unload "$job"
    write_job "$job_plist" "$job" "$bin" --request-permissions
    load "$job_plist"
    i=0
    until [ "$(job_field "$job" runs)" != "0" ] && [ "$(job_field "$job" state)" = "not running" ]; do
        i=$((i + 1))
        [ "$i" -le 50 ] || break
        sleep 0.2
    done
    unload "$job"
    rm -f "$job_plist"
    grep -h 'permission prompts shown' "$(newest_log)" 2>/dev/null | tail -n 1
    cat <<EOF

macOS has asked for Screen Recording and Accessibility on behalf of rdpmacd. In System Settings >
Privacy & Security, turn rdpmacd on under "Screen & System Audio Recording" and under
"Accessibility", then run: sh scripts/agent.sh restart
EOF
}

start_agent() {
    [ -f "$plist" ] || die "not installed; run: sh scripts/agent.sh install"
    if loaded "$label"; then
        launchctl kickstart "$domain/$label"
    else
        load "$plist"
    fi
}

restart_agent() {
    [ -f "$plist" ] || die "not installed; run: sh scripts/agent.sh install"
    if loaded "$label"; then
        launchctl kickstart -k "$domain/$label"
    else
        load "$plist"
    fi
}

stop_agent() {
    unload "$label"
    echo "stopped; it starts again at your next login unless you uninstall it"
}

status_agent() {
    if loaded "$label"; then
        pid=$(job_field "$label" pid)
        # launchd reports an exit code or, after a kill, the signal.
        last=$(job_field "$label" "last exit code")
        [ -n "$last" ] || last=$(job_field "$label" "last terminating signal")
        echo "agent: $(job_field "$label" state)${pid:+, pid $pid}; runs $(job_field "$label" runs)," \
            "last exit ${last:-none}"
    elif [ -f "$plist" ]; then
        echo "agent: installed, not loaded"
    else
        echo "agent: not installed"
        return 0
    fi
    echo "arguments: $(agent_args | tr '\n' ' ')"
    if [ -f "$bin" ]; then
        codesign -d -r- "$bin" 2>&1 | sed -n 's/^\(# \)\{0,1\}designated => /signature: /p'
    fi
    log=$(newest_log)
    if [ -n "$log" ]; then
        echo "log $log:"
        tail -n 5 "$log"
    fi
}

logs() {
    log=$(newest_log)
    [ -n "$log" ] || die "no log yet in $logs_dir"
    if [ "${1:-}" = "-f" ]; then
        tail -n 50 -F "$log"
    else
        tail -n 50 "$log"
    fi
}

uninstall_agent() {
    unload "$label"
    rm -f "$plist" "$bin" "$bin.new"
    rmdir "$(dirname "$bin")" 2>/dev/null || true
    echo "removed the agent; kept the logs in $logs_dir and the TLS certificate in $support"
    echo "rdpmacd's entries under Privacy & Security stay until you remove them there"
}

[ "$(id -u)" -ne 0 ] || die "run this as the user whose screen rdpmacd serves, not as root"

command="${1:-}"
[ "$#" -eq 0 ] || shift
case "$command" in
install) install_agent "$@" ;;
permissions) permissions ;;
start) start_agent ;;
stop) stop_agent ;;
restart) restart_agent ;;
status) status_agent ;;
logs) logs "$@" ;;
uninstall) uninstall_agent ;;
*) usage ;;
esac
