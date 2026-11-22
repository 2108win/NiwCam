#include "VideoDecoder.h"

#include <mferror.h>
#include <mfidl.h>
#include <strmif.h>   // ICodecAPI
#include <initguid.h> // defines CODECAPI_AVLowLatencyMode in this file only
#include <codecapi.h>

#include <cstdlib>
#include <cstring>
#include <d3d11_4.h>
#include <mfidl.h>

using Microsoft::WRL::ComPtr;

HRESULT VideoDecoder::Init(bool hevc, uint32_t width, uint32_t height)
{
    m_wantHevc = hevc;
    m_wantWidth = width;
    m_wantHeight = height;
    m_gpuFault = false;
    char env[16] = {};
    size_t got = 0;
    getenv_s(&got, env, sizeof(env), "NIWCAM_DECODER");
    if (strcmp(env, "software") != 0)
    {
        const HRESULT hr = InitWith(hevc, width, height, true);
        if (SUCCEEDED(hr)) return hr;
    }
    return InitWith(hevc, width, height, false);
}

// The device the decoder runs on, given to the (synchronous, inbox) decoder MFT. Any failure here
// means "decode in software".
HRESULT VideoDecoder::SetupGpu()
{
    ComPtr<IMFAttributes> attrs;
    HRESULT hr = m_mft->GetAttributes(&attrs);
    if (FAILED(hr)) return hr;
    if (MFGetAttributeUINT32(attrs.Get(), MF_SA_D3D11_AWARE, 0) == 0) return E_FAIL;
    hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, nullptr, 0,
        D3D11_SDK_VERSION, &m_device, nullptr, &m_context);
    if (FAILED(hr)) return hr;
    ComPtr<ID3D11Multithread> mt;
    if (SUCCEEDED(m_device.As(&mt))) mt->SetMultithreadProtected(TRUE); // the decoder uses it from its own threads
    UINT token = 0;
    hr = MFCreateDXGIDeviceManager(&token, &m_manager);
    if (FAILED(hr)) return hr;
    hr = m_manager->ResetDevice(m_device.Get(), token);
    if (FAILED(hr)) return hr;
    return m_mft->ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, reinterpret_cast<ULONG_PTR>(m_manager.Get()));
}

HRESULT VideoDecoder::InitWith(bool hevc, uint32_t width, uint32_t height, bool hardware)
{
    m_mft.Reset();
    m_gpu = false;
    m_stagingTex.Reset();
    m_stagingTexW = m_stagingTexH = 0;
    m_manager.Reset();
    m_context.Reset();
    m_device.Reset();
    const GUID subtype = hevc ? MFVideoFormat_HEVC : MFVideoFormat_H264;
    MFT_REGISTER_TYPE_INFO in{ MFMediaType_Video, subtype };
    MFT_REGISTER_TYPE_INFO out{ MFMediaType_Video, MFVideoFormat_NV12 };

    IMFActivate** activates = nullptr;
    UINT32 count = 0;
    // Synchronous decoders only; hardware MFTs are async and need an event loop.
    HRESULT hr = MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER, &in, &out, &activates, &count);
    if (FAILED(hr)) return hr;
    if (count == 0)
    {
        CoTaskMemFree(activates);
        return MF_E_TOPO_CODEC_NOT_FOUND; // e.g. HEVC Video Extensions not installed
    }
    hr = activates[0]->ActivateObject(IID_PPV_ARGS(&m_mft));
    for (UINT32 i = 0; i < count; ++i) activates[i]->Release();
    CoTaskMemFree(activates);
    if (FAILED(hr)) return hr;

    ComPtr<IMFAttributes> attrs;
    if (SUCCEEDED(m_mft->GetAttributes(&attrs))) attrs->SetUINT32(MF_LOW_LATENCY, TRUE);
    ComPtr<ICodecAPI> codecApi;
    if (SUCCEEDED(m_mft.As(&codecApi)))
    {
        VARIANT v;
        v.vt = VT_BOOL;
        v.boolVal = VARIANT_TRUE;
        codecApi->SetValue(&CODECAPI_AVLowLatencyMode, &v);
    }

    if (hardware)
    {
        hr = SetupGpu();
        if (FAILED(hr)) return hr;
        m_gpu = true;
    }

    ComPtr<IMFMediaType> type;
    hr = MFCreateMediaType(&type);
    if (FAILED(hr)) return hr;
    type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    type->SetGUID(MF_MT_SUBTYPE, subtype);
    type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    MFSetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, width, height);
    hr = m_mft->SetInputType(0, type.Get(), 0);
    if (FAILED(hr)) return hr;

    m_width = width;
    m_height = height;
    hr = SelectOutputType();
    if (FAILED(hr)) return hr;
    m_mft->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    m_mft->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    return S_OK;
}

HRESULT VideoDecoder::SelectOutputType()
{
    for (DWORD i = 0;; ++i)
    {
        ComPtr<IMFMediaType> type;
        HRESULT hr = m_mft->GetOutputAvailableType(0, i, &type);
        if (FAILED(hr)) return hr; // MF_E_NO_MORE_TYPES: no NV12 offered
        GUID sub{};
        type->GetGUID(MF_MT_SUBTYPE, &sub);
        if (sub != MFVideoFormat_NV12) continue;
        hr = m_mft->SetOutputType(0, type.Get(), 0);
        if (FAILED(hr)) return hr;

        // The surface may be taller than the picture (1088 for 1080p):
        // the visible area is the minimum display aperture.
        MFGetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, &m_codedWidth, &m_codedHeight);
        UINT32 stride = 0;
        m_stride = SUCCEEDED(type->GetUINT32(MF_MT_DEFAULT_STRIDE, &stride)) ? static_cast<LONG>(stride) : static_cast<LONG>(m_codedWidth);
        MFVideoArea area{};
        if (SUCCEEDED(type->GetBlob(MF_MT_MINIMUM_DISPLAY_APERTURE, reinterpret_cast<UINT8*>(&area), sizeof(area), nullptr)) &&
            area.Area.cx > 0 && area.Area.cy > 0)
        {
            m_width = static_cast<uint32_t>(area.Area.cx);
            m_height = static_cast<uint32_t>(area.Area.cy);
        }
        else
        {
            m_width = m_codedWidth;
            m_height = m_codedHeight;
        }

        MFT_OUTPUT_STREAM_INFO info{};
        m_mft->GetOutputStreamInfo(0, &info);
        m_providesSamples = (info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
        m_outputSize = info.cbSize;
        return S_OK;
    }
}

HRESULT VideoDecoder::Decode(const uint8_t* data, size_t size, int64_t time100ns, const FrameCallback& onFrame)
{
    if (m_gpuFault)
    {
        // The GPU path failed (device lost, an odd format): go on in software. This picture is lost,
        // so the caller asks for a keyframe like after any decode error.
        m_gpuFault = false;
        InitWith(m_wantHevc, m_wantWidth, m_wantHeight, false);
        return E_FAIL;
    }
    if (!m_mft) return E_UNEXPECTED;

    ComPtr<IMFSample> sample;
    ComPtr<IMFMediaBuffer> buffer;
    HRESULT hr = MFCreateMemoryBuffer(static_cast<DWORD>(size), &buffer);
    if (FAILED(hr)) return hr;
    BYTE* dst = nullptr;
    buffer->Lock(&dst, nullptr, nullptr);
    memcpy(dst, data, size);
    buffer->Unlock();
    buffer->SetCurrentLength(static_cast<DWORD>(size));
    MFCreateSample(&sample);
    sample->AddBuffer(buffer.Get());
    sample->SetSampleTime(time100ns);

    hr = m_mft->ProcessInput(0, sample.Get(), 0);
    if (hr == MF_E_NOTACCEPTING)
    {
        // Output is pending: drain it, then retry once.
        Drain(onFrame);
        hr = m_mft->ProcessInput(0, sample.Get(), 0);
    }
    if (FAILED(hr)) return hr;
    return Drain(onFrame);
}

HRESULT VideoDecoder::Drain(const FrameCallback& onFrame)
{
    for (;;)
    {
        MFT_OUTPUT_DATA_BUFFER out{};
        ComPtr<IMFSample> own;
        if (!m_providesSamples)
        {
            ComPtr<IMFMediaBuffer> b;
            MFCreateMemoryBuffer(m_outputSize ? m_outputSize : m_codedWidth * m_codedHeight * 3 / 2, &b);
            MFCreateSample(&own);
            own->AddBuffer(b.Get());
            out.pSample = own.Get();
        }
        DWORD status = 0;
        HRESULT hr = m_mft->ProcessOutput(0, 1, &out, &status);
        ComPtr<IMFSample> produced;
        produced.Attach(m_providesSamples ? out.pSample : nullptr);
        if (out.pEvents) out.pEvents->Release();

        if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) return S_OK;
        if (hr == MF_E_TRANSFORM_STREAM_CHANGE)
        {
            // First frame (or resolution change): the real output format is known now.
            hr = SelectOutputType();
            if (FAILED(hr))
            {
                m_gpuFault = m_gpu; // e.g. 10-bit HEVC offered as P010 only on the GPU path
                return hr;
            }
            continue;
        }
        if (FAILED(hr)) return hr;

        IMFSample* result = m_providesSamples ? produced.Get() : own.Get();
        if (m_gpu)
        {
            if (!result) continue;
            if (!ReadGpuSample(result, onFrame))
            {
                m_gpuFault = true;
                return E_FAIL;
            }
            continue;
        }
        ComPtr<IMFMediaBuffer> buf;
        if (!result || FAILED(result->ConvertToContiguousBuffer(&buf))) continue;
        BYTE* p = nullptr;
        DWORD len = 0;
        if (FAILED(buf->Lock(&p, nullptr, &len))) continue;

        const LONG stride = m_stride > 0 ? m_stride : static_cast<LONG>(m_codedWidth);
        const size_t lumaBytes = static_cast<size_t>(stride) * m_codedHeight;
        if (len >= lumaBytes + lumaBytes / 2)
        {
            if (m_height == m_codedHeight)
            {
                onFrame(p, stride, m_width, m_height);
            }
            else
            {
                // UV starts after the full coded height: repack so it follows
                // the visible rows, as the rest of the pipeline expects.
                const size_t visibleLuma = static_cast<size_t>(stride) * m_height;
                m_staging.resize(visibleLuma + visibleLuma / 2);
                memcpy(m_staging.data(), p, visibleLuma);
                memcpy(m_staging.data() + visibleLuma, p + lumaBytes, visibleLuma / 2);
                onFrame(m_staging.data(), stride, m_width, m_height);
            }
        }
        buf->Unlock();
    }
}

// One decoded GPU picture into system memory, then on. False = the GPU path is broken.
bool VideoDecoder::ReadGpuSample(IMFSample* sample, const FrameCallback& onFrame)
{
    ComPtr<IMFMediaBuffer> buf;
    ComPtr<IMFDXGIBuffer> dxgi;
    ComPtr<ID3D11Texture2D> tex;
    UINT sub = 0;
    if (FAILED(sample->GetBufferByIndex(0, &buf)) || FAILED(buf.As(&dxgi)) ||
        FAILED(dxgi->GetResource(IID_PPV_ARGS(&tex))) || FAILED(dxgi->GetSubresourceIndex(&sub)))
        return false;
    D3D11_TEXTURE2D_DESC desc{};
    tex->GetDesc(&desc);
    if (!m_stagingTex || m_stagingTexW != desc.Width || m_stagingTexH != desc.Height)
    {
        D3D11_TEXTURE2D_DESC sd{};
        sd.Width = desc.Width;
        sd.Height = desc.Height;
        sd.MipLevels = 1;
        sd.ArraySize = 1;
        sd.Format = desc.Format;
        sd.SampleDesc = desc.SampleDesc;
        sd.Usage = D3D11_USAGE_STAGING;
        sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        m_stagingTex.Reset();
        if (FAILED(m_device->CreateTexture2D(&sd, nullptr, &m_stagingTex))) return false;
        m_stagingTexW = desc.Width;
        m_stagingTexH = desc.Height;
    }
    m_context->CopySubresourceRegion(m_stagingTex.Get(), 0, 0, 0, 0, tex.Get(), sub, nullptr);
    D3D11_MAPPED_SUBRESOURCE mapped{};
    if (FAILED(m_context->Map(m_stagingTex.Get(), 0, D3D11_MAP_READ, 0, &mapped))) return false;
    const size_t pitch = mapped.RowPitch;
    const size_t rows = desc.Height;
    const uint8_t* p = static_cast<const uint8_t*>(mapped.pData);
    if (rows == m_height)
    {
        onFrame(p, static_cast<long>(pitch), m_width, m_height);
    }
    else
    {
        // UV starts after the texture's full height: repack so it follows the visible rows.
        const size_t visibleLuma = pitch * m_height;
        m_staging.resize(visibleLuma + visibleLuma / 2);
        memcpy(m_staging.data(), p, visibleLuma);
        memcpy(m_staging.data() + visibleLuma, p + pitch * rows, visibleLuma / 2);
        onFrame(m_staging.data(), static_cast<long>(pitch), m_width, m_height);
    }
    m_context->Unmap(m_stagingTex.Get(), 0);
    return true;
}
