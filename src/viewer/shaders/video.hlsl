struct VertexInput {
    float2 position : POSITION;
    float2 texcoord : TEXCOORD0;
};

struct PixelInput {
    float4 position : SV_POSITION;
    float2 texcoord : TEXCOORD0;
};

PixelInput vs_main(VertexInput input) {
    PixelInput output;
    output.position = float4(input.position, 0.0f, 1.0f);
    output.texcoord = input.texcoord;
    return output;
}

#ifdef VIDEO_ARRAY
Texture2DArray<float4> input_plane_0 : register(t0);
Texture2DArray<float4> input_plane_1 : register(t1);
#define VIDEO_COORD float3(input.texcoord, 0.0f)
#else
Texture2D<float4> input_plane_0 : register(t0);
Texture2D<float4> input_plane_1 : register(t1);
#define VIDEO_COORD input.texcoord
#endif
SamplerState video_sampler : register(s0);

cbuffer ColorTransform : register(b0) {
    float4 color_row_0;
    float4 color_row_1;
    float4 color_row_2;
};

float4 ps_yuv(PixelInput input) : SV_TARGET {
    float y = input_plane_0.Sample(video_sampler, VIDEO_COORD).r;
    float2 uv = input_plane_1.Sample(video_sampler, VIDEO_COORD).rg;
    float4 yuv = float4(y, uv.x, uv.y, 1.0f);
    return float4(
        dot(color_row_0, yuv),
        dot(color_row_1, yuv),
        dot(color_row_2, yuv),
        1.0f
    );
}

float4 ps_rgba(PixelInput input) : SV_TARGET {
    return input_plane_0.Sample(video_sampler, VIDEO_COORD);
}
