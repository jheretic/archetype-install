# archetype-install

Interactive TUI installer for Archetype Linux. A single Rust/ratatui binary
that gathers install parameters, generates `systemd-repart` partition
definitions under `/run`, and drives `systemd-repart` to clone the running
A/B immutable-`/usr` layout onto a chosen disk.

See `PLAN.md` for the full design.

## repart template runtime contract

The seven fixed partition definitions (ESP plus the A/B `usr` verity triples)
are owned by **archetype-build**, which ships them into the image at
`/usr/lib/repart.sysinstall.d/`:

```
00-efi.conf  10-usr.conf  20-usr-verity.conf  30-usr-verity-sig.conf
40-usr-b.conf  50-usr-verity-b.conf  60-usr-verity-sig-b.conf
```

At runtime the installer reads those files from `/usr/lib/repart.sysinstall.d/`
(see `RUNTIME_TEMPLATE_DIR` in `src/repart/generate.rs`) so the image is the
single source of truth for the install partition layout. If a file is missing
or unreadable, the installer falls back to a byte-identical copy embedded at
build time (`src/repart/templates/*.conf`, via `include_str!`).

The configurable `root` / `swap` / `home` definitions (`70`/`80`/`90`) are
**not** read from disk: the installer generates them from the user's sizing
choices. archetype-build ships `70-root.conf`, `80-swap.conf`, and
`90-home.conf` as documentation/reference of the intended contract
(`root`: Btrfs + `Encrypt=key-file+tpm2`; `swap`; `home`), which the generated
output mirrors.

### Sync obligation

The embedded `src/repart/templates/*.conf` copies MUST stay byte-identical to
archetype-build's `mkosi.extra/usr/lib/repart.sysinstall.d/*.conf` for the same
seven fixed files. They are the dev/testing fallback only; the image copy wins
at runtime, so a drift is silent on a real install but will surface in tests
and in dev runs off-image. When the canonical templates change in
archetype-build, copy them into `src/repart/templates/` in the same change set.

A quick check (run from the parent of both repos):

```
for f in 00-efi 10-usr 20-usr-verity 30-usr-verity-sig \
         40-usr-b 50-usr-verity-b 60-usr-verity-sig-b; do
  diff -q archetype-build/mkosi.extra/usr/lib/repart.sysinstall.d/$f.conf \
          archetype-install/src/repart/templates/$f.conf
done
```
