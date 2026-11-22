// H.264 / HEVC (Annex-B) -> NV12 with the Media Foundation decoder MFT.
#pragma once

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <mfapi.h>
#include <mftransform.h>
#include <wrl/client.h>
#include <d3d11.h>
#include <mfobjects.h>
#include <mfidl.h>

#include <cstdint>
#include <functional>
#include <vector>

class VideoDecoder
{
public:
    // nv12 points at the visible picture: `height` luma rows of `pitch`
    // bytes, then height/2 interleaved UV rows, contiguous.
    using FrameCallback = std::function<void(const uint8_t* nv12, long pitch, uint32_t width, uint32_t height)>;

    // Decodes on the GPU (DXVA, through a Direct3D 11 device) when it can, else in software
    // (NIWCAM_DECODER=software forces that). At 4K a software decoder needs 13+ ms a frame.
    HRESULT Init(bool hevc, uint32_t width, uint32_t height);
    void Reset() { m_mft.Reset(); m_gpu = false; }
    bool IsHardware() const { return m_gpu; }

    // Feeds one access unit (Annex-B). Calls onFrame for every decoded
    // frame. A failure means the stream needs a new keyframe.
    HRESULT Decode(const uint8_t* data, size_t size, int64_t time100ns, const FrameCallback& onFrame);

private:
    HRESULT InitWith(bool hevc, uint32_t width, uint32_t height, bool hardware);
    HRESULT SetupGpu();
    bool ReadGpuSample(IMFSample* sample, const FrameCallback& onFrame);
    HRESULT SelectOutputType();
    HRESULT Drain(const FrameCallback& onFrame);

    Microsoft::WRL::ComPtr<IMFTransform> m_mft;
    bool m_providesSamples = false;
    DWORD m_outputSize = 0;
    uint32_t m_codedWidth = 0, m_codedHeight = 0; // decoder surface (e.g. 1920x1088)
    uint32_t m_width = 0, m_height = 0;           // visible picture (e.g. 1920x1080)
    LONG m_stride = 0;
    std::vector<uint8_t> m_staging;

    // Hardware decoding.
    bool m_gpu = false;
    bool m_gpuFault = false; // the GPU path failed: the next Decode continues in software
    bool m_wantHevc = false;
    uint32_t m_wantWidth = 0, m_wantHeight = 0;
    Microsoft::WRL::ComPtr<ID3D11Device> m_device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> m_context;
    Microsoft::WRL::ComPtr<IMFDXGIDeviceManager> m_manager;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_stagingTex; // CPU-readable copy of a decoded picture
    UINT m_stagingTexW = 0, m_stagingTexH = 0;
};
