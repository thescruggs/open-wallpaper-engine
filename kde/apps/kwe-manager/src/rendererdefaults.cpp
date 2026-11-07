// SPDX-License-Identifier: GPL-3.0-or-later
#include "rendererdefaults.h"

#include "settingsclient.h"

#include <QSettings>

namespace {
// Where releases before the daemon-owned record kept the values.
const QString LegacyGroup = QStringLiteral("renderer-defaults");

QString legacyKey(const QString &kind, const char *field) {
    return QStringLiteral("%1/%2/%3").arg(LegacyGroup, kind, QLatin1String(field));
}
}

RendererDefaults::RendererDefaults(SettingsClient *client, QObject *parent)
    : QObject(parent), m_client(client) {
    connect(m_client, &SettingsClient::settingsChanged, this, [this] {
        ++m_revision;
        emit changed();
        if (m_client->loaded())
            migrateLegacySettings();
    });
}

int RendererDefaults::fps(const QString &kind) const {
    return m_client->rendererFps(kind);
}

QString RendererDefaults::scaling(const QString &kind) const {
    return m_client->rendererScaling(kind);
}

void RendererDefaults::setFps(const QString &kind, int value) {
    m_client->setRendererFps(kind, value);
}

void RendererDefaults::setScaling(const QString &kind, const QString &value) {
    m_client->setRendererScaling(kind, value);
}

void RendererDefaults::migrateLegacySettings() {
    if (m_migrated)
        return;
    m_migrated = true;
    QSettings settings;
    if (!settings.childGroups().contains(LegacyGroup))
        return;
    // Only a daemon that still holds the built-ins takes the legacy values:
    // a record the user already edited (or another client migrated) wins.
    if (m_client->rendererDefaultsAreBuiltIn()) {
        for (const auto &kind : {QStringLiteral("scene"), QStringLiteral("video"), QStringLiteral("web")}) {
            const auto fps = settings.value(legacyKey(kind, "fps"));
            if (fps.isValid())
                m_client->setRendererFps(kind, fps.toInt());
            const auto scaling = settings.value(legacyKey(kind, "scaling")).toString();
            if (!scaling.isEmpty())
                m_client->setRendererScaling(kind, scaling);
        }
    }
    settings.remove(LegacyGroup);
    settings.sync();
}
