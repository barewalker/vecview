# contrib

Situational helper scripts that are **not part of the normal build**. For a
regular install, ignore this directory and use `cargo install vecview` (see the
top-level [README](../README.md)).

## `install-vv.sh` — cross-glibc install for split build/run environments

For a setup where you **build** vv inside a newer distro but **run** it on an
older host that shares the same `~/.cargo/bin`. The canonical example:

- build inside an **Arch** distrobox/podman container (glibc 2.4x)
- run on the **Ubuntu** host (glibc 2.39), with `/home` (and thus `~/.cargo/bin`)
  shared between the two

A plain `cargo install` inside the container links against the container's newer
glibc, so the resulting `~/.cargo/bin/vv` fails to start on the host with
`version 'GLIBC_2.4x' not found`. This script uses
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) to target an old
glibc (default 2.35) from inside the container, producing one binary that runs on
both.

### Prerequisites

- `cargo install cargo-zigbuild`
- `zig` on `PATH`, or unpacked under `~/.local/share/zig-*/` (the script finds the
  newest one automatically). Pinned `zig` is preferred over a `/tmp` copy, which a
  reboot would wipe.

### Usage

```bash
contrib/install-vv.sh                 # target glibc 2.35, install to ~/.cargo/bin/vv
GLIBC_TARGET=2.39 contrib/install-vv.sh
```

It prints the built binary's max required glibc — make sure it is `<=` your host's.
