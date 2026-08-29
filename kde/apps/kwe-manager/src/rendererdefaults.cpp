// SPDX-License-Identifier: GPL-3.0-or-later
#include "rendererdefaults.h"

#include <QSettings>
#include <QStringList>

namespace {
constexpr int DefaultFps = 30;
// Mirrors the daemon's wallpaper.apply bounds (supervisor spec: 1..=240).
constexpr int MinFps = 1;
constexpr int MaxFps = 240;
const QString DefaultScaling = QStringLiteral("aspect");

bool isRendererKind(const QString &kind) {
    return kind == QStringLiteral("scene") || kind == QStringLiteral("video")
        || kind == QStringLiteral("web");
}

bool isScaling(const QString &value) {
    return value == QStringLiteral("aspect") || value == QStringLiteral("fill")
        || value == QStringLiteral("stretch");
}

QString key(const QString &kind, const char *field) {
    return QStringLiteral("renderer-defaults/%1/%2").arg(kind, QLatin1String(field));
}
}

RendererDefaults::RendererDefaults(QObject *parent) : QObject(parent) {}

int RendererDefaults::fps(const QString &kind) const {
    if (!isRendererKind(kind))
        return DefaultFps;
    const int value = QSettings().value(key(kind, "fps"), DefaultFps).toInt();
    return (value >= MinFps && value <= MaxFps) ? value : DefaultFps;
}

QString RendererDefaults::scaling(const QString &kind) const {
    if (!isRendererKind(kind))
        return DefaultScaling;
    const auto value = QSettings().value(key(kind, "scaling"), DefaultScaling).toString();
    return isScaling(value) ? value : DefaultScaling;
}

void RendererDefaults::setFps(const QString &kind, int value) {
    if (!isRendererKind(kind) || value < MinFps || value > MaxFps)
        return;
    QSettings().setValue(key(kind, "fps"), value);
    emit changed();
}

void RendererDefaults::setScaling(const QString &kind, const QString &value) {
    if (!isRendererKind(kind) || !isScaling(value))
        return;
    QSettings().setValue(key(kind, "scaling"), value);
    emit changed();
}
