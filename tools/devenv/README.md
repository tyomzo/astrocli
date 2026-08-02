# Bench build environment

Copied out of a session scratchpad on 2026-08-02 so that the recipe in
[`docs/HANDOVER-2026-08-02.md`](../../docs/HANDOVER-2026-08-02.md) survives `/tmp` being cleaned.

Neither `libgphoto2-dev` nor `libudev-dev` is installed system-wide on the bench machine and there is
no root there, so the `-dev` halves are staged by hand. The runtime `.so.6` libraries **are** present
system-wide (gvfs pulls them in), so this is a build-time problem only.

| file | what it is |
|---|---|
| `gp-env.sh` | `PKG_CONFIG_SYSROOT_DIR`, `LIBCLANG_PATH` and `BINDGEN_EXTRA_CLANG_ARGS` for the extracted libgphoto2 headers and llvm-20's libclang |
| `libudev.pc` | a hand-written stub; only the `.pc` is missing, `libudev.so.1` is present |
| `dev-field-tls.yaml.example` | the bench field-node config, with the operator's TLS paths and site |

`gp-env.sh` contains **absolute paths into the staging tree it was written for**. If that tree is
gone the prefix has to be re-extracted; the script documents what it expects.

## Building for real hardware

```bash
set -a; . tools/devenv/gp-env.sh; set +a
export PKG_CONFIG_PATH="$PKG_CONFIG_PATH:$PWD/tools/devenv"
cargo build -p astroctl-field --bin astroctl-field --features libgphoto2,serialport
```

**Always verify before restarting the node.** A bare `cargo build` succeeds and produces a binary
that refuses both devices — the camera fails at startup, the mount only on `connect`. Both of these
must print `0`:

```bash
strings target/debug/astroctl-field | grep -c "has no serial port implementation"
strings target/debug/astroctl-field | grep -c "has no libgphoto2 support"
```

The PWA is embedded with `include_dir!` at compile time, so a frontend change needs
`npm run build`, a `touch crates/astroctl-field/src/pwa.rs`, and this rebuild.

Stop a running node by PID from `ss -tlnp`. Not `pkill -f`: the pattern matches the invoking shell's
own command line and kills the caller.
