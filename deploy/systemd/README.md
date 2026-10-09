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
