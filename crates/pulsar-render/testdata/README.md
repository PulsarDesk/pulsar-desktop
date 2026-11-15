# Decode-probe fixtures

The decoder probe (`decode.rs::select` / `probe_json`) validates each codec by REALLY
decoding a tiny canned bitstream. There are two kinds per codec:

## Baseline (conformant) fixtures — `test.{h264,h265,av1.ivf}`

ffmpeg-muxed, spec-conformant single keyframes. They answer "does this decoder handle
this codec AT ALL". Regenerate with:

```
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 1 -c:v libx264   -f h264  test.h264
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 1 -c:v libx265   -f hevc  test.h265
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 1 -c:v libaom-av1 -f ivf  test.av1.ivf
```

## 4:4:4 fixtures — `test-444.{h264,h265}`

H.264 High 4:4:4 Predictive / HEVC RExt (Main 4:4:4) single keyframes. Every renderer's probe
decodes them (Windows Media Foundation → AYUV, macOS VideoToolbox → `444f`/`444v`, Linux
libavcodec in software → yuv444p) and reports `"yuv444"` per codec; the app asks a host for
4:4:4 only in a codec that passed (see `docs/agents/host-encode.md`, "4:4:4 chroma"). The
`stream::chroma` unit tests read their SPS. Regenerate with:

```
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 1 -pix_fmt yuv444p -c:v libx264 -profile:v high444 -f h264 test-444.h264
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 1 -pix_fmt yuv444p -c:v libx265 -f hevc test-444.h265
```

## Encoder-family fixtures — `test-nvenc.{h264,h265,av1.ivf}`

REAL bitstreams from our own native encoder path. A conformant sample is NOT enough: the
Pi's ffmpeg-4.4 rkmpp decodes the libx265 HEVC fine but chokes on our native NVENC HEVC
("Multi-layer HEVC coding is not implemented", "Skipping NAL unit 30"). The probe decodes
the family sample with the SAME decoder it just selected; if it fails, that codec is
tagged `incompatible_with: ["nvenc"]` in the probe JSON, and the negotiator
(`play.rs`) drops the codec when the host has that encoder family (falls back to h264).

An **empty** file means "no family fixture committed" and is silently skipped — the probe
degrades to baseline-only and never falsely flags a codec.

### How to (re)generate from a real host

ffmpeg's `hevc_nvenc` output does NOT reproduce the problematic bitstream — the issue is
in OUR native NVENC path, so the sample must come from the actual host encoder. Capture it
with the host's `PULSAR_DUMP_BITSTREAM` dump (added by the bitstream-dump task), then cut
the first ~1 second of access units:

```
# On the Windows host, with the native NVENC HEVC stream running:
#   PULSAR_DUMP_BITSTREAM=C:\path\dump.h265  (host writes the raw elementary stream there)
#
# Cut ~1 s (≈ first keyframe + following frames) into the fixture. The probe only needs a
# decodable GOP; keep it small. For an Annex-B elementary stream:
ffmpeg -i dump.h265 -t 1 -c:v copy -f hevc test-nvenc.h265
# AV1 / H.264 analogously (AV1 → -f ivf test-nvenc.av1.ivf).
```

Commit the resulting non-empty fixture. Verify:

```
cargo build --release -p pulsar-render
target/release/pulsar-render --probe   # h265 entry should show "incompatible_with":["nvenc"] on rkmpp
```

## HDR10 fixtures — `test-hdr10.{h265,av1.obu}`

64×64 single 10-bit keyframes tagged BT.2020 / SMPTE 2084 / BT.2020 NCL. The Windows probe
(`win/decode.rs`) decodes them to report `ten_bit` (the client may then ask a host for HDR),
and its tests check the decoder hands out P010 flagged PQ. Regenerate with:

```
ffmpeg -f lavfi -i testsrc2=size=64x64:rate=30 -frames:v 1 -c:v libx265 -pix_fmt yuv420p10le -x265-params "keyint=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc" -f hevc test-hdr10.h265
ffmpeg -f lavfi -i testsrc2=size=64x64:rate=30 -frames:v 1 -c:v libaom-av1 -cpu-used 8 -pix_fmt yuv420p10le -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -f obu test-hdr10.av1.obu
```

## Colour-bar fixtures — `colour-{bt709,untagged,bt601}.h264`

Two frames of red / green / blue / white bars (256×128, 64 px each) for the Windows decode +
present harness (`src/win/colour_bars.rs`). Made with the bundled ffmpeg (7.1+):

```
SRC="color=c=red:s=64x128:r=30[a];color=c=lime:s=64x128:r=30[b];color=c=blue:s=64x128:r=30[c];color=c=white:s=64x128:r=30[d];[a][b][c][d]hstack=4,format=bgr0"
X="-c:v libx264 -profile:v high -pix_fmt yuv420p -bf 0 -g 30 -preset veryslow -crf 10"
ffmpeg -filter_complex "$SRC,setparams=color_primaries=bt709:color_trc=bt709" -frames:v 2 $X -colorspace bt709 -color_range tv -f h264 colour-bt709.h264
ffmpeg -filter_complex "$SRC,setparams=color_primaries=smpte170m:color_trc=smpte170m" -frames:v 2 $X -colorspace smpte170m -color_range tv -f h264 colour-bt601.h264
ffmpeg -filter_complex "$SRC,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p,setparams=colorspace=unknown:range=unknown" -frames:v 2 $X -f h264 colour-untagged.h264
```
