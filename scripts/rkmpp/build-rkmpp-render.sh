#!/usr/bin/env bash
# Build `pulsar-render-rkmpp`: the Rockchip variant of the Linux renderer for the GENERIC
# linux-arm64 bundles (AppImage / .deb / .rpm), and stage it into src-tauri/resources/.
#
# Why: the generic arm64 build links Ubuntu 22.04's libavcodec58, which has no rkmpp decoders,
# so on an RK3588 board (Orange Pi 5, Rock 5, …) the stream decoded in SOFTWARE unless the
# board ran the separate opi5 .deb. This builds the SAME pulsar-render source a second time,
# linked against a private, minimal ffmpeg-rockchip (h264/hevc/av1_rkmpp → DRM_PRIME, the
# zero-copy EGL path the opi5 build uses), Rockchip MPP and dav1d, and ships them NEXT TO the
# normal renderer. Nothing changes for anyone else:
#   - The app only runs the variant on a board with an MPP device node, when the normal
#     renderer does NOT already decode on rkmpp (a system ffmpeg with rkmpp keeps winning), AND
#     when the variant's own `--probe` decodes a canned keyframe on an rkmpp decoder
#     (src-tauri/src/process.rs, `rkmpp_render_bin`). x64, non-Rockchip arm64 boards and
#     mainline-kernel Rockchip boards keep `pulsar-render`.
#   - Its libs live in their own dir (resources/pulsar-rkmpp/, sonames libavcodec.so.61 & co.,
#     different from the bundle's .58) and only the variant's LD_LIBRARY_PATH names it.
#
# Output (staged only when everything built and checked out):
#   src-tauri/resources/pulsar-render-rkmpp
#   src-tauri/resources/pulsar-rkmpp/{libavcodec.so.61,libavformat.so.61,libavutil.so.59,
#                                     librockchip_mpp.so.1,libdav1d.so.7}
#
#   INSTALL_DEPS=1 scripts/rkmpp/build-rkmpp-render.sh   # CI / fresh machine: apt build deps
#   scripts/rkmpp/build-rkmpp-render.sh                  # deps present (e.g. on the Pi)
#
# Env: RKMPP_WORK=<dir> (sources + prefix, default target/rkmpp-build),
#      CARGO_TARGET_DIR (default target/rkmpp-<pinned SHAs>: kept apart so the normal
#      renderer's ffmpeg-sys build, against the system libav, is never mixed with this one, and
#      named after the pins because ffmpeg-sys-next's build script prints no rerun-if-changed:
#      a reused target dir would keep bindings generated against the OLD ffmpeg headers after a
#      pin bump. CI passes a fresh dir under $RUNNER_TEMP instead).
#
# Licences: ffmpeg-rockchip is built --enable-gpl --enable-version3 (GPLv3, like Pulsar);
# Rockchip MPP is Apache-2.0, dav1d BSD-2-Clause (both GPLv3-compatible).
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"

[ "$(uname -m)" = aarch64 ] || { echo "build-rkmpp-render: run this on arm64 Linux" >&2; exit 1; }

# Pinned sources (bump deliberately; both are what Jellyfin ships for Rockchip).
# nyanmisaka/mpp `jellyfin-mpp`: MPP 1.3.9+ with the fixes ffmpeg-rockchip expects.
MPP_REPO=https://github.com/nyanmisaka/mpp.git
MPP_REF=848b885f95587994a53216e435f650041a894e31
# nyanmisaka/ffmpeg-rockchip `7.1` branch: FFmpeg 7.1.x (libavcodec 61) + rkmpp/rkrga.
FFMPEG_REPO=https://github.com/nyanmisaka/ffmpeg-rockchip.git
FFMPEG_REF=8b4385e985c0a50bf7d3f96cf3cb6d0b63c53e6c
# dav1d 1.5.4: the variant's software AV1 decoder (libdav1d), the same floor the generic
# renderer has through Ubuntu's libavcodec58 (FFmpeg's native av1 decoder is hwaccel-only).
DAV1D_REPO=https://github.com/videolan/dav1d.git
DAV1D_REF=54706fc6bc0cdecab7e9593974a4039cc038fca7
pins="${MPP_REF:0:12}-${FFMPEG_REF:0:12}-${DAV1D_REF:0:12}"

work="${RKMPP_WORK:-$repo/target/rkmpp-build}"
prefix="$work/prefix"
target_dir="${CARGO_TARGET_DIR:-$repo/target/rkmpp-$pins}"
res="$repo/src-tauri/resources"
jobs="$(nproc)"

if [ "${INSTALL_DEPS:-0}" = 1 ]; then
	sudo=""
	[ "$(id -u)" = 0 ] || sudo="sudo"
	export DEBIAN_FRONTEND=noninteractive
	$sudo apt-get update
	$sudo apt-get install -y --no-install-recommends git ca-certificates build-essential cmake \
		pkg-config libdrm-dev patchelf binutils clang libclang-dev meson ninja-build
fi

fetch() { # <repo> <sha> <dir>
	if [ "$(git -C "$3" rev-parse HEAD 2>/dev/null || true)" = "$2" ]; then
		return
	fi
	rm -rf "$3"
	git init -q "$3"
	git -C "$3" fetch -q --depth 1 "$1" "$2"
	git -C "$3" checkout -q FETCH_HEAD
}

mkdir -p "$work"
stamp="$prefix/.pulsar-rkmpp-$MPP_REF-$FFMPEG_REF-$DAV1D_REF"
if [ ! -f "$stamp" ]; then
	rm -rf "$prefix"

	# 1. Rockchip MPP (librockchip_mpp.so.1). No tests/demos.
	fetch "$MPP_REPO" "$MPP_REF" "$work/mpp"
	cmake -S "$work/mpp" -B "$work/mpp/build" \
		-DCMAKE_BUILD_TYPE=Release \
		-DCMAKE_INSTALL_PREFIX="$prefix" \
		-DCMAKE_INSTALL_LIBDIR=lib \
		-DBUILD_SHARED_LIBS=ON \
		-DBUILD_TEST=OFF
	cmake --build "$work/mpp/build" -j "$jobs"
	cmake --install "$work/mpp/build"

	# 2. dav1d (libdav1d.so.7), shared: the staging step below picks it up like MPP.
	fetch "$DAV1D_REPO" "$DAV1D_REF" "$work/dav1d"
	rm -rf "$work/dav1d/build"
	meson setup "$work/dav1d/build" "$work/dav1d" \
		--prefix="$prefix" --libdir=lib --buildtype=release --default-library=shared \
		-Denable_tools=false -Denable_tests=false -Denable_examples=false -Denable_docs=false
	ninja -C "$work/dav1d/build"
	ninja -C "$work/dav1d/build" install

	# 3. ffmpeg-rockchip, only what pulsar-render links (ffmpeg-sys-next features: avcodec +
	#    avformat, + avutil): the video decoders it probes (the rkmpp ones; as the in-renderer
	#    software fallback, used by the runtime zero-output fallback and for codecs MPP rejects,
	#    the native h264/hevc decoders plus libdav1d for AV1, because FFmpeg's native `av1`
	#    decodes only through hwaccels, which are off), the stock demuxers (incl. the SDP/RTP
	#    demux behind PULSAR_LINUX_DEMUX=ffmpeg), and no OTHER external library:
	#    --disable-autodetect, then explicitly pthreads (autodetect covers the thread libs too,
	#    and single-threaded software decode falls behind at 1080p60 on the A76 cores), libdrm,
	#    rkmpp and dav1d.
	fetch "$FFMPEG_REPO" "$FFMPEG_REF" "$work/ffmpeg"
	(
		cd "$work/ffmpeg"
		PKG_CONFIG_PATH="$prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" ./configure \
			--prefix="$prefix" \
			--enable-gpl --enable-version3 \
			--enable-shared --disable-static \
			--disable-programs --disable-doc --disable-debug \
			--disable-autodetect --enable-pthreads --enable-libdrm --enable-rkmpp \
			--enable-libdav1d \
			--disable-avdevice --disable-avfilter --disable-swscale --disable-swresample \
			--disable-postproc \
			--disable-encoders --disable-muxers --disable-devices --disable-filters \
			--disable-decoders \
			--enable-decoder=h264,hevc,libdav1d,h264_rkmpp,hevc_rkmpp,av1_rkmpp \
			--disable-hwaccels
		make -j "$jobs"
		make install
	)
	touch "$stamp"
fi

# The rkmpp decoders and libdav1d must really be in this libavcodec, and it must be threaded
# (configure silently drops a component whose deps are missing).
for d in h264_rkmpp hevc_rkmpp libdav1d; do
	grep -q "^#define CONFIG_$(echo "$d" | tr a-z A-Z)_DECODER 1" "$work/ffmpeg/config_components.h" ||
		{ echo "build-rkmpp-render: ffmpeg-rockchip was configured without $d" >&2; exit 1; }
done
grep -q '^#define HAVE_PTHREADS 1' "$work/ffmpeg/config.h" ||
	{ echo "build-rkmpp-render: ffmpeg-rockchip was configured without pthreads" >&2; exit 1; }

# 4. The renderer, same source, against that prefix. FFMPEG_DIR makes ffmpeg-sys-next bindgen
#    against these headers and link these libs; -rpath-link lets ld resolve libavcodec's own
#    librockchip_mpp; the RUNPATH ($ORIGIN/pulsar-rkmpp) is for running it by hand — the app
#    sets LD_LIBRARY_PATH itself (process.rs `apply_render_lib_env`).
(
	cd "$repo"
	export FFMPEG_DIR="$prefix"
	export CARGO_TARGET_DIR="$target_dir"
	export PKG_CONFIG_PATH="$prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
	export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-Wl,-rpath-link,$prefix/lib -C link-arg=-Wl,-rpath,\$ORIGIN/pulsar-rkmpp"
	cargo build --release -p pulsar-render
)
bin="$target_dir/release/pulsar-render"

# 5. Collect exactly the libs the binary loads from the prefix, each as a regular file under its
#    soname (no symlinks: bundlers copy resources file by file), stripped, RUNPATH=$ORIGIN.
stage="$work/stage"
rm -rf "$stage"
mkdir -p "$stage/pulsar-rkmpp"
cp "$bin" "$stage/pulsar-render-rkmpp"
strip --strip-unneeded "$stage/pulsar-render-rkmpp" || true
LD_LIBRARY_PATH="$prefix/lib" ldd "$bin" | awk -v p="$prefix/lib/" 'index($3, p) == 1 { print $1 " " $3 }' |
	while read -r soname path; do
		cp -L "$path" "$stage/pulsar-rkmpp/$soname"
	done
for f in "$stage"/pulsar-rkmpp/*; do
	strip --strip-unneeded "$f" || true
	patchelf --set-rpath '$ORIGIN' "$f"
done
for want in libavcodec.so libavutil.so librockchip_mpp.so libdav1d.so; do
	ls "$stage/pulsar-rkmpp/$want".* >/dev/null 2>&1 ||
		{ echo "build-rkmpp-render: $want.* missing from the staged libs" >&2; exit 1; }
done

# 6. Check the staged copy the way the app runs it:
#    - every lib resolves from the private dir;
#    - the renderer's decoder selection OFFERS h264_rkmpp / hevc_rkmpp as zero-copy (tier-0)
#      candidates (`--list-zerocopy`, nothing opened). Off a Rockchip board they cannot
#      decode, so this is the only check that the variant is not dead on arrival (e.g. an
#      ffmpeg-rockchip that declares DRM_PRIME somewhere the selection does not look);
#    - `--probe` prints its JSON with all three codecs decodable: off-board that is the
#      software floor (native h264/hevc, libdav1d), proving the bundle loads and decodes.
if LD_LIBRARY_PATH="$stage/pulsar-rkmpp" ldd "$stage/pulsar-render-rkmpp" | grep 'not found'; then
	echo "build-rkmpp-render: unresolved libraries (above)" >&2
	exit 1
fi
LD_LIBRARY_PATH="$stage/pulsar-rkmpp" ldd "$stage/pulsar-render-rkmpp" | grep -E 'avcodec|rockchip|dav1d'
zc="$(LD_LIBRARY_PATH="$stage/pulsar-rkmpp" "$stage/pulsar-render-rkmpp" --list-zerocopy)"
echo "zero-copy candidates: $zc"
for pair in h264:h264_rkmpp h265:hevc_rkmpp; do
	printf '%s' "$zc" | grep -q "\"${pair%%:*}\":\[[^]]*\"${pair#*:}\"" ||
		{ echo "build-rkmpp-render: ${pair#*:} is not a zero-copy candidate for ${pair%%:*}" >&2; exit 1; }
done
probe="$(LD_LIBRARY_PATH="$stage/pulsar-rkmpp" "$stage/pulsar-render-rkmpp" --probe)"
echo "probe: $probe"
case "$probe" in
\[*\]) ;;
*) echo "build-rkmpp-render: --probe printed no JSON list" >&2; exit 1 ;;
esac
for c in h264 h265 av1; do
	printf '%s' "$probe" | grep -q "\"codec\":\"$c\",\"ok\":true" ||
		{ echo "build-rkmpp-render: --probe found no working $c decoder" >&2; exit 1; }
done

# 7. Stage into resources/ only now (a failed run above leaves the bundle as it was).
mkdir -p "$res"
rm -rf "$res/pulsar-rkmpp" "$res/pulsar-render-rkmpp"
cp -r "$stage/pulsar-rkmpp" "$res/pulsar-rkmpp"
cp "$stage/pulsar-render-rkmpp" "$res/pulsar-render-rkmpp"
chmod 755 "$res/pulsar-render-rkmpp"
ls -l "$res/pulsar-render-rkmpp" "$res/pulsar-rkmpp"
du -sh "$res/pulsar-rkmpp"
