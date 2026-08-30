// The in-game counter's quad.
//
// Compiled to DXBC and committed next to this file; the DLL `include_bytes!`s
// the blobs. Compiling at runtime would mean loading `d3dcompiler_47.dll` into
// someone's game, which is slow, may not be present, and is exactly the kind of
// thing an anti-cheat is right to look twice at. Regenerate with:
//
//   fxc /T vs_4_0 /E vs_main /Fo overlay_vs.dxbc overlay.hlsl
//   fxc /T ps_4_0 /E ps_main /Fo overlay_ps.dxbc overlay.hlsl
//
// The quad is generated from SV_VertexID, so there is no vertex buffer and no
// input layout to bind — which is state we then do not have to save, restore or
// get wrong inside a game's render loop.

cbuffer Params : register(b0)
{
    // xy = top-left in normalised device coordinates, zw = size in NDC
    // (z positive, w negative, because NDC y runs upwards).
    float4 rect;
    // x = opacity 0..1. The rest is padding: a D3D11 constant buffer is
    // allocated in 16-byte registers whatever we put in it.
    float4 tint;
};

Texture2D    badge : register(t0);
SamplerState samp  : register(s0);

struct VSOut
{
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

VSOut vs_main(uint vid : SV_VertexID)
{
    // Triangle strip: 0=(0,0) 1=(1,0) 2=(0,1) 3=(1,1).
    float2 corner = float2(vid & 1, (vid >> 1) & 1);
    VSOut o;
    o.pos = float4(rect.xy + corner * rect.zw, 0.0f, 1.0f);
    o.uv = corner;
    return o;
}

float4 ps_main(VSOut i) : SV_Target
{
    // The sprite carries straight alpha, which is what SRC_ALPHA/INV_SRC_ALPHA
    // blending expects, so the colour goes out untouched.
    float4 c = badge.Sample(samp, i.uv);
    c.a *= tint.x;
    return c;
}
