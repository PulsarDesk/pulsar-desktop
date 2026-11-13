// Snapdragon Game Super Resolution 1 (SGSR v1), from github.com/quic/snapdragon-gsr
// (sgsr/v1/include/hlsl), unmodified except for the Pulsar wrapper below.
/*
Copyright (c) 2023, Qualcomm Innovation Center, Inc. All rights reserved.

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

#define SGSR_MOBILE
cbuffer PerFrameConstants : register(b0) {
	float4 ViewportInfo; // 1/input size (xy), input size (zw)
	float4 Unused;
};
struct VOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VOut vs_main(uint id : SV_VertexID) {
	VOut o;
	float2 uv = float2((id << 1) & 2, id & 2);
	o.uv = uv;
	o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
	return o;
}
SamplerState samLinearClamp : register(s0);
Texture2D<half4> InputTexture : register(t0);
#define SGSR_H 1
half4 SGSRRH(float2 p) { return InputTexture.GatherRed(samLinearClamp, p); }
half4 SGSRGH(float2 p) { return InputTexture.GatherGreen(samLinearClamp, p); }
half4 SGSRBH(float2 p) { return InputTexture.GatherBlue(samLinearClamp, p); }
half4 SGSRAH(float2 p) { return InputTexture.GatherAlpha(samLinearClamp, p); }
half4 SGSRRGBH(float2 p) { return InputTexture.SampleLevel(samLinearClamp, p, 0); }
half4 SGSRH(float2 p, uint channel) {
	if (channel == 0) return SGSRRH(p);
	if (channel == 1) return SGSRGH(p);
	if (channel == 2) return SGSRBH(p);
	return SGSRAH(p);
}
//============================================================================================================
//
//
//                  Copyright (c) 2023, Qualcomm Innovation Center, Inc. All rights reserved.
//                              SPDX-License-Identifier: BSD-3-Clause
//
//============================================================================================================

////////////////////////
// USER CONFIGURATION //
////////////////////////

/*
* Operation modes:
* RGBA -> 1
* RGBY -> 3
* LERP -> 4
*/
#define OperationMode 1

/*
* If set, will use edge direction to improve visual quality
* Expect a minimal cost increase
*/
// #define UseEdgeDirection

#define EdgeThreshold 8.0/255.0

#define EdgeSharpness 2.0

////////////////////////
////////////////////////
////////////////////////

// ///////SGSR_GL_Mobile.frag/////////////////////////////////////////
#if defined(SGSR_MOBILE)
half fastLanczos2(half x)
{
	half wA = x- half(4.0);
	half wB = x*wA-wA;
	wA *= wA;
	return wB*wA;
}

#if defined(UseEdgeDirection)
half2 weightY(half dx, half dy, half c, half3 data)
#else
half2 weightY(half dx, half dy, half c, half data)
#endif
{
#if defined(UseEdgeDirection)
	half std = data.x;
	half2 dir = data.yz;

	half edgeDis = ((dx*dir.y)+(dy*dir.x));
	half x = (((dx*dx)+(dy*dy))+((edgeDis*edgeDis)*((clamp(((c*c)*std),0.0,1.0)*0.7)+-1.0)));
#else
	half std = data;
	half x = ((dx*dx)+(dy* dy))* half(0.5) + clamp(abs(c)*std, 0.0, 1.0);
#endif

	half w = fastLanczos2(x);
	return half2(w, w * c);
}

half2 edgeDirection(half4 left, half4 right)
{
	half2 dir;
	half RxLz = (right.x + (-left.z));
	half RwLy = (right.w + (-left.y));
	half2 delta;
	delta.x = (RxLz + RwLy);
	delta.y = (RxLz + (-RwLy));
	half lengthInv = rsqrt((delta.x * delta.x+ 3.075740e-05) + (delta.y * delta.y));
	dir.x = (delta.x * lengthInv);
	dir.y = (delta.y * lengthInv);
	return dir;
}

void SgsrYuvH(
	out half4 pix,
	float2 uv,
	float4 con1)
{
	int mode = OperationMode;
	half edgeThreshold = EdgeThreshold;
	half edgeSharpness = EdgeSharpness;
	if(mode == 1)
		pix.xyz = SGSRRGBH(uv).xyz;
    else
		pix.xyzw = SGSRRGBH(uv).xyzw;
	float xCenter;
	xCenter = abs(uv.x+-0.5);
	float yCenter;
	yCenter = abs(uv.y+-0.5);
	
	//todo: config the SR region based on needs
    //if ( mode!=4 && xCenter*xCenter+yCenter*yCenter<=0.4 * 0.4)
	if ( mode!=4)
	{
		float2 imgCoord = ((uv.xy*con1.zw)+ float2(-0.5,0.5));
		float2 imgCoordPixel = floor(imgCoord);
		float2 coord = (imgCoordPixel*con1.xy);
		half2 pl = (imgCoord+(-imgCoordPixel));
		half4  left = SGSRH(coord, mode);
		
		half edgeVote = abs(left.z - left.y) + abs(pix[mode] - left.y)  + abs(pix[mode] - left.z) ;
		if(edgeVote > edgeThreshold)
		{
			coord.x += con1.x;

			half4 right = SGSRH(coord + float2(con1.x,  0.0), mode);
			half4 upDown;
			upDown.xy = SGSRH(coord + float2(0.0, -con1.y), mode).wz;
			upDown.zw = SGSRH(coord + float2(0.0,  con1.y), mode).yx;

			half mean = (left.y+left.z+right.x+right.w)* half(0.25);
			left = left - half4(mean,mean,mean,mean);
			right = right - half4(mean, mean, mean, mean);
			upDown = upDown - half4(mean, mean, mean, mean);
			pix.w =pix[mode] - mean;

			half sum = (((((abs(left.x)+abs(left.y))+abs(left.z))+abs(left.w))+(((abs(right.x)+abs(right.y))+abs(right.z))+abs(right.w)))+(((abs(upDown.x)+abs(upDown.y))+abs(upDown.z))+abs(upDown.w)));
			half sumMean = 1.014185e+01/sum;
			half std = (sumMean*sumMean);	

#if defined(UseEdgeDirection)
			half3 data = half3(std, edgeDirection(left, right));
#else
			half data = std;
#endif
			
			half2 aWY = weightY(pl.x, pl.y+1.0, upDown.x,data);
			aWY += weightY(pl.x-1.0, pl.y+1.0, upDown.y,data);
			aWY += weightY(pl.x-1.0, pl.y-2.0, upDown.z,data);
			aWY += weightY(pl.x, pl.y-2.0, upDown.w,data);			
			aWY += weightY(pl.x+1.0, pl.y-1.0, left.x,data);
			aWY += weightY(pl.x, pl.y-1.0, left.y,data);
			aWY += weightY(pl.x, pl.y, left.z,data);
			aWY += weightY(pl.x+1.0, pl.y, left.w,data);
			aWY += weightY(pl.x-1.0, pl.y-1.0, right.x,data);
			aWY += weightY(pl.x-2.0, pl.y-1.0, right.y,data);
			aWY += weightY(pl.x-2.0, pl.y, right.z,data);
			aWY += weightY(pl.x-1.0, pl.y, right.w,data);

			half finalY = aWY.y/aWY.x;

			half max4 = max(max(left.y,left.z),max(right.x,right.w));
			half min4 = min(min(left.y,left.z),min(right.x,right.w));
			finalY = clamp(edgeSharpness*finalY, min4, max4);
					
			half deltaY = finalY -pix.w;

			pix.x = saturate((pix.x+deltaY));
			pix.y = saturate((pix.y+deltaY));
			pix.z = saturate((pix.z+deltaY));
		}
	}
	pix.w = 1.0;  //assume alpha channel is not used

}
#endif
////////////////////////////////////////////////////////////////////////
half4 ps_sgsr(float4 position : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
	half4 c = half4(0, 0, 0, 1);
	SgsrYuvH(c, uv, ViewportInfo);
	return c;
}
