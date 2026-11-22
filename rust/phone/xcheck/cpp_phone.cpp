// C++ reference driver for rust/phone: the same subcommands and output lines as
// rust/phone/examples/phone_driver.rs, built from the real src/engine files (PhoneLink, Crypto,
// Discovery, UsbTransport). Never starts discovery or an adb server. Needs CAMSTUDIO_DATA_DIR.
//
//   cpp_phone datadir | clientid | clientname
//   cpp_phone keysave <deviceId> <hex> | keyload <deviceId> | keyhas <deviceId> | keyforget <deviceId>
//   cpp_phone connect <host> <port> known|auto|pair
//   cpp_phone stream <host> <port> <frames> [audio]
//   cpp_phone known <host> <port>        (Reconnect-style lookup: KnownPhones with a manual entry)
#include "../../../src/engine/Crypto.h"
#include "../../../src/engine/Discovery.h"
#include "../../../src/engine/Json.h"
#include "../../../src/engine/PhoneLink.h"
#include "../../../src/common/AppDataDir.h"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <map>
#include <set>
#include <string>
#include <thread>
#include <vector>

namespace
{
std::string Narrow(const std::wstring& w)
{
    std::string s;
    for (wchar_t c : w) s += static_cast<char>(c);
    return s;
}

phone::Endpoint MakeEndpoint(const char* host, const char* port)
{
    phone::Endpoint e;
    e.host = host;
    e.port = static_cast<uint16_t>(atoi(port));
    e.name = host;
    e.manual = true;
    return e;
}

std::string NalTypes(const crypto::Bytes& d)
{
    std::string out;
    for (size_t i = 0; i + 3 < d.size(); ++i)
    {
        size_t skip = 0;
        if (d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1) skip = 3;
        else if (i + 4 < d.size() && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 0 && d[i + 3] == 1) skip = 4;
        if (!skip) continue;
        if (!out.empty()) out += ",";
        out += std::to_string(d[i + skip] & 0x1f);
        i += skip - 1;
    }
    return out;
}

int Connect(const char* host, const char* port, const std::string& mode)
{
    phone::PhoneLink link;
    std::string error;
    const phone::Endpoint e = MakeEndpoint(host, port);
    bool ok;
    if (mode == "pair")
        ok = link.Connect(e, [](const std::string& code) { if (!code.empty()) printf("pairing code %s\n", code.c_str()); },
                          [] { return true; }, error);
    else if (mode == "auto") ok = link.ConnectAuto(e, error);
    else ok = link.ConnectKnown(e, error);
    if (!ok)
    {
        printf("connect failed: %s code=%s\n", error.c_str(), link.LastErrorCode().c_str());
        return 1;
    }
    printf("connect ok deviceId=%s name=%s\n", link.DeviceId().c_str(), link.DeviceName().c_str());
    return 0;
}

// A connect to a phone that never answers, cut short by Abort() from another thread after 500 ms.
int AbortTest(const char* host, const char* port)
{
    phone::PhoneLink link;
    const auto t0 = std::chrono::steady_clock::now();
    std::thread t([&] {
        std::string error;
        const bool ok = link.ConnectAuto(MakeEndpoint(host, port), error);
        const double s = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
        printf("connect %s after %.1f s: %s\n", ok ? "ok" : "failed", s, error.c_str());
    });
    std::this_thread::sleep_for(std::chrono::milliseconds(500));
    link.Abort();
    t.join();
    return 0;
}

int Stream(const char* host, const char* port, int frames, bool audio)
{
    phone::PhoneLink link;
    std::string error;
    if (!link.ConnectKnown(MakeEndpoint(host, port), error))
    {
        printf("connect failed: %s code=%s\n", error.c_str(), link.LastErrorCode().c_str());
        return 1;
    }
    json::Value caps;
    if (!link.RequestCaps(caps, error)) { printf("caps failed: %s\n", error.c_str()); return 1; }
    const json::Value& lens = caps["lenses"].Items()[0];
    const json::Value& mode = caps["modes"].Items()[0];
    std::string fps, codecs;
    for (const auto& f : mode["fps"].Items()) fps += (fps.empty() ? "" : ",") + std::to_string(f.Int());
    for (const auto& c : mode["codecs"].Items()) codecs += (codecs.empty() ? "" : ",") + c.Str();
    printf("caps lens=%s mode=%dx%d fps=%s codecs=%s audio=%d\n", lens["id"].Str().c_str(), mode["width"].Int(), mode["height"].Int(),
           fps.c_str(), codecs.c_str(), caps["audio"].IsObject() ? 1 : 0);

    json::Value start = json::Value::MakeObject();
    start.Set("type", "start").Set("lens", "cam0").Set("width", 1280).Set("height", 720).Set("fps", 30).Set("codec", "h264")
        .Set("bitrateKbps", 4000);
    if (audio) start.Set("audio", true);
    if (!link.SendControl(start)) { printf("start not sent\n"); return 1; }

    int video = 0, config = 0, audioRecords = 0, keyframes = 0, other = 0;
    size_t audioBytes = 0;
    std::string firstNals, started;
    std::set<std::string> controls;
    const auto begin = std::chrono::steady_clock::now();
    while (video < frames && std::chrono::steady_clock::now() - begin < std::chrono::seconds(30))
    {
        phone::Message m;
        bool timedOut = false;
        if (!link.Receive(m, 1000, timedOut))
        {
            if (timedOut) continue;
            printf("receive failed\n");
            return 1;
        }
        switch (m.kind)
        {
        case phone::Kind::Control:
        {
            const json::Value v = m.Json();
            controls.insert(v["type"].Str());
            if (v["type"].Str() == "started")
                started = "orientation=" + std::to_string(v["orientation"].Int()) + " fps=" + std::to_string(v["fps"].Int()) + " audio.on=" +
                          (v["audio"]["on"].Bool() ? "true" : "false");
            break;
        }
        case phone::Kind::VideoConfig: ++config; break;
        case phone::Kind::VideoFrame:
            if (video == 0) firstNals = NalTypes(m.payload);
            if (m.Keyframe()) ++keyframes;
            ++video;
            break;
        case phone::Kind::Audio: ++audioRecords; audioBytes += m.payload.size(); break;
        default: ++other; break;
        }
    }
    json::Value set = json::Value::MakeObject();
    set.Set("type", "set").Set("zoom", 2).Set("torch", 0.5);
    json::Value stats = json::Value::MakeObject();
    stats.Set("type", "stats").Set("recvFps", 30).Set("decodeMs", 2.5).Set("queueMs", 12).Set("lost", 0);
    const bool sent = link.SendControl(set) && link.SendControl(stats) &&
                      link.SendControl(json::Value::MakeObject().Set("type", "keyframe")) &&
                      link.SendControl(json::Value::MakeObject().Set("type", "stop"));
    // Read what is still on its way: closing with unread data makes Windows send a reset, which can
    // drop the phone's not yet read messages (stats...) and make the phone log racy.
    for (phone::Message m; ; )
    {
        bool timedOut = false;
        if (!link.Receive(m, 400, timedOut)) break;
    }
    printf("frames=%d keyframes>0=%d config=%d audio_records_gt0=%d other=%d first_nals=%s controls=", video, keyframes > 0, config,
           audioRecords > 0, other, firstNals.c_str());
    bool first = true;
    for (const auto& c : controls) { printf("%s%s", first ? "" : ",", c.c_str()); first = false; }
    printf("\nstarted %s\nsent set/stats/keyframe/stop=%d\n", started.c_str(), sent);
    if (audio) printf("audio_record_bytes_divisible_by_960=%d\n", audioBytes % 960 == 0);
    return 0;
}
} // namespace

int main(int argc, char** argv)
{
    const std::string cmd = argc > 1 ? argv[1] : "";
    if (cmd == "datadir") { printf("datadir=%s\n", Narrow(AppDataDir()).c_str()); return 0; }
    if (cmd == "clientid") { printf("clientid=%s\n", phone::ClientId().c_str()); return 0; }
    if (cmd == "clientname") { printf("clientname=%s\n", phone::ClientName().c_str()); return 0; }
    if (cmd == "keysave" && argc > 3) { phone::SavePairKey(argv[2], crypto::FromHex(argv[3])); printf("saved\n"); return 0; }
    if (cmd == "keyload" && argc > 2) { printf("key=%s\n", crypto::ToHex(phone::LoadPairKey(argv[2])).c_str()); return 0; }
    if (cmd == "keyhas" && argc > 2) { printf("has=%d\n", phone::HasPairKey(argv[2])); return 0; }
    if (cmd == "keyforget" && argc > 2) { phone::ForgetPairKey(argv[2]); printf("forgotten\n"); return 0; }
    if (cmd == "connect" && argc > 4) return Connect(argv[2], argv[3], argv[4]);
    if (cmd == "stream" && argc > 4) return Stream(argv[2], argv[3], atoi(argv[4]), argc > 5 && std::string(argv[5]) == "audio");
    if (cmd == "aborttest" && argc > 3) return AbortTest(argv[2], argv[3]);
    if (cmd == "known" && argc > 3)
    {
        phone::AddManual(argv[2], static_cast<uint16_t>(atoi(argv[3])));
        for (const auto& e : phone::KnownPhones()) printf("known host=%s port=%u name=%s manual=%d\n", e.host.c_str(), e.port, e.name.c_str(), e.manual);
        phone::RemoveManual(argv[2], static_cast<uint16_t>(atoi(argv[3])));
        printf("after remove: %zu\n", phone::KnownPhones().size());
        return 0;
    }
    fprintf(stderr, "usage: see header\n");
    return 2;
}
