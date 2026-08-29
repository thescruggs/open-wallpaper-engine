export module weweb:browser_host;

import rstd.cppstd;
import wescene.json;

import :frame;
import :manifest;

export namespace weweb
{

class OsrRenderHandler;
class ClientHandler;

class BrowserHost {
public:
    struct InitOptions {
        std::filesystem::path resources_dir;
        std::filesystem::path locales_dir;
        std::filesystem::path cache_dir;
        bool                  enable_remote_debugging { false };
        int                   remote_debugging_port { 0 };
        bool                  enable_audio { true };
        bool                  shared_texture_enabled { true };
        // Launch children directly instead of through the zygote. Needed
        // when the CEF resources do not sit beside the executable — the
        // zygote is spawned without the resources-dir switches and then
        // cannot load ICU.
        bool                  no_zygote { false };
        std::string           render_node_override;
    };

    struct OpenOptions {
        bool shared_texture_enabled { true };
        int  frame_rate { 60 };
    };

    BrowserHost();
    ~BrowserHost();

    BrowserHost(const BrowserHost&)            = delete;
    BrowserHost& operator=(const BrowserHost&) = delete;

    int  RunOrExitIfHelper(int argc, char** argv);
    bool Init(const InitOptions& opts);

    void SetAcceleratedPaintCallback(AcceleratedPaintCallback cb);
    void SetCpuPaintCallback(CpuPaintCallback cb);
    void SetAudioResponseDemandCallback(std::function<void(bool)> cb);

    bool OpenWallpaper(const WebManifest& manifest, const std::filesystem::path& workshop_dir,
                       int width, int height);
    bool OpenWallpaper(const WebManifest& manifest, const std::filesystem::path& workshop_dir,
                       int width, int height, OpenOptions opts);

    void OnResize(int width, int height);
    void Invalidate();

    void OnMouseMove(int x, int y, bool left_down);
    void OnMouseButton(int x, int y, int cef_button, bool down, int click_count);
    void OnMouseWheel(int x, int y, int delta_x, int delta_y);
    void OnKey(int cef_key_event_type, int native_key_code, int windows_key_code, int modifiers,
               unsigned int unicode_char);
    void OnFocus(bool gained);

    void Pump();

    void ApplyVolume(float volume);
    void SetFrameRate(int fps);
    void SetPaused(bool paused);
    void ApplyUserProperty(std::string_view key, const owe::Json& value);
    void PushAudioData(const float* data, std::size_t count);

    bool ShouldExit() const;
    void RequestClose();
    void Shutdown();

    // The bundled CEF's version string, e.g. "139.0.4+g2f1bfd8+chromium-139.0...".
    static std::string CefVersionString();

    // Locates the CEF resource/locales directories by the runtime path of
    // the loaded libcef.so (dladdr): the standard bundle layout keeps
    // icudtl.dat and the .pak files beside libcef.so or in a sibling
    // Resources/ directory. Returns false when neither is found; hosts use
    // this as a fallback when their own install layout has no staged copy.
    static bool LocateCefResources(std::filesystem::path& resources_dir,
                                   std::filesystem::path& locales_dir);

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace weweb
