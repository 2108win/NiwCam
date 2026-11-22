// C++ reference for the mDNS browse. The real StartDiscovery() also starts the USB watcher with the adb
// starter (it may run adb.exe when no adb server answers and a phone with USB debugging is plugged
// in), which must not happen on the user's PC. So this driver makes the very same DnsServiceBrowse /
// DnsServiceResolve calls and decodes the TXT properties exactly like Discovery.cpp (copied from it),
// and prints every phone found.
//
//   cpp_discovery browse <seconds>
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <windns.h>

#include <chrono>
#include <cstdio>
#include <map>
#include <mutex>
#include <string>
#include <thread>

#include "../../../src/engine/Discovery.h"

namespace
{
std::mutex g_lock;
std::map<std::wstring, phone::Endpoint> g_found;

std::string Narrow(const wchar_t* w)
{
    if (!w) return {};
    const int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, nullptr, 0, nullptr, nullptr);
    std::string s(n > 0 ? n - 1 : 0, '\0');
    if (n > 1) WideCharToMultiByte(CP_UTF8, 0, w, -1, s.data(), n, nullptr, nullptr);
    return s;
}

void CALLBACK OnResolved(DWORD status, PVOID context, PDNS_SERVICE_INSTANCE instance)
{
    std::wstring* key = static_cast<std::wstring*>(context);
    if (status == ERROR_SUCCESS && instance && instance->ip4Address)
    {
        phone::Endpoint e;
        IN_ADDR addr;
        addr.S_un.S_addr = *instance->ip4Address;
        char buf[INET_ADDRSTRLEN] = {};
        inet_ntop(AF_INET, &addr, buf, sizeof(buf));
        e.host = buf;
        e.port = instance->wPort;
        e.name = Narrow(instance->pszInstanceName);
        if (const size_t dot = e.name.find("._camstudio"); dot != std::string::npos) e.name.resize(dot);
        for (DWORD i = 0; i < instance->dwPropertyCount; ++i)
        {
            const std::wstring k = instance->keys[i] ? instance->keys[i] : L"";
            const std::string v = Narrow(instance->values[i]);
            if (k == L"id") e.deviceId = v;
            else if (k == L"name" && !v.empty()) e.name = v;
            else if (k == L"model") e.model = v;
            else if (k == L"os") e.os = v;
            else if (k == L"src") e.screen = v == "screen";
        }
        std::lock_guard lk(g_lock);
        g_found[*key] = e;
    }
    if (instance) DnsServiceFreeInstance(instance);
    delete key;
}

void CALLBACK OnBrowse(DWORD status, PVOID, PDNS_RECORD records)
{
    if (status == ERROR_SUCCESS)
    {
        for (PDNS_RECORD r = records; r; r = r->pNext)
        {
            if (r->wType != DNS_TYPE_PTR || !r->Data.PTR.pNameHost) continue;
            const std::wstring instance = r->Data.PTR.pNameHost;
            if (r->dwTtl == 0)
            {
                std::lock_guard lk(g_lock);
                g_found.erase(instance);
            }
            else
            {
                auto* key = new std::wstring(instance);
                DNS_SERVICE_RESOLVE_REQUEST req{};
                req.Version = DNS_QUERY_REQUEST_VERSION1;
                req.QueryName = const_cast<PWSTR>(key->c_str());
                req.pResolveCompletionCallback = OnResolved;
                req.pQueryContext = key;
                DNS_SERVICE_CANCEL cancel{};
                if (DnsServiceResolve(&req, &cancel) != DNS_REQUEST_PENDING) delete key;
            }
        }
    }
    if (records) DnsRecordListFree(records, DnsFreeRecordList);
}
} // namespace

int main(int argc, char** argv)
{
    const int seconds = argc > 2 ? atoi(argv[2]) : 5;
    DNS_SERVICE_BROWSE_REQUEST req{};
    req.Version = DNS_QUERY_REQUEST_VERSION1;
    req.QueryName = const_cast<PWSTR>(L"_camstudio._tcp.local");
    req.pBrowseCallback = OnBrowse;
    DNS_SERVICE_CANCEL cancel{};
    const DNS_STATUS st = DnsServiceBrowse(&req, &cancel);
    printf("DnsServiceBrowse -> %ld (%s)\n", static_cast<long>(st), st == DNS_REQUEST_PENDING ? "pending: browsing" : "failed");
    std::this_thread::sleep_for(std::chrono::seconds(seconds));
    if (st == DNS_REQUEST_PENDING) DnsServiceBrowseCancel(&cancel);
    std::lock_guard lk(g_lock);
    printf("found %zu phone(s)\n", g_found.size());
    for (const auto& [_, e] : g_found)
        printf("  %s:%u name=\"%s\" id=%s model=%s os=%s screen=%d\n", e.host.c_str(), e.port, e.name.c_str(), e.deviceId.c_str(),
               e.model.c_str(), e.os.c_str(), e.screen);
    return 0;
}
