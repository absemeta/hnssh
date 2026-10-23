# systemd units

These units implement HNSSH's production listener topology. `hnssh.socket` owns
public TCP port 22 and passes one dual-stack listening socket to a single
asynchronous, non-root `hnssh.service` process. The service receives the real
TCP peer address and needs no bind capability.

Do not enable the socket until administrative OpenSSH has been moved and tested
using the production runbook in [`../../docs/production.md`](../../docs/production.md).
The paths in `hnssh.service` assume an immutable release selected through
`/opt/hnssh/current` and a separately provisioned key at
`/etc/hnssh/host-key`.

Run [`../validate-production-topology.sh`](../validate-production-topology.sh)
only on a disposable Linux VM. It performs a destructive port-22 cutover with a
test administrative route and validates the actual units, socket activation,
peer-address preservation, sandbox, loopback operations endpoint, anonymous SSH
journey, and stable host fingerprint. CI runs the same gate on its disposable
Ubuntu VM.
