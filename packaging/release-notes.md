tedge-dot ships as **two interchangeable packages**, each containing the same
`/usr/bin/tedge-dot` binary, the same systemd unit and the same
`/etc/tedge/plugins/ot/` config layout. They speak the same
[OT Connector Contract](https://github.com/thin-edge/tedge-dot/blob/main/doc/contract/),
so a config, a flow or a cloud integration built against one works unchanged against
the other. Install **one or the other** — they declare each other as conflicting.

| Package | Implementation | Pick it when |
|---|---|---|
| `tedge-dot-rs` | Rust (`impl/rust/`) | Default. Richest protocol support, one static binary, no shared-library dependencies. |
| `tedge-dot-c` | C (`impl/c/`) | Small or old devices: ~25x smaller, a **glibc 2.17** floor (Debian 8 / RHEL 7 era), and it additionally ships the **PROFIBUS-DP** connector. |

### Install

Both packages are published to the thin-edge.io **community** repository, which
also carries their `tedge-parameter-plugin` dependency (it maps the Cumulocity
*Parameters* tab onto point writes). With that repository set up
([instructions](https://thin-edge.github.io/thin-edge.io/install/#community-plugins)):

```sh
# Debian / Ubuntu
sudo apt-get install -y tedge-dot-rs                         # or tedge-dot-c
sudo systemctl status tedge-dot

# RPM distros
sudo dnf install tedge-dot-rs                                # or tedge-dot-c
```

Or download the package for your architecture from the assets below. The
package manager still resolves `tedge-parameter-plugin` from the community
repository, so set that up first:

```sh
sudo apt-get install -y ./tedge-dot-rs_*_linux_amd64.deb    # or ./tedge-dot-c_*_amd64.deb
sudo dnf install ./tedge-dot-rs_*_linux_amd64.rpm           # or ./tedge-dot-c_*_amd64.rpm
```

> **Alpine:** the attached `.apk` files carry a version string apk-tools rejects
> (`apk version -c` refuses both this project's tag format and the snapshot
> form), so `apk add` will not install them. Use the tarball on Alpine until
> that is fixed — see TODO.md.

Or grab a tarball and run the binary directly — it doubles as a one-shot
read/write CLI. The `tedge-dot-c` tarball carries the default configs to start from:

```sh
./tedge-dot read -c config-defaults/modbus.toml --json
```

`SHA256SUMS` covers every asset in this release.

### Upgrading from a release before the split

The package formerly called `tedge-dot` is now **`tedge-dot-rs`**. Both packages
`Replaces:` the old name, so installing either over an existing `tedge-dot`
works and keeps your `/etc/tedge/plugins/ot/` configuration:

```sh
sudo apt-get install -y ./tedge-dot-rs_*_linux_amd64.deb
```

Two caveats:

- `apt install tedge-dot` no longer resolves — `tedge-dot` is now a *virtual*
  package provided by both, so apt cannot choose. Install by the real name.
  Scripts and runbooks using the old name need updating.
- A plain `apt upgrade` will **not** migrate an existing `tedge-dot` install; it
  stays on the old package name and receives no further updates until you
  install one of the new ones explicitly.

### Notes

- On a fresh install the service starts with **no devices configured**. Add
  `[[device]]` sections under `/etc/tedge/plugins/ot/`, or copy a demo config
  from `/usr/share/tedge-dot/demo/`, then restart the service.
- The connector is a dumb driver: the device-side **flows** that map its
  envelopes onto the thin-edge data model are deployed active into
  `/etc/tedge/mappers/c8y/flows/`, with the opt-in alarm/event flows in
  `/usr/share/tedge-dot/flows/`. See `flows/README.md`.
- `tedge-dot-c` runtime dependencies (Debian/Ubuntu names): `libmodbus5`,
  `libmosquitto1`, `libcjson1`; open62541 is statically linked. It is
  cross-compiled with zig against the glibc 2.17 floor.
- `tedge-dot-rs` omits the PROFIBUS connector: its serial dependency has a
  native libudev build script that does not cross-compile. Build from source on
  Linux with `cargo build --manifest-path impl/rust/Cargo.toml --features profibus`, or use `tedge-dot-c`.
- Where the two implementations differ in behaviour, see the parity table in
  `impl/c/README.md`.
- On **Alpine**, the two packages are not mutually exclusive by metadata: apk
  expresses conflicts differently and the `conflicts` field is not carried into
  the `.apk`. Installing one over the other will collide on
  `/usr/bin/tedge-dot` — remove the first with `apk del` before installing the
  second, rather than forcing the overwrite.
