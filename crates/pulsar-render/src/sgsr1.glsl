// Snapdragon Game Super Resolution 1 (SGSR v1), from github.com/quic/snapdragon-gsr
// (sgsr/v1/include/glsl/sgsr1_shader_mobile.frag), ported to GLSL ES 1.00 for Pulsar's GL
// presenters (Linux `gl_fsr.rs`; reusable by any GL / GLSL renderer — the Windows HLSL twin is
// `win/sgsr1.hlsl`). Same math as the CURRENT upstream mobile variant (2025 revision) in its
// RGBA operation mode (edge analysis on the green channel, no edge direction):
// std = 2.181818 / sum, distance weight 0.55, and the luma change limited to +-23/255. The
// 2023 revision the Windows port started from (std = (10.14185 / sum)^2, weight 0.5, no limit)
// over-sharpens desktop text into dark halos — the 2026-11-13 Windows A/B. The only other change
// is that ES 1.00 has no `textureGather`, so each 2x2 gather is four explicit texel-centre
// fetches (`gatherG`, same component order as the hardware gather).
/*
Copyright (c) 2023-2025, Qualcomm Innovation Center, Inc. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.

SPDX-License-Identifier: BSD-3-Clause
*/
// Interface (supplied by the including program's prelude, see `gl_fsr.rs` PRE):
//   varying vec2 uv;        output pixel's position in the source, 0..1
//   uniform sampler2D tex;  the source picture, BILINEAR filtering, clamp to edge
//   uniform vec4 con0;      source size (xy), 1 / source size (zw)

#define SGSR_EDGE_THRESHOLD (8.0 / 255.0)
#define SGSR_EDGE_SHARPNESS 2.0
#define SGSR_MAX_DELTA (23.0 / 255.0)

// Green of texel `t` (integer texel index; the centre fetch is exact under bilinear filtering).
float sgsrG(vec2 t){ return texture2D(tex, (t + 0.5) * con0.zw).g; }

// textureGather(tex, c / size).g around the texel corner `c`: x = (-1, 0), y = (0, 0),
// z = (0, -1), w = (-1, -1) relative to `c` (the hardware gather's counter-clockwise order).
vec4 gatherG(vec2 c){
	return vec4(sgsrG(c + vec2(-1.0, 0.0)), sgsrG(c), sgsrG(c + vec2(0.0, -1.0)), sgsrG(c + vec2(-1.0, -1.0)));
}

float fastLanczos2(float x){
	float wA = x - 4.0;
	float wB = x * wA - wA;
	wA *= wA;
	return wB * wA;
}

vec2 weightY(float dx, float dy, float c, float std){
	float x = ((dx * dx) + (dy * dy)) * 0.55 + clamp(abs(c) * std, 0.0, 1.0);
	float w = fastLanczos2(x);
	return vec2(w, w * c);
}

void main(){
	vec4 pix = vec4(texture2D(tex, uv).rgb, 0.0);
	vec2 imgCoord = uv * con0.xy + vec2(-0.5, 0.5);
	vec2 imgCoordPixel = floor(imgCoord);
	vec2 coord = imgCoordPixel;
	vec2 pl = imgCoord - imgCoordPixel;
	vec4 left = gatherG(coord);
	float edgeVote = abs(left.z - left.y) + abs(pix.y - left.y) + abs(pix.y - left.z);
	if (edgeVote > SGSR_EDGE_THRESHOLD) {
		coord.x += 1.0;
		vec4 right = gatherG(coord + vec2(1.0, 0.0));
		vec4 upDown;
		upDown.xy = gatherG(coord + vec2(0.0, -1.0)).wz;
		upDown.zw = gatherG(coord + vec2(0.0, 1.0)).yx;
		float mean = (left.y + left.z + right.x + right.w) * 0.25;
		left = left - vec4(mean);
		right = right - vec4(mean);
		upDown = upDown - vec4(mean);
		pix.w = pix.y - mean;
		float sum = abs(left.x) + abs(left.y) + abs(left.z) + abs(left.w)
			+ abs(right.x) + abs(right.y) + abs(right.z) + abs(right.w)
			+ abs(upDown.x) + abs(upDown.y) + abs(upDown.z) + abs(upDown.w);
		// Guarded (upstream divides as is): a flat 4x4 never gets here with highp, but a
		// mediump-only GPU could round `sum` to 0.
		float std = 2.181818 / max(sum, 1e-4);
		vec2 aWY = weightY(pl.x, pl.y + 1.0, upDown.x, std);
		aWY += weightY(pl.x - 1.0, pl.y + 1.0, upDown.y, std);
		aWY += weightY(pl.x - 1.0, pl.y - 2.0, upDown.z, std);
		aWY += weightY(pl.x, pl.y - 2.0, upDown.w, std);
		aWY += weightY(pl.x + 1.0, pl.y - 1.0, left.x, std);
		aWY += weightY(pl.x, pl.y - 1.0, left.y, std);
		aWY += weightY(pl.x, pl.y, left.z, std);
		aWY += weightY(pl.x + 1.0, pl.y, left.w, std);
		aWY += weightY(pl.x - 1.0, pl.y - 1.0, right.x, std);
		aWY += weightY(pl.x - 2.0, pl.y - 1.0, right.y, std);
		aWY += weightY(pl.x - 2.0, pl.y, right.z, std);
		aWY += weightY(pl.x - 1.0, pl.y, right.w, std);
		float wSum = abs(aWY.x) < 1e-5 ? 1e-5 : aWY.x;
		float finalY = aWY.y / wSum;
		float max4 = max(max(left.y, left.z), max(right.x, right.w));
		float min4 = min(min(left.y, left.z), min(right.x, right.w));
		finalY = clamp(SGSR_EDGE_SHARPNESS * finalY, min4, max4);
		float deltaY = clamp(finalY - pix.w, -SGSR_MAX_DELTA, SGSR_MAX_DELTA);
		pix.xyz = clamp(pix.xyz + deltaY, 0.0, 1.0);
	}
	gl_FragColor = vec4(pix.xyz, 1.0);
}
