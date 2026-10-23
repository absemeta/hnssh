#!/usr/bin/env bash
set -Eeuo pipefail

fail() {
    printf 'production topology validation failed: %s\n' "$*" >&2
    exit 1
}

pass() {
    printf 'ok: %s\n' "$*"
}

if [[ ${HNSSH_DISPOSABLE_VM:-} != 1 ]]; then
    fail 'set HNSSH_DISPOSABLE_VM=1; this test changes port 22, systemd units, users, /etc, and /opt'
fi
[[ $(uname -s) == Linux ]] || fail 'Linux is required'
[[ ${EUID} -eq 0 ]] || fail 'run as root on a disposable VM'
[[ $(ps -p 1 -o comm= | tr -d ' ') == systemd ]] || fail 'systemd must be PID 1'

binary=${1:-}
[[ -n ${binary} ]] || fail "usage: $0 /path/to/linux/hnssh"
binary=$(readlink -f "$binary")
[[ -x ${binary} ]] || fail "HNSSH binary is not executable: $binary"

script_directory=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository_root=$(cd -- "$script_directory/.." && pwd)
socket_unit=$repository_root/deploy/systemd/hnssh.socket
service_unit=$repository_root/deploy/systemd/hnssh.service
[[ -f ${socket_unit} && -f ${service_unit} ]] || fail 'production systemd units are missing'

required_commands=(
    curl getent ip nc nsenter readlink runuser ssh ssh-keygen ssh-keyscan sshd
    ss stat systemctl systemd-analyze timeout useradd userdel
)
for command_name in "${required_commands[@]}"; do
    command -v "$command_name" >/dev/null || fail "required command is missing: $command_name"
done

validation_root=/run/hnssh-production-topology
admin_user=hnssh-validation-admin
admin_unit=hnssh-validation-admin.service
admin_unit_path=/run/systemd/system/$admin_unit
admin_port=22222
source_hold_pid=

show_failure_context() {
    systemctl status --no-pager hnssh.socket hnssh.service "$admin_unit" 2>/dev/null || true
    journalctl --no-pager -n 200 -u hnssh.socket -u hnssh.service -u "$admin_unit" 2>/dev/null || true
}

cleanup() {
    local status=$?
    trap - EXIT
    set +e
    if [[ ${status} -ne 0 ]]; then
        show_failure_context
    fi
    if [[ -n ${source_hold_pid} ]]; then
        kill "$source_hold_pid" 2>/dev/null || true
        wait "$source_hold_pid" 2>/dev/null || true
    fi
    systemctl stop hnssh.service hnssh.socket "$admin_unit" 2>/dev/null || true
    rm -rf /etc/systemd/system/hnssh.service.d
    rm -f /etc/systemd/system/hnssh.service /etc/systemd/system/hnssh.socket "$admin_unit_path"
    systemctl daemon-reload 2>/dev/null || true
    systemctl reset-failed 2>/dev/null || true
    userdel -r "$admin_user" 2>/dev/null || true
    userdel hnssh 2>/dev/null || true
    rm -rf /etc/hnssh /opt/hnssh "$validation_root"
    rm -f /tmp/hnssh-production-topology-host-marker /opt/hnssh-production-topology-write-probe
    exit "$status"
}
trap cleanup EXIT

listener_on_port() {
    ss -H -ltn "( sport = :$1 )" | grep -q .
}

wait_for_url() {
    local url=$1
    for _ in $(seq 1 100); do
        if curl --noproxy '*' --silent --fail --max-time 1 "$url" >/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

wait_for_admin() {
    for _ in $(seq 1 100); do
        if ssh -p "$admin_port" \
            -i "$validation_root/admin-key" \
            -o BatchMode=yes \
            -o ConnectTimeout=1 \
            -o StrictHostKeyChecking=no \
            -o UserKnownHostsFile=/dev/null \
            "$admin_user"@127.0.0.1 'test "$(id -un)" = hnssh-validation-admin' \
            >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

assert_property() {
    local property=$1
    local expected=$2
    local actual
    actual=$(systemctl show hnssh.service -p "$property" --value)
    [[ ${actual} == "$expected" ]] || fail "$property is $actual, expected $expected"
}

for path in /etc/hnssh /opt/hnssh /etc/systemd/system/hnssh.socket /etc/systemd/system/hnssh.service; do
    [[ ! -e ${path} ]] || fail "clean VM prerequisite failed; path already exists: $path"
done
getent passwd hnssh >/dev/null && fail 'clean VM prerequisite failed; hnssh user already exists'
getent passwd "$admin_user" >/dev/null && fail "clean VM prerequisite failed; $admin_user already exists"
listener_on_port "$admin_port" && fail "port $admin_port is already in use"
listener_on_port 9090 && fail 'port 9090 is already in use'

install -d -m 0700 "$validation_root"

# Establish and prove a separate key-only administrative route before freeing
# public port 22, mirroring the production cutover order.
ssh-keygen -A >/dev/null
useradd --create-home --shell /bin/bash "$admin_user"
passwd -d "$admin_user" >/dev/null
install -d -o "$admin_user" -g "$admin_user" -m 0700 "/home/$admin_user/.ssh"
ssh-keygen -q -t ed25519 -N '' -f "$validation_root/admin-key"
install -o "$admin_user" -g "$admin_user" -m 0600 \
    "$validation_root/admin-key.pub" "/home/$admin_user/.ssh/authorized_keys"
install -d -m 0755 /run/sshd
cat >"$validation_root/sshd_config" <<EOF
Port $admin_port
ListenAddress 127.0.0.1
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
PubkeyAuthentication yes
AuthorizedKeysFile .ssh/authorized_keys
StrictModes yes
UsePAM no
PidFile $validation_root/sshd.pid
HostKey /etc/ssh/ssh_host_ed25519_key
Subsystem sftp internal-sftp
EOF
sshd -t -f "$validation_root/sshd_config"
sshd_path=$(command -v sshd)
cat >"$admin_unit_path" <<EOF
[Unit]
Description=HNSSH production-topology validation administrative SSH
After=network.target

[Service]
Type=simple
ExecStartPre=$sshd_path -t -f $validation_root/sshd_config
ExecStart=$sshd_path -D -e -f $validation_root/sshd_config
Restart=on-failure
EOF
systemctl daemon-reload
systemctl start "$admin_unit"
wait_for_admin || fail 'fresh key-only administrative SSH connection failed'
pass 'separate key-only administrative SSH route works before port-22 cutover'

# Package installation may have activated the distribution OpenSSH socket.
# This test is explicitly disposable and has already proved its recovery route.
for stock_ssh_unit in ssh.socket ssh.service sshd.socket sshd.service; do
    if systemctl cat "$stock_ssh_unit" >/dev/null 2>&1; then
        systemctl disable --now "$stock_ssh_unit" >/dev/null 2>&1 || true
    fi
done
for _ in $(seq 1 50); do
    listener_on_port 22 || break
    sleep 0.1
done
listener_on_port 22 && fail 'public port 22 is still occupied after moving administrative SSH'

useradd --system --user-group --home-dir /nonexistent --shell /usr/sbin/nologin hnssh
install -d -o root -g root -m 0755 /opt/hnssh/releases/validation
install -o root -g root -m 0755 "$binary" /opt/hnssh/releases/validation/hnssh
ln -sfn /opt/hnssh/releases/validation /opt/hnssh/current
install -d -o root -g hnssh -m 0750 /etc/hnssh
ssh-keygen -q -t ed25519 -N '' -f /etc/hnssh/host-key
chown root:hnssh /etc/hnssh/host-key /etc/hnssh/host-key.pub
chmod 0640 /etc/hnssh/host-key
chmod 0644 /etc/hnssh/host-key.pub
install -o root -g root -m 0644 "$socket_unit" /etc/systemd/system/hnssh.socket
install -o root -g root -m 0644 "$service_unit" /etc/systemd/system/hnssh.service
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/hnssh.socket /etc/systemd/system/hnssh.service

[[ $(stat -c '%U:%G:%a' /etc/hnssh) == root:hnssh:750 ]] || fail 'unexpected /etc/hnssh ownership or mode'
[[ $(stat -c '%U:%G:%a' /etc/hnssh/host-key) == root:hnssh:640 ]] || fail 'unexpected private host-key ownership or mode'
[[ $(stat -c '%U:%G:%a' /etc/hnssh/host-key.pub) == root:hnssh:644 ]] || fail 'unexpected public host-key ownership or mode'
[[ $(stat -c '%U:%G:%a' /opt/hnssh/releases/validation/hnssh) == root:root:755 ]] || fail 'release binary is not a root-owned immutable input'
[[ $(readlink -f /opt/hnssh/current) == /opt/hnssh/releases/validation ]] || fail 'current release link is incorrect'
[[ $(getent passwd hnssh | cut -d: -f6) == /nonexistent ]] || fail 'HNSSH service identity has an unexpected home'
[[ $(getent passwd hnssh | cut -d: -f7) == /usr/sbin/nologin ]] || fail 'HNSSH service identity has a login shell'
pass 'service identity, release, and host-key boundaries are provisioned correctly'

missing_key=/etc/hnssh/host-key-missing
if timeout 5 runuser -u hnssh -- /opt/hnssh/current/hnssh \
    --host-key "$missing_key" \
    --listen-address 127.0.0.1:0 \
    --operations-address 127.0.0.1:0 \
    >"$validation_root/missing-key.out" 2>"$validation_root/missing-key.err"; then
    fail 'HNSSH unexpectedly started without a host key'
else
    missing_key_status=$?
fi
[[ ${missing_key_status} -ne 124 ]] || fail 'missing-host-key startup did not fail promptly'
[[ ! -e ${missing_key} ]] || fail 'HNSSH generated a replacement for a missing host key'
grep -Eqi 'LoadHostKey|host key' "$validation_root/missing-key.err" || fail 'missing-host-key failure was not actionable'
pass 'a missing host key fails startup without generating a replacement'

# Start only the socket, then prove that the first SSH arrival activates the
# asynchronous service through the native systemd descriptor contract.
systemctl start hnssh.socket
[[ $(systemctl is-active hnssh.socket) == active ]] || fail 'HNSSH socket did not start'
[[ $(systemctl is-active hnssh.service 2>/dev/null || true) == inactive ]] || fail 'HNSSH service started before socket traffic'
if curl --noproxy '*' --silent --fail --max-time 1 http://127.0.0.1:9090/livez >/dev/null 2>&1; then
    fail 'operations listener existed before service activation'
fi
ssh-keyscan -T 5 -t ed25519 127.0.0.1 >"$validation_root/key-initial" 2>/dev/null
wait_for_url http://127.0.0.1:9090/livez || fail 'liveness did not recover after socket activation'
wait_for_url http://127.0.0.1:9090/readyz || fail 'readiness did not recover after socket activation'
curl --noproxy '*' --silent --fail http://127.0.0.1:9090/metrics >"$validation_root/metrics"
[[ -s $validation_root/metrics ]] || fail 'metrics endpoint returned no body'
grep -q 'systemd-inherited SSH listener' < <(journalctl --no-pager -u hnssh.service) || fail 'service did not report the inherited listener'
pass 'native socket activation starts the production service and operations endpoints'

assert_property User hnssh
assert_property Group hnssh
assert_property NoNewPrivileges yes
assert_property CapabilityBoundingSet ''
assert_property AmbientCapabilities ''
assert_property ProtectSystem strict
assert_property ProtectHome yes
assert_property PrivateTmp yes
assert_property PrivateDevices yes
assert_property ProtectControlGroups yes
assert_property ProtectKernelModules yes
assert_property ProtectKernelTunables yes
assert_property ProtectKernelLogs yes
assert_property ProtectClock yes
assert_property ProtectHostname yes
assert_property ProtectProc invisible
assert_property ProcSubset pid
assert_property LockPersonality yes
assert_property MemoryDenyWriteExecute yes
assert_property RestrictRealtime yes
assert_property RestrictSUIDSGID yes
assert_property RestrictNamespaces yes
assert_property RemoveIPC yes

address_families=$(systemctl show hnssh.service -p RestrictAddressFamilies --value)
for family in AF_INET AF_INET6 AF_UNIX; do
    grep -qw "$family" <<<"$address_families" || fail "missing restricted address family: $family"
done
[[ $(wc -w <<<"$address_families") -eq 3 ]] || fail "unexpected address family allowance: $address_families"

pid=$(systemctl show hnssh.service -p MainPID --value)
[[ ${pid} =~ ^[1-9][0-9]*$ ]] || fail 'HNSSH service has no live process'
for capability_field in CapInh CapPrm CapEff CapBnd CapAmb; do
    capability_value=$(awk -v field="$capability_field:" '$1 == field { print $2 }' "/proc/$pid/status")
    [[ ${capability_value} == 0000000000000000 ]] || fail "$capability_field is not empty: $capability_value"
done

echo host-namespace-marker >/tmp/hnssh-production-topology-host-marker
if nsenter -t "$pid" -m -- test -e /tmp/hnssh-production-topology-host-marker; then
    fail 'PrivateTmp did not isolate the service mount namespace'
fi
if nsenter -t "$pid" -m -- touch /opt/hnssh-production-topology-write-probe 2>/dev/null; then
    fail 'ProtectSystem=strict still allowed a write under /opt'
fi
if nsenter -t "$pid" -m -- test -r "/home/$admin_user/.ssh/authorized_keys"; then
    fail 'ProtectHome did not hide the administrative home from HNSSH'
fi
systemd-analyze security --no-pager hnssh.service >"$validation_root/security.txt"
grep -q 'Overall exposure level' "$validation_root/security.txt" || fail 'systemd security analysis did not complete'
pass 'the running process is non-root, capability-free, read-only, and mount-isolated'

socket_lines=$(ss -H -ltn '( sport = :22 )')
grep -Eq '(^|[[:space:]])(\*|\[::\]|0\.0\.0\.0):22([[:space:]]|$)' <<<"$socket_lines" || fail 'port 22 is not listening publicly'
operations_lines=$(ss -H -ltn '( sport = :9090 )')
grep -Eq '127\.0\.0\.1:9090([[:space:]]|$)' <<<"$operations_lines" || fail 'operations listener is not on IPv4 loopback'
if grep -Eq '(\*|0\.0\.0\.0|\[::\]):9090([[:space:]]|$)' <<<"$operations_lines"; then
    fail 'operations listener is publicly exposed'
fi
admin_lines=$(ss -H -ltn "( sport = :$admin_port )")
grep -Eq "127\\.0\\.0\\.1:$admin_port([[:space:]]|$)" <<<"$admin_lines" || fail 'administrative route is not restricted to loopback in the validation VM'

non_loopback_address=$(ip -o -4 addr show scope global | awk '{ split($4, parts, "/"); print parts[1]; exit }')
[[ -n ${non_loopback_address} ]] || fail 'validation VM has no non-loopback IPv4 address'
if curl --noproxy '*' --silent --fail --max-time 1 "http://$non_loopback_address:9090/livez" >/dev/null 2>&1; then
    fail 'operations listener was reachable through the non-loopback address'
fi
ssh-keyscan -4 -T 5 -t ed25519 127.0.0.1 >"$validation_root/key-v4" 2>/dev/null
ssh-keyscan -6 -T 5 -t ed25519 ::1 >"$validation_root/key-v6" 2>/dev/null
ssh-keyscan -4 -T 5 -t ed25519 "$non_loopback_address" >"$validation_root/key-public" 2>/dev/null
cut -d' ' -f2- "$validation_root/key-v4" >"$validation_root/key-v4-material"
cut -d' ' -f2- "$validation_root/key-v6" >"$validation_root/key-v6-material"
cut -d' ' -f2- "$validation_root/key-public" >"$validation_root/key-public-material"
cmp "$validation_root/key-v4-material" "$validation_root/key-v6-material"
cmp "$validation_root/key-v4-material" "$validation_root/key-public-material"
pass 'port 22 is dual-stack/public while operations and administration stay restricted'

set +e
{ sleep 1; printf q; } | TERM=xterm-256color timeout 20 ssh -tt \
    -o BatchMode=yes \
    -o PreferredAuthentications=none \
    -o PubkeyAuthentication=no \
    -o PasswordAuthentication=no \
    -o KbdInteractiveAuthentication=no \
    -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    visitor@127.0.0.1 \
    >"$validation_root/browsing.out" 2>"$validation_root/browsing.err"
pipeline_status=("${PIPESTATUS[@]}")
set -e
ssh_status=${pipeline_status[1]}
[[ ${ssh_status} -ne 124 ]] || fail 'real SSH browsing smoke test timed out'
grep -aFq HNSSH "$validation_root/browsing.out" || fail 'real SSH client did not render the HNSSH interface'
pass 'a stock OpenSSH client enters and exits an anonymous PTY browsing session'

cut -d' ' -f2- "$validation_root/key-initial" >"$validation_root/key-before-restart"
fingerprint_before=$(ssh-keygen -lf "$validation_root/key-before-restart" | awk '{ print $2 }')
pid_before=$pid
systemctl restart hnssh.service
wait_for_url http://127.0.0.1:9090/readyz || fail 'readiness did not recover after restart'
pid_after=$(systemctl show hnssh.service -p MainPID --value)
[[ ${pid_before} != "$pid_after" ]] || fail 'service PID did not change across restart'
ssh-keyscan -T 5 -t ed25519 127.0.0.1 >"$validation_root/key-after-restart-full" 2>/dev/null
cut -d' ' -f2- "$validation_root/key-after-restart-full" >"$validation_root/key-after-restart"
cmp "$validation_root/key-before-restart" "$validation_root/key-after-restart"
fingerprint_after=$(ssh-keygen -lf "$validation_root/key-after-restart" | awk '{ print $2 }')
[[ ${fingerprint_before} == "$fingerprint_after" ]] || fail 'host fingerprint changed across service restart'
[[ $(systemctl is-active hnssh.socket) == active ]] || fail 'systemd socket did not remain active across service restart'
pass "stable host fingerprint survives restart: $fingerprint_after"

# Prove that HNSSH receives the accepted socket's real peer address. With a
# one-connection source cap, a second 127.0.0.2 connection must be rejected,
# while 127.0.0.3 must still reach the SSH transport.
install -d -m 0755 /etc/systemd/system/hnssh.service.d
cat >/etc/systemd/system/hnssh.service.d/source-validation.conf <<EOF
[Service]
ExecStart=
ExecStart=/opt/hnssh/current/hnssh --host-key /etc/hnssh/host-key --operations-address 127.0.0.1:9090 --max-source-connections 1
EOF
systemctl daemon-reload
systemctl restart hnssh.service
wait_for_url http://127.0.0.1:9090/livez || fail 'service did not recover for source-address validation'
{ sleep 15; } | nc -s 127.0.0.2 127.0.0.1 22 >"$validation_root/source-first.out" &
source_hold_pid=$!
for _ in $(seq 1 30); do
    grep -q 'SSH-2.0' "$validation_root/source-first.out" 2>/dev/null && break
    sleep 0.1
done
grep -q 'SSH-2.0' "$validation_root/source-first.out" || fail 'first transformed source did not reach SSH'
timeout 3 nc -s 127.0.0.2 127.0.0.1 22 </dev/null >"$validation_root/source-same.out" || true
timeout 3 nc -s 127.0.0.3 127.0.0.1 22 </dev/null >"$validation_root/source-other.out" || true
grep -Eqi 'busy|capacity' "$validation_root/source-same.out" || fail 'same-source connection was not rejected at its configured cap'
grep -q 'SSH-2.0' "$validation_root/source-other.out" || fail 'different source did not retain independent admission'
kill "$source_hold_pid" 2>/dev/null || true
wait "$source_hold_pid" 2>/dev/null || true
source_hold_pid=
rm -rf /etc/systemd/system/hnssh.service.d
systemctl daemon-reload
systemctl restart hnssh.service
wait_for_url http://127.0.0.1:9090/readyz || fail 'service did not recover after source-address validation'
pass 'socket activation preserves real peer addresses for source admission'

wait_for_admin || fail 'administrative SSH route failed after HNSSH activation and restart'
pass 'administrative access remains available independently of public HNSSH'
operating_system=$(awk -F= '$1 == "PRETTY_NAME" { value = substr($0, index($0, "=") + 1); gsub(/^"|"$/, "", value); print value }' /etc/os-release)
printf '\nHNSSH production topology validation passed on %s with systemd %s.\n' \
    "$operating_system" \
    "$(systemd --version | awk 'NR == 1 { print $2 }')"
