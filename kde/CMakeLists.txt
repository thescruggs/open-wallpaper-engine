# SPDX-License-Identifier: GPL-3.0-or-later
cmake_minimum_required(VERSION 3.28)
project(KDEWallpaperEngine VERSION 0.1.0 LANGUAGES CXX)

include(CTest)
include(GNUInstallDirs)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_AUTOMOC ON)

option(KWE_BUILD_MANAGER "Build the Kirigami manager" ON)

if(KWE_BUILD_MANAGER)
    find_package(Qt6 6.6 REQUIRED COMPONENTS Core DBus Gui Network Qml Quick QuickControls2)
    if(BUILD_TESTING)
        find_package(Qt6 6.6 REQUIRED COMPONENTS Test)
    endif()
    find_package(KF6Kirigami REQUIRED)
    qt_standard_project_setup(REQUIRES 6.6)
    qt_policy(SET QTP0004 NEW)
    add_subdirectory(modules/org/kde/kwe/display)
    add_subdirectory(apps/kwe-frame-preview)
    add_subdirectory(apps/kwe-manager)
    add_subdirectory(apps/kwe-occlusion-worker)
    # F4: optional, because it is the only target that needs libkscreen's
    # DPMS library. Without it the daemon finds no helper beside itself and
    # the pause-when-display-off policy simply never pauses.
    find_package(KF6Screen QUIET)
    if(TARGET KF6::ScreenDpms)
        add_subdirectory(apps/kwe-display-power-worker)
    else()
        message(STATUS "KF6::ScreenDpms not found: kwe-display-power-worker will not be built")
    endif()
    add_subdirectory(plasma/wallpapers/org.kde.kwe.wallpaper)
endif()
