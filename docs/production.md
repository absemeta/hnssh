# Production port-22 runbook

This runbook installs HNSSH on one Linux VPS using native systemd socket
activation. The socket unit owns public TCP port 22 and passes one listener to a
single non-root HNSSH process. HNSSH accepts the connections itself, so SSH host
identity and real peer-address admission remain end to end.

Commands assume a systemd distribution and root access. Adapt the administrative
OpenSSH unit name (`ssh` or `sshd`) and firewall commands to the target image.
Do not perform the cutover without a second operator connection and a tested,
network-independent provider console or rescue environment.

## 1. Prove the recovery and administration paths

Before changing a listener or firewall:

1. Enable MFA and recovery codes on the VPS provider account.
2. Open the provider's serial/VNC console or bootable rescue environment. Verify
   that it still works when guest networking and `sshd` are unavailable. A web
   console that is only an SSH wrapper is not sufficient.
3. Record how to mount the root filesystem, disable `hnssh.socket`, restore the
   firewall, and edit `sshd_config` from that environment.
4. Keep the current administrative SSH connection open throughout the cutover.

Move OpenSSH to a private management address or to a high port restricted by
both provider and host firewalls to trusted source CIDRs. A high port alone is
not an access control. A temporary migration can configure both ports:

```text
# /etc/ssh/sshd_config.d/20-administration.conf
Port 22
Port 22222
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
```

Validate before reload, allow TCP 22222 only from the administrative source or
private network, and test a **fresh external connection** while the old session
remains open:

```sh
sshd -t
systemctl reload ssh.service             # often sshd.service on RPM systems
ssh -p 22222 admin@ADMIN_HOST
```

After the fresh connection succeeds, remove `Port 22`, run `sshd -t`, reload,
and test port 22222 again. Some distributions activate OpenSSH through
`ssh.socket`; move or disable that socket as well. Confirm that nothing owns
port 22 before enabling HNSSH:

```sh
ss -ltnp '( sport = :22 )'
```

## 2. Create the service identity and stable host key

Create one dedicated identity with no login shell and no home directory:

```sh
useradd --system --user-group --home-dir /nonexistent \
  --shell /usr/sbin/nologin hnssh
```

Generate the HNSSH Ed25519 key once, outside every release. The root-owned
configuration directory and service-only group read bit let HNSSH read but not
replace the key:

```sh
install -d -o root -g hnssh -m 0750 /etc/hnssh
umask 077
ssh-keygen -q -t ed25519 -N '' -f /etc/hnssh/host-key
chown root:hnssh /etc/hnssh/host-key /etc/hnssh/host-key.pub
chmod 0640 /etc/hnssh/host-key
chmod 0644 /etc/hnssh/host-key.pub
ssh-keygen -lf /etc/hnssh/host-key.pub
```

Record the SHA256 fingerprint through an operator-controlled channel. Store an
encrypted, access-controlled off-host backup of the private key and test its
restore procedure. Never put the private key in a release, source repository,
VM image, or general-purpose administrative `sshd` configuration. A missing key
must stop startup; do not generate one in `ExecStartPre` or a recovery script.

The first HNSSH connection receives a normal host-authenticity prompt. This
application key is intentionally distinct from the key previously presented by
administrative OpenSSH on port 22. After cutover, an unexpected fingerprint
change is an incident, not a prompt to delete `known_hosts` blindly.

## 3. Install the binary and units

Install a release as a root-owned, immutable input. The `current` link is the
interface used by the supplied unit; release switching and rollback are covered
by the deployment milestone.

```sh
install -d -o root -g root -m 0755 /opt/hnssh/releases/0.1.0
install -o root -g root -m 0755 hnssh /opt/hnssh/releases/0.1.0/hnssh
ln -sfn /opt/hnssh/releases/0.1.0 /opt/hnssh/current

install -o root -g root -m 0644 deploy/systemd/hnssh.socket /etc/systemd/system/
install -o root -g root -m 0644 deploy/systemd/hnssh.service /etc/systemd/system/
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/hnssh.socket \
  /etc/systemd/system/hnssh.service
```

The socket is explicitly one dual-stack listener (`BindIPv6Only=both`). The
service accepts the socket with `Accept=no`, runs as `hnssh`, has an empty
capability set, and can write only to its private temporary filesystem. `AF_UNIX`
is retained solely for libc resolver integration; public traffic remains
`AF_INET`/`AF_INET6`.

Start both units explicitly so the loopback operations endpoint exists before
the first visitor arrives:

```sh
systemctl enable --now hnssh.socket hnssh.service
systemctl status hnssh.socket hnssh.service
```

HNSSH consumes exactly one descriptor only when `LISTEN_PID` identifies its own
process and `LISTEN_FDS=1`. Without a valid activation environment, the same
binary uses `--listen-address`/`HNSSH_LISTEN_ADDRESS` for development. A partial
or malformed activation environment fails startup rather than silently choosing
a different production listener.

## 4. Verify before declaring the cutover healthy

On the VPS, verify operations are loopback-only and readiness is healthy. A
Hacker News outage may report `ready (upstream degraded)` and remains ready by
design.

```sh
curl --fail http://127.0.0.1:9090/livez
curl --fail http://127.0.0.1:9090/readyz
curl --fail http://127.0.0.1:9090/metrics >/dev/null
ss -ltnp '( sport = :22 or sport = :9090 )'
```

Only port 22 should be generally public. Port 22222 (or the private management
address) must remain source-restricted, and port 9090 must show only a loopback
listener. Verify the process identity, empty effective capability set, and
sandbox:

```sh
systemctl show hnssh.service -p User -p Group -p NoNewPrivileges \
  -p CapabilityBoundingSet -p AmbientCapabilities -p RestrictAddressFamilies
pid=$(systemctl show hnssh.service -p MainPID --value)
grep -E '^(Uid|Gid|Cap(Inh|Prm|Eff|Bnd|Amb)):' "/proc/$pid/status"
systemd-analyze security hnssh.service
```

From fresh external IPv4 and IPv6 clients, compare the independently recorded
fingerprint and complete an anonymous browsing journey:

```sh
ssh-keyscan -4 -p 22 APP_HOST 2>/dev/null | ssh-keygen -lf -
ssh-keyscan -6 -p 22 APP_HOST 2>/dev/null | ssh-keygen -lf -
ssh -4 -p 22 APP_HOST
ssh -6 -p 22 APP_HOST
```

Navigate the feed, open an entry, and quit. Restart the service (not the socket),
repeat the fingerprint and browsing checks, and confirm the fingerprint is
unchanged:

```sh
systemctl restart hnssh.service
```

Also confirm a fresh administrative connection still works before closing the
original operator session.

## 5. Break-glass recovery

If HNSSH, its socket, the firewall, or OpenSSH is broken:

1. Keep any surviving administrative session open. Otherwise enter the tested
   provider serial/VNC console or rescue environment.
2. Stop public HNSSH acceptance:
   `systemctl disable --now hnssh.socket hnssh.service`.
3. Restore the last known-good OpenSSH configuration, validate with `sshd -t`,
   restore its port-22 listener if needed, and repair provider and host firewall
   rules before reloading OpenSSH.
4. Verify a fresh administrative connection before ending console access.
5. For an HNSSH key loss, restore the backed-up `/etc/hnssh/host-key`, ownership,
   and modes above. Do **not** silently regenerate it. Recheck the recorded
   fingerprint before re-enabling HNSSH.
6. For a unit or binary failure, retain the key, select the prior root-owned
   binary, run `systemd-analyze verify`, then start the socket and service and
   repeat every local and external health check.

A provider console is a break-glass dependency, not the normal administrative
route. Test this recovery sequence on a clean disposable VM before the public
cutover and after material OS or firewall changes.
