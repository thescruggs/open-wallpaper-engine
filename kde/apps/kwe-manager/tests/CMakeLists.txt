# SPDX-License-Identifier: GPL-3.0-or-later
qt_add_executable(kwe-package-installer-test
    packageinstallertest.cpp
    ../src/packageinstaller.cpp
    ../src/packageinstaller.h
)
target_link_libraries(kwe-package-installer-test PRIVATE Qt6::Core Qt6::Qml Qt6::Test)
add_test(NAME kwe-package-installer-test COMMAND kwe-package-installer-test)

qt_add_executable(kwe-daemon-activator-test
    daemonactivatortest.cpp
    ../src/daemonactivator.cpp
    ../src/daemonactivator.h
)
target_link_libraries(kwe-daemon-activator-test PRIVATE Qt6::Core Qt6::Qml Qt6::Network Qt6::Test)
add_test(NAME kwe-daemon-activator-test COMMAND kwe-daemon-activator-test)

qt_add_executable(kwe-playlist-controller-test
    playlistcontrollertest.cpp
    ../src/playlistcontroller.cpp
    ../src/playlistcontroller.h
    ../src/playlistclient.cpp
    ../src/playlistclient.h
)
target_link_libraries(kwe-playlist-controller-test PRIVATE Qt6::Core Qt6::Network Qt6::Test)
add_test(NAME kwe-playlist-controller-test COMMAND kwe-playlist-controller-test)

qt_add_executable(kwe-settings-client-test
    settingsclienttest.cpp
    ../src/settingsclient.cpp
    ../src/settingsclient.h
)
target_link_libraries(kwe-settings-client-test PRIVATE Qt6::Core Qt6::Network Qt6::Test)
add_test(NAME kwe-settings-client-test COMMAND kwe-settings-client-test)

qt_add_executable(kwe-renderer-defaults-test
    rendererdefaultstest.cpp
    ../src/rendererdefaults.cpp
    ../src/rendererdefaults.h
)
target_link_libraries(kwe-renderer-defaults-test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME kwe-renderer-defaults-test COMMAND kwe-renderer-defaults-test)

qt_add_executable(kwe-permissions-client-test
    permissionsclienttest.cpp
    ../src/permissionsclient.cpp
    ../src/permissionsclient.h
)
target_link_libraries(kwe-permissions-client-test PRIVATE Qt6::Core Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-permissions-client-test COMMAND kwe-permissions-client-test)

qt_add_executable(kwe-apply-client-test
    applyclienttest.cpp
    ../src/applyclient.cpp
    ../src/applyclient.h
)
target_link_libraries(kwe-apply-client-test PRIVATE Qt6::Core Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-apply-client-test COMMAND kwe-apply-client-test)

qt_add_executable(kwe-renderer-status-test
    rendererstatustest.cpp
    ../src/rendererstatus.cpp
    ../src/rendererstatus.h
)
target_link_libraries(kwe-renderer-status-test PRIVATE Qt6::Core Qt6::Network Qt6::Test)
add_test(NAME kwe-renderer-status-test COMMAND kwe-renderer-status-test)

qt_add_executable(kwe-web-preview-test
    webpreviewtest.cpp
    ../src/webpreview.cpp
    ../src/webpreview.h
    ../src/permissionsclient.cpp
    ../src/permissionsclient.h
)
target_link_libraries(kwe-web-preview-test PRIVATE Qt6::Core Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-web-preview-test COMMAND kwe-web-preview-test)

qt_add_executable(kwe-issue-reporter-test
    issuereportertest.cpp
    ../src/issuereporter.cpp
    ../src/issuereporter.h
)
target_link_libraries(kwe-issue-reporter-test PRIVATE Qt6::Core Qt6::Gui Qt6::Test)
add_test(NAME kwe-issue-reporter-test COMMAND kwe-issue-reporter-test)

# Workshop browsing: the daemon client and its sign-in state machine, the
# Browse list model, and the sign-in QR matrix, against a fake QLocalServer.
qt_add_executable(kwe-workshop-client-test
    workshopclienttest.cpp
    fakeworkshopdaemon.h
    ../src/workshopclient.cpp
    ../src/workshopclient.h
)
target_link_libraries(kwe-workshop-client-test PRIVATE Qt6::Core Qt6::Gui Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-workshop-client-test COMMAND kwe-workshop-client-test)

qt_add_executable(kwe-remote-catalog-model-test
    remotecatalogmodeltest.cpp
    fakeworkshopdaemon.h
    ../src/remotecatalogmodel.cpp
    ../src/remotecatalogmodel.h
    ../src/workshopclient.cpp
    ../src/workshopclient.h
    ../src/catalogmodel.cpp
    ../src/catalogmodel.h
)
target_link_libraries(kwe-remote-catalog-model-test PRIVATE Qt6::Core Qt6::Gui Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-remote-catalog-model-test COMMAND kwe-remote-catalog-model-test)

qt_add_executable(kwe-qr-code-item-test
    qrcodeitemtest.cpp
    ../src/qrcodeitem.cpp
    ../src/qrcodeitem.h
)
target_link_libraries(kwe-qr-code-item-test PRIVATE Qt6::Core Qt6::Gui Qt6::Quick Qt6::Qml Qt6::Test PkgConfig::QRENCODE)
add_test(NAME kwe-qr-code-item-test COMMAND kwe-qr-code-item-test)

qt_add_executable(kwe-wallpaper-filter-model-test
    wallpaperfiltermodeltest.cpp
    ../src/catalogmodel.cpp
    ../src/catalogmodel.h
)
target_link_libraries(kwe-wallpaper-filter-model-test PRIVATE Qt6::Core Qt6::Gui Qt6::Test)
add_test(NAME kwe-wallpaper-filter-model-test COMMAND kwe-wallpaper-filter-model-test)
