# Orange Pi 5B image

A flashable SD-card image that boots straight into Pulsar: a living-room game-streaming box.

**Status: prepared, not yet built or booted.** Verify it on the board before it is published
(checklist below).

## Why a separate image

Hardware video decode on the RK3588 needs three board-specific pieces together:

- Rockchip's vendor kernel (`/dev/mpp_service`, `/dev/rga`). Mainline kernels have no MPP.
- The MPP/RGA userspace libraries.
- An ffmpeg built with the `rkmpp` decoders.

The generic `linux-arm64` release links Ubuntu's own libavcodec, which has no rkmpp, so on
the board it would decode in software. Shipping a whole image keeps that stack consistent.

## What is in it

- **Base:** [ubuntu-rockchip](https://github.com/Joshua-Riek/ubuntu-rockchip) v2.4.0, *Ubuntu
  22.04 server* for `orangepi-5b`. That is the Rockchip 5.10 kernel, panfork Mesa, and the
  rockchip-multimedia PPA (MPP 1.5, RGA 2.2, rkmpp ffmpeg). It is the same stack the
  development Pi runs.
  - The project was archived in 2026 and its PPAs are frozen, but releases and packages are
    still served.
  - The maintained successor is Armbian's `orangepi5b` with the `vendor` 6.1 kernel. There
    MPP, RGA, ffmpeg-rockchip and the GPU userspace would have to be built from source.
- **Pulsar:** a `.deb` built against that PPA's libavcodec (`build-deb.sh`), so
  `pulsar-render` gets `h264_rkmpp`/`hevc_rkmpp`.
- **Kiosk:** the flow at boot is:
  1. tty1 autologin as `pulsar`.
  2. `startx` with openbox (no desktop environment).
  3. `pulsar-kiosk` forces game mode, fullscreen and the dark theme in the UI's localStorage,
     then starts Pulsar. It restarts Pulsar if it exits.

  `xinitrc` asks for 1920×1080 at 120 Hz and falls back to the display's default.
- **Accounts:**
  - `pulsar` has no password and is not a sudoer.
  - Admin is the image's own `ubuntu` account over SSH; cloud-init forces a password change
    at first login.

## Build

On arm64 Linux. The CI runner `ubuntu-22.04-arm` works, and so does the Pi itself.

```bash
# 1. The .deb (Ubuntu 22.04 arm64; INSTALL_DEPS=1 sets up the PPA, toolchains, dev packages)
INSTALL_DEPS=1 scripts/opi5-image/build-deb.sh
# 2. The image (root: loop devices + chroot)
sudo scripts/opi5-image/build-image.sh target/release/bundle/deb/Pulsar_*_arm64.deb pulsar-opi5b.img.xz
```

CI: run the **Orange Pi 5B image** workflow (`.github/workflows/opi5-image.yml`) by hand. It
uploads `pulsar-opi5b.img.xz` as an artifact.

Flash it with Raspberry Pi Imager / balenaEtcher, or:

```bash
xz -dc pulsar-opi5b.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync status=progress
```

## Verify before publishing

1. The image builds in CI. Check that the root partition grows and that `customize.sh` runs
   to `customize: done`.
2. First boot reaches Pulsar fullscreen, dark, in game mode, with no login prompt.
3. `xrandr` shows 1920×1080 at 120 Hz on a 120 Hz display.
4. A stream decodes in hardware. The overlay's decoder row should read `hevc_rkmpp (HW)` /
   `h264_rkmpp (HW)`, and `pulsar-render` must not show a software fallback.
5. Audio plays (native libopus + SDL3 → PulseAudio), and a controller works.
6. It goes online on `relay.pulsardesk.com` and shows up on the LAN.
7. SSH as `ubuntu` asks for the forced password change.

The size must also stay under GitHub's 2 GiB per-asset limit before the image is attached to
releases. The base server image is ~0.9 GB compressed.
