lito.install({
    artifacts = {
        {
            target = { kind = "bin", name = "kwe-web-renderer" },
            destination = "lib/kde-wallpaper-engine/weweb/kwe-web-renderer",
            runtime_search = {
                {
                    external_asset = {
                        dependency = "cef",
                        set = "runtime",
                    },
                },
            },
        },
    },
    external_assets = {
        {
            dependency = "cef",
            set = "runtime",
            destination = "lib/kde-wallpaper-engine/weweb",
            strip = {
                mode = "symbols",
                files = {
                    "chrome-sandbox",
                    "libEGL.so",
                    "libGLESv2.so",
                    "libcef.so",
                    "libvk_swiftshader.so",
                    "libvulkan.so.1",
                },
            },
        },
    },
})
